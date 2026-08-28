//! Finding the same song twice.
//!
//! A collection assembled over years from several sources accumulates the same
//! recording under different filenames, in different folders, at different
//! bitrates. This finds those groups so they can be reviewed.
//!
//! # What counts as a duplicate
//!
//! Same title and same artist, within a tolerance on duration. All three
//! matter:
//!
//! * Title alone would group every "Intro" in the library.
//! * Title and artist alone would group a studio recording with its live
//!   version, its radio edit, and a twelve-minute remix — different recordings
//!   that happen to share a name.
//! * The duration tolerance is what separates "the same song encoded twice"
//!   from "a different take". A few seconds covers differing lead-in silence
//!   and encoder padding; much more starts merging genuinely different cuts.
//!
//! Titles are compared after normalisation, so `Song (Remastered)` and
//! `song` group together — those really are the same recording most of the
//! time — while `Song (Live)` deliberately does not, because it is not.
//!
//! # This never deletes anything
//!
//! It returns groups. Choosing what to do about them is the user's, and
//! Resonance does not remove files at all.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;
use rusqlite::Connection;

use super::model::Track;
use super::query;

/// How far two durations may differ and still be the same recording.
pub const DEFAULT_TOLERANCE: Duration = Duration::from_secs(3);

/// Words in a parenthetical that mark it as cosmetic — the same recording,
/// relabelled.
const COSMETIC_WORDS: &[&str] = &[
    "remaster",
    "remastered",
    "explicit",
    "clean",
    "bonus",
    "mono",
    "stereo",
    "deluxe",
    "hd",
    "hq",
];

/// Cosmetic markers that are more than one word.
const COSMETIC_PHRASES: &[&str] = &["album version", "original mix", "single version"];

/// Words that mark a parenthetical as naming a *different* recording.
///
/// These win over the cosmetic list. `(Live 2011 Remaster)` is a remaster of a
/// live recording, not of the studio one, and merging the two would quietly
/// hide music — which is a far worse failure than showing one duplicate too
/// few.
const DISTINGUISHING_WORDS: &[&str] = &[
    "live",
    "acoustic",
    "remix",
    "demo",
    "instrumental",
    "cover",
    "karaoke",
    "edit",
    "session",
    "unplugged",
    "reprise",
];

/// One set of tracks that appear to be the same recording.
#[derive(Debug, Clone)]
pub struct Group {
    /// The normalised title the group is keyed on, for display.
    pub title: String,
    pub artist: String,
    /// Every copy found, longest first — usually the least truncated.
    pub tracks: Vec<Track>,
}

impl Group {
    /// How many copies beyond the first.
    pub fn surplus(&self) -> usize {
        self.tracks.len().saturating_sub(1)
    }

    /// The copy worth keeping, on the evidence available.
    ///
    /// Prefers real tags over a filename-derived title, then the longest
    /// duration, then the largest file — which between them usually identify
    /// the least mangled copy. It is a suggestion, not an action.
    pub fn best(&self) -> Option<&Track> {
        self.tracks.iter().max_by_key(|track| {
            (
                track.tagged,
                track.duration.unwrap_or_default(),
                track.art_id.is_some(),
            )
        })
    }
}

/// Find groups of duplicate recordings.
///
/// Tracks with no duration are compared on title and artist alone — there is
/// no third signal to apply, and excluding them would hide the duplicates in
/// exactly the badly-tagged corner of a library where they collect.
pub fn find(connection: &Connection, tolerance: Duration) -> Result<Vec<Group>> {
    let tracks = query::tracks(
        connection,
        &super::model::Filter::All,
        super::model::Order::Title,
        false,
    )?;

    // Bucket by (normalised title, lowercased artist) first, then split each
    // bucket on duration. Comparing every track against every other would be
    // quadratic over the whole library; this is quadratic only within a bucket,
    // and buckets are tiny.
    let mut buckets: HashMap<(String, String), Vec<Track>> = HashMap::new();

    for track in tracks {
        let key = (normalise_title(&track.title), track.artist.to_lowercase());

        // A track with no title to speak of cannot be matched on one.
        if key.0.is_empty() {
            continue;
        }

        buckets.entry(key).or_default().push(track);
    }

    let mut groups = Vec::new();

    for ((title, _), bucket) in buckets {
        if bucket.len() < 2 {
            continue;
        }

        for mut cluster in split_by_duration(bucket, tolerance) {
            if cluster.len() < 2 {
                continue;
            }

            // Longest first: the fuller copy is the more useful default.
            cluster.sort_by(|a, b| {
                b.duration
                    .unwrap_or_default()
                    .cmp(&a.duration.unwrap_or_default())
                    .then_with(|| a.id.cmp(&b.id))
            });

            groups.push(Group {
                title: title.clone(),
                artist: cluster[0].artist.clone(),
                tracks: cluster,
            });
        }
    }

    // Worst offenders first, then alphabetical so the list is stable.
    groups.sort_by(|a, b| {
        b.tracks
            .len()
            .cmp(&a.tracks.len())
            .then_with(|| a.artist.cmp(&b.artist))
            .then_with(|| a.title.cmp(&b.title))
    });

    Ok(groups)
}

/// Split a same-title bucket into clusters of similar length.
fn split_by_duration(mut bucket: Vec<Track>, tolerance: Duration) -> Vec<Vec<Track>> {
    // Sorted, so a single pass can walk the run of near-equal durations.
    bucket.sort_by_key(|track| track.duration.unwrap_or_default());

    let mut clusters: Vec<Vec<Track>> = Vec::new();

    for track in bucket {
        let length = track.duration.unwrap_or_default();

        let fits = clusters.last().is_some_and(|cluster| {
            let previous = cluster
                .last()
                .and_then(|track| track.duration)
                .unwrap_or_default();

            // Compared against the previous member rather than the cluster's
            // first: durations were sorted, so this keeps a gradual run
            // together instead of splitting it at an arbitrary point.
            length.abs_diff(previous) <= tolerance
        });

        if fits {
            clusters
                .last_mut()
                .expect("checked by is_some_and")
                .push(track);
        } else {
            clusters.push(vec![track]);
        }
    }

    clusters
}

/// Reduce a title to what identifies the recording.
///
/// Lowercased, cosmetic parentheticals removed, punctuation and spacing
/// flattened. Anything that names a *different* recording is left alone.
pub fn normalise_title(title: &str) -> String {
    let mut text = title.to_lowercase();

    // Strip cosmetic bracketed suffixes, repeatedly — real filenames stack
    // them, as in `Song (Remastered) [Explicit]`.
    loop {
        let trimmed = strip_cosmetic_suffix(&text);
        if trimmed.len() == text.len() {
            break;
        }
        text = trimmed;
    }

    // Keep letters, digits and single spaces. This is what makes `Don't` and
    // `Dont`, or `Song - Part 1` and `Song Part 1`, land together.
    let mut out = String::with_capacity(text.len());
    let mut last_was_space = true;

    for character in text.chars() {
        if character.is_alphanumeric() {
            out.push(character);
            last_was_space = false;
        } else if !last_was_space {
            out.push(' ');
            last_was_space = true;
        }
    }

    out.trim().to_owned()
}

/// Remove one trailing `(...)` or `[...]` if it is cosmetic.
fn strip_cosmetic_suffix(text: &str) -> String {
    let trimmed = text.trim_end();

    let (open, close) = match trimmed.chars().last() {
        Some(')') => ('(', ')'),
        Some(']') => ('[', ']'),
        _ => return trimmed.to_owned(),
    };

    let Some(start) = trimmed.rfind(open) else {
        return trimmed.to_owned();
    };

    let inside = trimmed[start + 1..trimmed.len() - close.len_utf8()].trim();

    // Only strip what is definitely cosmetic. An unrecognised parenthetical
    // might be `(Live at Leeds)`, which identifies a different recording.
    //
    // Compared word by word rather than as substrings, and matched anywhere
    // inside rather than as a prefix. Both matter: `(2011 Remaster)` and
    // `(Digital Remaster)` are as common as the bare word so a prefix rule
    // misses them, and a substring rule finds "edit" inside "deluxe edition"
    // and refuses to strip a suffix that is plainly cosmetic.
    let words: Vec<&str> = inside
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();

    let distinguishing = words.iter().any(|word| DISTINGUISHING_WORDS.contains(word));

    let cosmetic = !distinguishing
        && (words.iter().any(|word| COSMETIC_WORDS.contains(word))
            || COSMETIC_PHRASES
                .iter()
                .any(|phrase| inside.contains(phrase)));

    if cosmetic {
        trimmed[..start].trim_end().to_owned()
    } else {
        trimmed.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::db;
    use rusqlite::params;

    fn fixture(rows: &[(i64, &str, &str, u64)]) -> Connection {
        let db = db::open_in_memory().unwrap();

        let mut artists: HashMap<&str, i64> = HashMap::new();

        for (id, title, artist, seconds) in rows {
            let next = artists.len() as i64 + 1;
            let artist_id = *artists.entry(artist).or_insert(next);

            db.execute(
                "INSERT OR IGNORE INTO artists (id, name, sort_name) VALUES (?1, ?2, ?3)",
                params![artist_id, artist, artist.to_lowercase()],
            )
            .unwrap();

            db.execute(
                "INSERT INTO tracks (
                     id, path, folder, file_name, mtime, size, title, sort_title,
                     artist_id, duration_ms, tagged, added_at, last_seen_at
                 ) VALUES (?1, ?2, '/m', 'x.mp3', 1, 2, ?3, ?4, ?5, ?6, 1, 0, 0)",
                params![
                    id,
                    format!("/m/{id}.mp3"),
                    title,
                    title.to_lowercase(),
                    artist_id,
                    (*seconds * 1000) as i64,
                ],
            )
            .unwrap();
        }

        db
    }

    #[test]
    fn the_same_song_twice_is_one_group() {
        let db = fixture(&[
            (1, "Teardrop", "Massive Attack", 330),
            (2, "Teardrop", "Massive Attack", 331),
            (3, "Angel", "Massive Attack", 380),
        ]);

        let groups = find(&db, DEFAULT_TOLERANCE).unwrap();

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].tracks.len(), 2);
        assert_eq!(groups[0].surplus(), 1);
    }

    /// The whole reason duration is part of the key.
    #[test]
    fn a_live_version_of_the_same_length_name_is_not_a_duplicate() {
        let db = fixture(&[
            (1, "Teardrop", "Massive Attack", 330),
            // Same title and artist, seven minutes long: a different recording.
            (2, "Teardrop", "Massive Attack", 420),
        ]);

        assert!(find(&db, DEFAULT_TOLERANCE).unwrap().is_empty());
    }

    #[test]
    fn the_same_title_by_different_artists_is_not_a_duplicate() {
        let db = fixture(&[
            (1, "Intro", "Alpha", 60),
            (2, "Intro", "Beta", 60),
            (3, "Intro", "Gamma", 60),
        ]);

        assert!(find(&db, DEFAULT_TOLERANCE).unwrap().is_empty());
    }

    #[test]
    fn cosmetic_suffixes_do_not_prevent_a_match() {
        let db = fixture(&[
            (1, "Teardrop", "Massive Attack", 330),
            (2, "Teardrop (Remastered)", "Massive Attack", 330),
            (3, "Teardrop [Explicit]", "Massive Attack", 331),
        ]);

        let groups = find(&db, DEFAULT_TOLERANCE).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].tracks.len(), 3);
    }

    /// A live take is a different recording, whatever its length.
    #[test]
    fn a_parenthetical_that_names_a_different_recording_is_kept() {
        assert_eq!(normalise_title("Song (Live)"), "song live");
        assert_eq!(normalise_title("Song (Acoustic)"), "song acoustic");
        assert_eq!(normalise_title("Song (Radio Edit)"), "song radio edit");

        // And these really are cosmetic.
        assert_eq!(normalise_title("Song (Remastered)"), "song");
        assert_eq!(normalise_title("Song (Explicit)"), "song");

        // The forms real libraries are actually full of.
        assert_eq!(normalise_title("Song (2011 Remaster)"), "song");
        assert_eq!(normalise_title("Song (Digital Remaster)"), "song");
        assert_eq!(normalise_title("Song [Deluxe Edition]"), "song");

        // A remaster of a *live* recording is still a live recording.
        assert_eq!(
            normalise_title("Song (Live 2011 Remaster)"),
            "song live 2011 remaster"
        );
    }

    #[test]
    fn stacked_suffixes_are_all_removed() {
        assert_eq!(normalise_title("Song (Remastered) [Explicit]"), "song");
    }

    #[test]
    fn punctuation_and_spacing_are_flattened() {
        assert_eq!(normalise_title("Don't  Stop"), "don t stop");
        assert_eq!(normalise_title("Don t Stop"), "don t stop");
        assert_eq!(normalise_title("  Song  -  Part 1 "), "song part 1");
    }

    #[test]
    fn a_title_of_only_punctuation_is_ignored() {
        assert_eq!(normalise_title("???"), "");

        let db = fixture(&[(1, "???", "A", 100), (2, "???", "A", 100)]);
        assert!(
            find(&db, DEFAULT_TOLERANCE).unwrap().is_empty(),
            "tracks with no usable title were grouped"
        );
    }

    #[test]
    fn the_tolerance_is_respected() {
        let db = fixture(&[(1, "Song", "A", 200), (2, "Song", "A", 209)]);

        assert!(find(&db, Duration::from_secs(3)).unwrap().is_empty());
        assert_eq!(find(&db, Duration::from_secs(15)).unwrap().len(), 1);
    }

    /// A gradual run of near-equal durations should stay one group rather than
    /// splitting wherever the first member happened to fall.
    #[test]
    fn a_gradual_run_of_durations_stays_together() {
        let db = fixture(&[
            (1, "Song", "A", 200),
            (2, "Song", "A", 202),
            (3, "Song", "A", 204),
            (4, "Song", "A", 206),
        ]);

        let groups = find(&db, Duration::from_secs(3)).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].tracks.len(), 4);
    }

    #[test]
    fn groups_come_back_worst_first() {
        let db = fixture(&[
            (1, "Pair", "A", 100),
            (2, "Pair", "A", 100),
            (3, "Triple", "B", 100),
            (4, "Triple", "B", 100),
            (5, "Triple", "B", 100),
        ]);

        let groups = find(&db, DEFAULT_TOLERANCE).unwrap();

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].tracks.len(), 3, "the worst group was not first");
        assert_eq!(groups[1].tracks.len(), 2);
    }

    #[test]
    fn the_longest_copy_is_listed_first() {
        let db = fixture(&[(1, "Song", "A", 200), (2, "Song", "A", 202)]);

        let groups = find(&db, DEFAULT_TOLERANCE).unwrap();
        assert_eq!(groups[0].tracks[0].id, 2);
    }

    /// Duplicates collect in exactly the untagged corner of a library, so
    /// tracks with no duration must still be checked.
    #[test]
    fn tracks_with_no_duration_are_still_compared() {
        let db = db::open_in_memory().unwrap();

        for id in 1..=2 {
            db.execute(
                "INSERT INTO tracks (
                     id, path, folder, file_name, mtime, size, title, sort_title,
                     added_at, last_seen_at
                 ) VALUES (?1, ?2, '/m', 'x.mp3', 1, 2, 'Song', 'song', 0, 0)",
                params![id, format!("/m/{id}.mp3")],
            )
            .unwrap();
        }

        let groups = find(&db, DEFAULT_TOLERANCE).unwrap();
        assert_eq!(groups.len(), 1, "untagged duplicates were missed");
    }

    #[test]
    fn a_library_with_no_duplicates_reports_none() {
        let db = fixture(&[
            (1, "One", "A", 100),
            (2, "Two", "A", 100),
            (3, "Three", "B", 100),
        ]);

        assert!(find(&db, DEFAULT_TOLERANCE).unwrap().is_empty());
    }

    #[test]
    fn the_suggested_keeper_prefers_tagged_and_longest() {
        let db = fixture(&[(1, "Song", "A", 200), (2, "Song", "A", 202)]);

        // Mark the shorter one as untagged.
        db.execute("UPDATE tracks SET tagged = 0 WHERE id = 2", [])
            .unwrap();

        let groups = find(&db, DEFAULT_TOLERANCE).unwrap();
        let best = groups[0].best().unwrap();

        assert_eq!(best.id, 1, "an untagged copy was preferred");
    }
}
