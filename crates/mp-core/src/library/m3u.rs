//! Reading and writing M3U8 playlists.
//!
//! M3U is the only playlist format everything else understands, which is the
//! entire reason for supporting it: a playlist that cannot leave Resonance is
//! a playlist held hostage.
//!
//! The format is barely specified — it grew by accretion — so the rule here is
//! to write the strictest thing everyone accepts and read the loosest thing
//! anyone emits. Concretely: written files are UTF-8 with `#EXTM3U`, forward
//! slashes, relative paths where possible and CRLF line endings; read files
//! may use either slash, either line ending, a BOM or not, absolute or
//! relative paths, and may have no `#EXTINF` lines at all.
//!
//! Paths are written relative to the playlist file whenever the track sits
//! under the same root. That is what makes an exported playlist survive being
//! copied to another machine alongside the music, which is the main thing
//! people export playlists *for*.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use super::model::Track;

/// One line of a parsed playlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Resolved against the playlist's own folder when it was relative.
    pub path: PathBuf,
    /// From `#EXTINF`, when the file carried one.
    pub duration: Option<Duration>,
    /// The `Artist - Title` text from `#EXTINF`, unsplit.
    ///
    /// Kept whole rather than split on the first dash: a track called
    /// "Post-Rock - A Song" would be cut in the wrong place, and this is only
    /// ever a fallback label for a file the library has never seen.
    pub label: Option<String>,
}

/// Render tracks as an extended M3U8 playlist.
///
/// `destination` is where the playlist file itself will be written, and is
/// what paths are made relative to. Passing `None` writes absolute paths.
pub fn export(tracks: &[Track], destination: Option<&Path>) -> String {
    let base = destination.and_then(Path::parent);

    let mut out = String::from("#EXTM3U\r\n");

    for track in tracks {
        // -1 is the format's "unknown", and is what every player expects when
        // the length is not known. Rounding up rather than truncating keeps a
        // 3.6 second track from being written as 3.
        let seconds = track
            .duration
            .map_or(-1, |d| d.as_secs_f64().round().max(0.0) as i64);

        out.push_str(&format!(
            "#EXTINF:{seconds},{} - {}\r\n",
            sanitise(&track.artist),
            sanitise(&track.title)
        ));
        out.push_str(&write_path(&track.path, base));
        out.push_str("\r\n");
    }

    out
}

/// Parse a playlist file's contents.
///
/// `source` is the playlist file's own path, used to resolve relative entries.
/// Unreadable or nonsensical lines are skipped rather than failing the import:
/// a playlist with one bad line in fifty should import forty-nine tracks.
pub fn parse(text: &str, source: &Path) -> Vec<Entry> {
    let base = source.parent().unwrap_or(Path::new(""));

    let mut entries = Vec::new();
    // Carried from the `#EXTINF` line down to the path line that follows it.
    let mut pending: Option<(Option<Duration>, Option<String>)> = None;

    for line in text.trim_start_matches('\u{feff}').lines() {
        let line = line.trim();

        if line.is_empty() {
            continue;
        }

        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            pending = Some(parse_extinf(rest));
            continue;
        }

        if line.starts_with('#') {
            // Every other directive — `#EXTM3U`, `#PLAYLIST`, comments.
            continue;
        }

        // Remote entries are silently dropped rather than turned into a
        // nonsense local path. This is a local-first player; a stream URL in
        // an imported playlist is something it genuinely cannot play.
        if is_remote(line) {
            pending = None;
            continue;
        }

        let (duration, label) = pending.take().unwrap_or((None, None));

        entries.push(Entry {
            path: resolve(line, base),
            duration,
            label,
        });
    }

    entries
}

/// `#EXTINF:245,Artist - Title`
fn parse_extinf(rest: &str) -> (Option<Duration>, Option<String>) {
    let (seconds, label) = match rest.split_once(',') {
        Some((seconds, label)) => (seconds, Some(label.trim())),
        None => (rest, None),
    };

    // Some writers put extra attributes after the number, as in
    // `#EXTINF:245 tvg-id="x",Title`. Take the leading number and ignore them.
    let seconds = seconds
        .split_whitespace()
        .next()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| *value >= 0.0)
        .map(Duration::from_secs_f64);

    let label = label.filter(|label| !label.is_empty()).map(str::to_owned);

    (seconds, label)
}

/// Turn a playlist line into a path on this machine.
fn resolve(line: &str, base: &Path) -> PathBuf {
    // `file:///C:/music/x.mp3` shows up in playlists written by media
    // libraries, and is a local path wearing a URL.
    let line = line.strip_prefix("file:///").unwrap_or(line);

    // M3U convention is forward slashes; Windows accepts them, but a playlist
    // written on Windows may well contain backslashes, and those are not
    // separators on other platforms.
    let normalised = line.replace('\\', "/");
    let candidate = PathBuf::from(&normalised);

    if candidate.is_absolute() || is_windows_absolute(&normalised) {
        candidate
    } else {
        normalise(&base.join(candidate))
    }
}

/// `C:/music/x.mp3` is absolute even when this is not Windows.
///
/// Matters because a playlist exported on Windows can be imported anywhere,
/// and joining a drive-lettered path onto a base directory produces gibberish.
fn is_windows_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
}

/// How a track's path is written into the file.
fn write_path(path: &Path, base: Option<&Path>) -> String {
    let relative = base.and_then(|base| relative_to(path, base));

    let text = match relative {
        Some(relative) => relative,
        None => path.to_string_lossy().into_owned(),
    };

    text.replace('\\', "/")
}

/// Express `path` relative to `base`, or `None` when it is not underneath it.
///
/// Deliberately does not climb out with `..`: a playlist full of
/// `../../../../Music/x.mp3` is worse than one with absolute paths, because it
/// breaks silently the moment the file is moved rather than obviously.
fn relative_to(path: &Path, base: &Path) -> Option<String> {
    let path = normalise(path);
    let base = normalise(base);

    let stripped = path.strip_prefix(&base).ok()?;
    Some(stripped.to_string_lossy().into_owned())
}

/// Resolve `.` and `..` textually.
///
/// Not `canonicalize`: that touches the filesystem and fails outright for a
/// file that does not exist, which is exactly the case an import has to
/// survive and report.
fn normalise(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();

    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }

    out
}

fn is_remote(line: &str) -> bool {
    const SCHEMES: &[&str] = &["http://", "https://", "rtsp://", "mms://", "rtmp://"];
    let lower = line.to_ascii_lowercase();
    SCHEMES.iter().any(|scheme| lower.starts_with(scheme))
}

/// Strip what would break the one-line-per-field structure.
fn sanitise(text: &str) -> String {
    text.replace(['\r', '\n'], " ").trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::model::TrackId;

    fn track(id: TrackId, path: &str, artist: &str, title: &str, secs: Option<u64>) -> Track {
        Track {
            id,
            path: PathBuf::from(path),
            title: title.into(),
            artist: artist.into(),
            album: "An Album".into(),
            album_id: None,
            artist_id: None,
            track_no: None,
            disc_no: None,
            year: None,
            duration: secs.map(Duration::from_secs),
            art_id: None,
            tagged: true,
            play_count: 0,
        }
    }

    #[test]
    fn an_exported_playlist_has_the_expected_shape() {
        let tracks = [track(1, "C:/music/a.mp3", "An Artist", "First", Some(245))];
        let text = export(&tracks, Some(Path::new("C:/music/list.m3u8")));

        assert!(text.starts_with("#EXTM3U\r\n"));
        assert!(text.contains("#EXTINF:245,An Artist - First\r\n"));
        assert!(text.contains("a.mp3"));
    }

    /// The point of relative paths: the playlist keeps working when the folder
    /// is copied somewhere else.
    #[test]
    fn paths_under_the_playlist_are_written_relative() {
        let tracks = [track(1, "C:/music/rock/a.mp3", "X", "A", None)];
        let text = export(&tracks, Some(Path::new("C:/music/list.m3u8")));

        assert!(
            text.contains("\r\nrock/a.mp3\r\n"),
            "expected a relative path, got:\n{text}"
        );
        assert!(!text.contains("C:/music/rock"));
    }

    /// And a track from somewhere else entirely has to stay absolute.
    #[test]
    fn paths_outside_the_playlist_stay_absolute() {
        let tracks = [track(1, "D:/elsewhere/b.mp3", "X", "B", None)];
        let text = export(&tracks, Some(Path::new("C:/music/list.m3u8")));

        assert!(text.contains("D:/elsewhere/b.mp3"), "got:\n{text}");
    }

    #[test]
    fn backslashes_are_written_as_forward_slashes() {
        let tracks = [track(1, r"D:\elsewhere\c.mp3", "X", "C", None)];
        let text = export(&tracks, None);

        assert!(text.contains("D:/elsewhere/c.mp3"), "got:\n{text}");
        assert!(!text.contains('\\'));
    }

    #[test]
    fn an_unknown_duration_is_written_as_minus_one() {
        let tracks = [track(1, "C:/music/a.mp3", "X", "A", None)];
        let text = export(&tracks, None);
        assert!(text.contains("#EXTINF:-1,X - A"), "got:\n{text}");
    }

    #[test]
    fn a_playlist_survives_a_round_trip() {
        let tracks = [
            track(1, "C:/music/a.mp3", "First Artist", "One", Some(100)),
            track(2, "C:/music/deep/b.flac", "Second Artist", "Two", Some(250)),
            track(3, "D:/other/c.ogg", "Third Artist", "Three", None),
        ];

        let destination = Path::new("C:/music/list.m3u8");
        let text = export(&tracks, Some(destination));
        let back = parse(&text, destination);

        assert_eq!(back.len(), 3);
        for (original, entry) in tracks.iter().zip(&back) {
            assert_eq!(
                normalise(&entry.path),
                normalise(&PathBuf::from(
                    original.path.to_string_lossy().replace('\\', "/")
                )),
                "{} did not round-trip",
                original.title
            );
            assert_eq!(entry.duration, original.duration);
        }
    }

    #[test]
    fn relative_entries_resolve_against_the_playlist() {
        let entries = parse(
            "#EXTM3U\n#EXTINF:10,A - B\nsub/track.mp3\n",
            Path::new("C:/music/list.m3u8"),
        );

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, PathBuf::from("C:/music/sub/track.mp3"));
    }

    #[test]
    fn parent_directory_hops_are_resolved() {
        let entries = parse("../above/track.mp3\n", Path::new("C:/music/lists/l.m3u"));
        assert_eq!(entries[0].path, PathBuf::from("C:/music/above/track.mp3"));
    }

    /// A Windows-absolute path must not be treated as relative, even when the
    /// playlist is being read on another platform.
    #[test]
    fn drive_letters_are_recognised_as_absolute() {
        let entries = parse("D:\\music\\track.mp3\n", Path::new("C:/lists/l.m3u"));
        assert_eq!(entries[0].path, PathBuf::from("D:/music/track.mp3"));
    }

    #[test]
    fn file_urls_are_unwrapped() {
        let entries = parse("file:///C:/music/x.mp3\n", Path::new("C:/lists/l.m3u"));
        assert_eq!(entries[0].path, PathBuf::from("C:/music/x.mp3"));
    }

    /// A plain list of filenames with no directives at all is still a valid
    /// M3U, and plenty of them exist.
    #[test]
    fn a_playlist_with_no_extinf_lines_still_parses() {
        let entries = parse("a.mp3\nb.mp3\n", Path::new("C:/music/l.m3u"));

        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|entry| entry.duration.is_none()));
        assert!(entries.iter().all(|entry| entry.label.is_none()));
    }

    #[test]
    fn comments_blank_lines_and_a_bom_are_ignored() {
        let entries = parse(
            "\u{feff}#EXTM3U\r\n\r\n# just a comment\r\n#PLAYLIST:Mine\r\na.mp3\r\n",
            Path::new("C:/music/l.m3u"),
        );

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, PathBuf::from("C:/music/a.mp3"));
    }

    /// This is a local player; a stream URL is something it cannot play, and a
    /// broken entry is worse than an absent one.
    #[test]
    fn remote_urls_are_dropped() {
        let entries = parse(
            "#EXTINF:-1,Radio\nhttp://example.com/stream\n#EXTINF:5,Local\na.mp3\n",
            Path::new("C:/music/l.m3u"),
        );

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].label.as_deref(), Some("Local"));
    }

    /// The `#EXTINF` belonging to a dropped stream must not be reused by the
    /// next track down.
    #[test]
    fn a_dropped_entry_does_not_leak_its_metadata() {
        let entries = parse(
            "#EXTINF:999,Radio Station\nhttps://example.com/s\na.mp3\n",
            Path::new("C:/music/l.m3u"),
        );

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].label, None, "the stream's label leaked");
        assert_eq!(entries[0].duration, None);
    }

    #[test]
    fn extinf_labels_are_kept_whole() {
        let entries = parse(
            "#EXTINF:10,Post-Rock Band - A Song - Live\na.mp3\n",
            Path::new("C:/l.m3u"),
        );

        assert_eq!(
            entries[0].label.as_deref(),
            Some("Post-Rock Band - A Song - Live")
        );
    }

    #[test]
    fn extra_attributes_after_the_duration_are_tolerated() {
        let entries = parse(
            "#EXTINF:120 tvg-id=\"x\",A - B\na.mp3\n",
            Path::new("C:/l.m3u"),
        );

        assert_eq!(entries[0].duration, Some(Duration::from_secs(120)));
        assert_eq!(entries[0].label.as_deref(), Some("A - B"));
    }

    #[test]
    fn a_malformed_extinf_does_not_lose_the_track() {
        let entries = parse("#EXTINF:not-a-number,A - B\na.mp3\n", Path::new("C:/l.m3u"));

        assert_eq!(entries.len(), 1, "the track should still be imported");
        assert_eq!(entries[0].duration, None);
    }

    #[test]
    fn newlines_in_a_title_cannot_break_the_file() {
        let tracks = [track(1, "C:/a.mp3", "Art\nist", "Ti\rtle", Some(5))];
        let text = export(&tracks, None);

        // Header, one EXTINF, one path, and the trailing empty piece.
        assert_eq!(text.lines().count(), 3, "got:\n{text}");
        assert!(text.contains("#EXTINF:5,Art ist - Ti tle"));
    }

    #[test]
    fn an_empty_playlist_is_still_a_valid_file() {
        let text = export(&[], None);
        assert_eq!(text, "#EXTM3U\r\n");
        assert!(parse(&text, Path::new("C:/l.m3u")).is_empty());
    }
}
