//! A flat recursive folder scan, for tests and headless tools.
//!
//! This is **not** the application's scanner — `mp_core::library::ingest` is,
//! and it is what the UI uses. This exists so the audio examples can find some
//! files to play without dragging SQLite, tag parsing and image decoding into a
//! test whose entire subject is whether the decoder produces clean samples.

use std::path::{Path, PathBuf};

use mp_core::format::{self, Support};

/// A file that was found but cannot be played.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejected {
    pub path: PathBuf,
    pub reason: &'static str,
}

/// The result of walking one or more folders.
#[derive(Debug, Default, Clone)]
pub struct ScanResult {
    /// Playable files, sorted by path.
    pub tracks: Vec<PathBuf>,
    /// Audio files this build cannot decode, with the reason.
    pub rejected: Vec<Rejected>,
    /// Directories that could not be read (permissions, a disconnected drive).
    pub unreadable: Vec<PathBuf>,
}

impl ScanResult {
    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    pub fn len(&self) -> usize {
        self.tracks.len()
    }
}

/// Maximum directory depth, to stop a symlink cycle from running forever.
const MAX_DEPTH: usize = 24;

/// Walk `roots` and collect everything playable beneath them.
pub fn scan(roots: &[PathBuf]) -> ScanResult {
    let mut result = ScanResult::default();

    for root in roots {
        walk(root, 0, &mut result);
    }

    // A stable order keeps the track list from reshuffling between scans.
    result.tracks.sort();
    result.tracks.dedup();
    result.rejected.sort_by(|a, b| a.path.cmp(&b.path));
    result.rejected.dedup();

    result
}

fn walk(path: &Path, depth: usize, result: &mut ScanResult) {
    if depth > MAX_DEPTH {
        tracing::warn!("stopping at {}: nested too deeply", path.display());
        return;
    }

    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) => {
            tracing::debug!("cannot stat {}: {err}", path.display());
            return;
        }
    };

    // Following symlinks risks cycles and duplicate entries; a music folder has
    // no real need for them.
    if metadata.is_symlink() {
        return;
    }

    if metadata.is_file() {
        match format::classify(path) {
            Support::Supported => result.tracks.push(path.to_path_buf()),
            Support::Unsupported { reason } => result.rejected.push(Rejected {
                path: path.to_path_buf(),
                reason,
            }),
            Support::NotAudio => {}
        }
        return;
    }

    if !metadata.is_dir() {
        return;
    }

    // Skip the caches and metadata folders that music tools scatter around.
    if is_ignored_dir(path) {
        return;
    }

    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(err) => {
            tracing::debug!("cannot read {}: {err}", path.display());
            result.unreadable.push(path.to_path_buf());
            return;
        }
    };

    for entry in entries.flatten() {
        walk(&entry.path(), depth + 1, result);
    }
}

/// Directories that never contain music worth indexing.
fn is_ignored_dir(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };

    // Hidden folders (including the `.thumbnails` cache Windows leaves behind)
    // plus the usual system noise.
    name.starts_with('.')
        || matches!(
            name.to_ascii_lowercase().as_str(),
            "$recycle.bin" | "system volume information" | "__macosx"
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Build a small folder tree in a unique temporary directory.
    /// A scratch tree that removes itself when the guard is dropped.
    fn fixture(name: &str, files: &[&str]) -> tempfile::TempDir {
        let guard = tempfile::Builder::new()
            .prefix(&format!("resonance-scan-{name}-"))
            .tempdir()
            .expect("temp dir");
        let root = guard.path();

        for relative in files {
            let path = root.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("creating the fixture tree");
            }
            fs::write(&path, b"not really audio").expect("writing a fixture file");
        }

        guard
    }

    #[test]
    fn finds_playable_files_recursively() {
        let scratch = fixture(
            "recursive",
            &["a.mp3", "album/b.flac", "album/deep/c.wav", "notes.txt"],
        );
        let root = scratch.path().to_path_buf();

        let result = scan(std::slice::from_ref(&root));

        assert_eq!(result.tracks.len(), 3, "text files must not be picked up");
        assert!(result.tracks.iter().all(|p| p.starts_with(&root)));
    }

    /// The point of `rejected`: an unplayable file is reported, not dropped.
    #[test]
    fn unsupported_audio_is_reported_rather_than_ignored() {
        let scratch = fixture("rejects", &["good.mp3", "mix.opus"]);
        let root = scratch.path().to_path_buf();

        let result = scan(std::slice::from_ref(&root));

        assert_eq!(result.tracks.len(), 1);
        assert_eq!(result.rejected.len(), 1);
        assert!(result.rejected[0].path.ends_with("mix.opus"));
        assert!(result.rejected[0].reason.contains("Opus"));
    }

    #[test]
    fn hidden_and_cache_folders_are_skipped() {
        let scratch = fixture(
            "hidden",
            &["real.mp3", ".thumbnails/thumb.mp3", "__MACOSX/junk.mp3"],
        );
        let root = scratch.path().to_path_buf();

        let result = scan(std::slice::from_ref(&root));

        assert_eq!(result.tracks.len(), 1);
        assert!(result.tracks[0].ends_with("real.mp3"));
    }

    #[test]
    fn results_are_sorted_and_deduplicated() {
        let scratch = fixture("dupes", &["b.mp3", "a.mp3"]);
        let root = scratch.path().to_path_buf();

        // The same root twice must not yield each track twice.
        let result = scan(&[root.clone(), root.clone()]);

        assert_eq!(result.tracks.len(), 2);
        assert!(result.tracks[0].ends_with("a.mp3"));
        assert!(result.tracks[1].ends_with("b.mp3"));
    }

    #[test]
    fn a_missing_folder_is_not_fatal() {
        let missing = std::env::temp_dir().join("resonance-scan-does-not-exist");
        let result = scan(&[missing]);
        assert!(result.is_empty());
    }
}
