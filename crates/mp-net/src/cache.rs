//! Answers kept on disk, so the same question is not asked twice.
//!
//! Two reasons, and the second is the important one. Refetching on every
//! launch is slow; it is also rude to a free service run by volunteers, and
//! the fastest way to get an application blocked.
//!
//! ## Misses are cached too
//!
//! A library of a few thousand tracks contains a great many the service has
//! never heard of. Without a negative cache, every one of those is a fresh
//! request on every launch, forever — which is the bulk of the traffic and
//! all of the waste.
//!
//! So a miss is recorded as an [`Entry`] with nothing in it, and expires after
//! [`NEGATIVE_TTL`]. A hit does not expire at all: the words to a song do not
//! change, and the user can clear the cache if they ever need to.
//!
//! ## The files are readable
//!
//! One small JSON file per answer, laid out like the artwork cache — fanned
//! out by the first two characters of the key so no directory ends up with ten
//! thousand entries in it. Anyone wondering what Resonance has stored about
//! their music can go and look.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::timestamp;

/// How long a recorded miss stands before it is worth asking again.
///
/// LRCLIB is contributed to by its users, so a track with no lyrics today may
/// well have them next month. Two weeks trades a little traffic for that.
pub const NEGATIVE_TTL: Duration = Duration::from_secs(14 * 24 * 60 * 60);

/// One cached answer.
///
/// `found` being `None` is the recorded miss, and is the whole reason this is
/// a struct rather than just the payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct Entry<T> {
    /// When the service was asked, in unix seconds.
    pub fetched_at: i64,
    /// What it said, or nothing if it had no answer.
    #[serde(default = "none", skip_serializing_if = "Option::is_none")]
    pub found: Option<T>,
}

fn none<T>() -> Option<T> {
    None
}

impl<T> Entry<T> {
    /// An answer, recorded now.
    pub fn found(value: T) -> Self {
        Self {
            fetched_at: timestamp::now_unix(),
            found: Some(value),
        }
    }

    /// A miss, recorded now.
    pub fn missing() -> Self {
        Self {
            fetched_at: timestamp::now_unix(),
            found: None,
        }
    }

    /// Whether this may still be used as of `now`.
    ///
    /// An answer never goes stale. A miss does, because the service gains
    /// entries over time and a permanent "no" would make the feature look
    /// broken for anyone who waited.
    pub fn is_fresh_at(&self, now: i64) -> bool {
        if self.found.is_some() {
            return true;
        }

        let age = now.saturating_sub(self.fetched_at);

        // A negative age means the clock moved backwards between the write and
        // this read. Treating that as fresh could pin a miss for years, so it
        // counts as stale and costs one extra request.
        (0..NEGATIVE_TTL.as_secs() as i64).contains(&age)
    }
}

/// A directory of cached answers.
#[derive(Debug, Clone)]
pub struct Cache {
    root: PathBuf,
}

impl Cache {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where one key is stored.
    pub fn path(&self, key: &str) -> PathBuf {
        self.root
            .join(&key[..2.min(key.len())])
            .join(format!("{key}.json"))
    }

    /// Read a cached answer, if there is a usable one.
    ///
    /// A file that cannot be read or parsed counts as absent. The cost of
    /// being wrong is one extra request, which is not worth an error path.
    pub fn read<T: DeserializeOwned>(&self, key: &str) -> Option<Entry<T>> {
        let text = std::fs::read_to_string(self.path(key)).ok()?;
        let entry: Entry<T> = serde_json::from_str(&text).ok()?;

        entry.is_fresh_at(timestamp::now_unix()).then_some(entry)
    }

    /// Store an answer.
    pub fn write<T: Serialize>(&self, key: &str, entry: &Entry<T>) -> Result<()> {
        let path = self.path(key);

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }

        let text = serde_json::to_string_pretty(entry).context("encoding a cache entry")?;
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;

        Ok(())
    }

    /// Delete everything. For a "clear the cache" button.
    ///
    /// A missing directory is a success: the postcondition is that nothing is
    /// cached, and that already holds.
    pub fn clear(&self) -> Result<()> {
        match std::fs::remove_dir_all(&self.root) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => {
                Err(anyhow::Error::from(err).context(format!("clearing {}", self.root.display())))
            }
        }
    }

    /// How many answers are stored, and how much room they take.
    pub fn size(&self) -> (usize, u64) {
        let mut files = 0;
        let mut bytes = 0;

        let Ok(fanout) = std::fs::read_dir(&self.root) else {
            return (0, 0);
        };

        for bucket in fanout.flatten() {
            let Ok(entries) = std::fs::read_dir(bucket.path()) else {
                continue;
            };

            for entry in entries.flatten() {
                if let Ok(meta) = entry.metadata()
                    && meta.is_file()
                {
                    files += 1;
                    bytes += meta.len();
                }
            }
        }

        (files, bytes)
    }
}

/// A stable key for a set of query parts.
///
/// FNV-1a over the parts, lowercased and separated, exactly as the artwork
/// cache keys covers by the hash of their bytes. Deliberately not
/// `DefaultHasher`, whose output is explicitly allowed to change between Rust
/// releases — that would silently empty the cache on a toolchain upgrade.
pub fn key(parts: &[&str]) -> String {
    const OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;

    let mut hash = OFFSET;

    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            // A separator, so ("ab", "c") and ("a", "bc") are different keys.
            hash ^= u128::from(b'\x1f');
            hash = hash.wrapping_mul(PRIME);
        }

        for byte in part.to_lowercase().bytes() {
            hash ^= u128::from(byte);
            hash = hash.wrapping_mul(PRIME);
        }
    }

    format!("{hash:032x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    type Text = Entry<String>;

    #[test]
    fn an_answer_survives_a_round_trip() {
        let dir = tempfile::tempdir().expect("temp dir");
        let cache = Cache::new(dir.path());
        let key = key(&["Radiohead", "Creep"]);

        cache
            .write(&key, &Entry::found("the words".to_owned()))
            .expect("write");

        let read: Text = cache.read(&key).expect("read back");
        assert_eq!(read.found.as_deref(), Some("the words"));
    }

    #[test]
    fn an_unknown_key_is_simply_absent() {
        let dir = tempfile::tempdir().expect("temp dir");
        let cache = Cache::new(dir.path());

        assert!(cache.read::<String>(&key(&["nothing"])).is_none());
    }

    /// The reason the cache exists at all: without this, every track the
    /// service has never heard of is a fresh request on every launch.
    #[test]
    fn a_miss_is_remembered() {
        let dir = tempfile::tempdir().expect("temp dir");
        let cache = Cache::new(dir.path());
        let key = key(&["Nobody", "Nothing"]);

        cache.write(&key, &Text::missing()).expect("write");

        let read: Text = cache.read(&key).expect("the miss should be recorded");
        assert!(read.found.is_none());
    }

    #[test]
    fn an_answer_never_goes_stale() {
        let ancient = Entry {
            fetched_at: 0,
            found: Some("the words".to_owned()),
        };

        assert!(ancient.is_fresh_at(4_000_000_000));
    }

    /// A permanent "no" would make the feature look broken to anyone who
    /// waited for the service to gain the track.
    #[test]
    fn a_miss_expires() {
        let recorded_at = 1_788_480_000;
        let miss = Text {
            fetched_at: recorded_at,
            found: None,
        };

        assert!(miss.is_fresh_at(recorded_at), "just recorded");
        assert!(
            miss.is_fresh_at(recorded_at + NEGATIVE_TTL.as_secs() as i64 - 1),
            "still inside the window"
        );
        assert!(
            !miss.is_fresh_at(recorded_at + NEGATIVE_TTL.as_secs() as i64),
            "the window has passed and it is worth asking again"
        );
    }

    /// A clock that jumped backwards must not pin a miss for years.
    #[test]
    fn a_miss_from_the_future_is_not_trusted() {
        let miss = Text {
            fetched_at: 2_000_000_000,
            found: None,
        };

        assert!(!miss.is_fresh_at(1_000_000_000));
    }

    #[test]
    fn a_stale_miss_reads_as_absent() {
        let dir = tempfile::tempdir().expect("temp dir");
        let cache = Cache::new(dir.path());
        let key = key(&["Long", "Ago"]);

        cache
            .write(
                &key,
                &Text {
                    fetched_at: 1,
                    found: None,
                },
            )
            .expect("write");

        assert!(
            cache.read::<String>(&key).is_none(),
            "an expired miss should not be served"
        );
    }

    #[test]
    fn a_corrupt_file_is_treated_as_absent() {
        let dir = tempfile::tempdir().expect("temp dir");
        let cache = Cache::new(dir.path());
        let key = key(&["Broken"]);

        let path = cache.path(&key);
        std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir");
        std::fs::write(&path, "{ this is not json").expect("write");

        assert!(cache.read::<String>(&key).is_none());
    }

    #[test]
    fn keys_are_stable_and_case_insensitive() {
        assert_eq!(key(&["Radiohead", "Creep"]), key(&["radiohead", "creep"]));
        assert_eq!(key(&["Radiohead"]).len(), 32);
    }

    #[test]
    fn different_queries_get_different_keys() {
        assert_ne!(key(&["Radiohead", "Creep"]), key(&["Radiohead", "Bones"]));
        assert_ne!(key(&["a", "b"]), key(&["b", "a"]));
    }

    /// Without a separator, ("ab", "c") and ("a", "bc") would collide and one
    /// track would be served another's words.
    #[test]
    fn the_separator_keeps_the_parts_apart() {
        assert_ne!(key(&["ab", "c"]), key(&["a", "bc"]));
    }

    #[test]
    fn clearing_removes_everything() {
        let dir = tempfile::tempdir().expect("temp dir");
        let cache = Cache::new(dir.path().join("lyrics"));

        for name in ["one", "two", "three"] {
            cache
                .write(&key(&[name]), &Entry::found(name.to_owned()))
                .expect("write");
        }
        assert_eq!(cache.size().0, 3);

        cache.clear().expect("clear");

        assert_eq!(cache.size(), (0, 0));
        assert!(cache.read::<String>(&key(&["one"])).is_none());
    }

    /// The button should work twice without reporting a failure the second
    /// time, and on a fresh install where nothing has been cached at all.
    #[test]
    fn clearing_an_empty_cache_is_not_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let cache = Cache::new(dir.path().join("never-used"));

        cache.clear().expect("clearing nothing should succeed");
        cache.clear().expect("and should still succeed");
    }

    #[test]
    fn size_reports_what_is_stored() {
        let dir = tempfile::tempdir().expect("temp dir");
        let cache = Cache::new(dir.path());

        assert_eq!(cache.size(), (0, 0), "nothing cached yet");

        cache
            .write(&key(&["one"]), &Entry::found("words".to_owned()))
            .expect("write");

        let (files, bytes) = cache.size();
        assert_eq!(files, 1);
        assert!(bytes > 0);
    }
}
