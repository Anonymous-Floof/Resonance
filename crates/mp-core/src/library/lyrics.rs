//! Finding and reading lyrics for a track.
//!
//! Two shapes exist in the wild and both matter. A `.lrc` sidecar carries a
//! timestamp per line and can be highlighted in time with the music; an
//! embedded `USLT` frame is usually a plain block of text with no timings at
//! all. Rather than modelling those as separate types, a line's timestamp is
//! optional and "synced" is simply whether any line has one — which means the
//! interface has a single thing to draw and degrades to a scrolling block of
//! text without a special case.
//!
//! Nothing here goes near the network. Lyrics come from the user's own files
//! or they do not come at all, in keeping with the rest of the app.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Sidecar extensions checked beside the audio file, in order.
///
/// `.lrc` first because it is the only one that can carry timings. A `.txt`
/// with a *matching stem* is a common way to keep plain lyrics; a bare
/// `readme.txt` in the folder is not, so only the exact stem is accepted.
pub const SIDECAR_EXTENSIONS: &[&str] = &["lrc", "txt"];

/// Where a set of lyrics came from, so the interface can say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A tag inside the audio file itself.
    Embedded,
    /// A file sitting beside the track.
    Sidecar(PathBuf),
    /// Fetched from a service, and cached. Carries the service's name.
    ///
    /// Kept distinct from the two local sources rather than folded in with
    /// them, because the difference is exactly what a user would want to know.
    /// Words that arrived over the network should say so on screen; anything
    /// else would be this build quietly passing off a lookup as something it
    /// found on disk.
    Fetched(String),
}

impl Source {
    /// How this reads on screen, under the words.
    pub fn describe(&self) -> String {
        match self {
            Self::Embedded => "From this file's tags".to_owned(),
            Self::Sidecar(path) => match path.file_name() {
                Some(name) => format!("From {}", name.to_string_lossy()),
                None => "From a file beside this track".to_owned(),
            },
            Self::Fetched(service) => format!("Fetched from {service}"),
        }
    }

    /// Whether these words came from off the machine.
    pub fn is_fetched(&self) -> bool {
        matches!(self, Self::Fetched(_))
    }
}

/// One line, with the moment it is sung if that is known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub at: Option<Duration>,
    pub text: String,
}

/// The lyrics for a track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lyrics {
    pub lines: Vec<Line>,
    pub source: Source,
}

impl Lyrics {
    /// Whether these lines can be followed along with the music.
    pub fn is_synced(&self) -> bool {
        self.lines.iter().any(|line| line.at.is_some())
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Which line is being sung at `position`.
    ///
    /// The last line whose timestamp has passed, so the highlight stays put
    /// through instrumental gaps instead of blinking off between verses.
    /// `None` before the first timestamp, and for unsynced lyrics, where there
    /// is nothing to highlight and pretending otherwise would be a lie.
    pub fn active_at(&self, position: Duration) -> Option<usize> {
        let mut active = None;

        for (index, line) in self.lines.iter().enumerate() {
            match line.at {
                Some(at) if at <= position => active = Some(index),
                Some(_) => break,
                // A blank spacer line between timed ones keeps the previous
                // highlight rather than clearing it.
                None => {}
            }
        }

        active
    }
}

/// Find lyrics for a track, preferring a sidecar over an embedded tag.
///
/// The sidecar wins because it is the one the user put there deliberately, and
/// it is the only one that can be timed. An embedded block is whatever the
/// file arrived with.
pub fn for_track(path: &Path) -> Option<Lyrics> {
    sidecar_for(path).or_else(|| embedded_in(path))
}

/// Read a `.lrc` or `.txt` sitting beside the audio file.
pub fn sidecar_for(path: &Path) -> Option<Lyrics> {
    for extension in SIDECAR_EXTENSIONS {
        let candidate = path.with_extension(extension);

        // `with_extension` on a path that already ends in `.lrc` would hand
        // back the same file, which is not a lyrics sidecar for itself.
        if candidate == path {
            continue;
        }

        let Ok(text) = std::fs::read_to_string(&candidate) else {
            continue;
        };

        let lyrics = parse(&text, Source::Sidecar(candidate));
        if !lyrics.is_empty() {
            return Some(lyrics);
        }
    }

    None
}

/// Read lyrics out of the file's own tags.
pub fn embedded_in(path: &Path) -> Option<Lyrics> {
    use lofty::config::ParseOptions;
    use lofty::prelude::{ItemKey, TaggedFileExt};
    use lofty::probe::Probe;

    let parse_options = ParseOptions::new()
        .read_properties(false)
        .read_tags(true)
        .read_cover_art(false);

    let tagged = Probe::open(path)
        .and_then(|probe| probe.options(parse_options).read())
        .ok()?;

    let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
    let text = tag.get_string(ItemKey::Lyrics)?;

    let lyrics = parse(text, Source::Embedded);
    (!lyrics.is_empty()).then_some(lyrics)
}

/// Parse a block of lyrics, timed or not.
///
/// One entry point for both shapes: a file with no timestamps simply produces
/// lines with no timestamps, so there is no format sniffing to get wrong.
pub fn parse(text: &str, source: Source) -> Lyrics {
    let mut lines: Vec<Line> = Vec::new();

    for raw in text.lines() {
        let raw = raw.trim_end_matches('\r');

        let (stamps, body) = split_timestamps(raw);
        let body = body.trim();

        if stamps.is_empty() {
            // A metadata header like `[ti:Song]` has no timestamp and no words
            // worth showing. Dropping only *bracketed* untimed lines keeps
            // ordinary lyrics that happen to start with a bracket.
            if is_metadata(raw) {
                continue;
            }

            lines.push(Line {
                at: None,
                text: body.to_owned(),
            });
            continue;
        }

        // `[00:12.00][01:04.00]Chorus line` is one line sung twice.
        for at in stamps {
            lines.push(Line {
                at: Some(at),
                text: body.to_owned(),
            });
        }
    }

    // Repeated choruses arrive out of order, and the whole highlight logic
    // assumes ascending time.
    lines.sort_by_key(|line| line.at);

    trim_blank_edges(&mut lines);

    Lyrics { lines, source }
}

/// Peel `[mm:ss.xx]` stamps off the front of a line.
fn split_timestamps(line: &str) -> (Vec<Duration>, &str) {
    let mut stamps = Vec::new();
    let mut rest = line.trim_start();

    while let Some(close) = rest.find(']') {
        if !rest.starts_with('[') {
            break;
        }

        let Some(at) = parse_timestamp(&rest[1..close]) else {
            break;
        };

        stamps.push(at);
        rest = &rest[close + 1..];
    }

    (stamps, rest)
}

/// `mm:ss`, `mm:ss.xx`, or `mm:ss:xx` — all three are in circulation.
fn parse_timestamp(body: &str) -> Option<Duration> {
    let (minutes, rest) = body.split_once(':')?;
    let minutes: u64 = minutes.trim().parse().ok()?;

    // Some writers separate hundredths with a colon rather than a dot.
    let (seconds, fraction) = match rest.split_once(['.', ':']) {
        Some((seconds, fraction)) => (seconds, Some(fraction)),
        None => (rest, None),
    };

    let seconds: u64 = seconds.trim().parse().ok()?;
    if seconds >= 60 {
        return None;
    }

    let millis = match fraction {
        Some(fraction) => {
            let digits: String = fraction.trim().chars().take(3).collect();
            if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            // Two digits are hundredths, three are milliseconds.
            let value: u64 = digits.parse().ok()?;
            match digits.len() {
                1 => value * 100,
                2 => value * 10,
                _ => value,
            }
        }
        None => 0,
    };

    Some(Duration::from_millis(
        (minutes * 60 + seconds) * 1000 + millis,
    ))
}

/// Whether an untimed line is an LRC header rather than words to sing.
fn is_metadata(line: &str) -> bool {
    let line = line.trim();

    line.starts_with('[')
        && line.ends_with(']')
        && line[1..line.len() - 1]
            .split_once(':')
            .is_some_and(|(key, _)| !key.is_empty() && key.chars().all(|c| c.is_ascii_alphabetic()))
}

/// Blank lines at either end are padding in the source file, not silence in
/// the song.
fn trim_blank_edges(lines: &mut Vec<Line>) {
    while lines.first().is_some_and(is_blank) {
        lines.remove(0);
    }
    while lines.last().is_some_and(is_blank) {
        lines.pop();
    }
}

fn is_blank(line: &Line) -> bool {
    line.text.trim().is_empty() && line.at.is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(value: f64) -> Duration {
        Duration::from_secs_f64(value)
    }

    fn lrc(text: &str) -> Lyrics {
        parse(text, Source::Embedded)
    }

    #[test]
    fn a_timed_file_is_parsed_in_order() {
        let lyrics = lrc("[00:12.50]First\n[00:20.00]Second\n[01:05.25]Third");

        assert!(lyrics.is_synced());
        assert_eq!(lyrics.lines.len(), 3);
        assert_eq!(lyrics.lines[0].at, Some(secs(12.5)));
        assert_eq!(lyrics.lines[1].at, Some(secs(20.0)));
        assert_eq!(lyrics.lines[2].at, Some(secs(65.25)));
        assert_eq!(lyrics.lines[2].text, "Third");
    }

    #[test]
    fn plain_text_becomes_untimed_lines() {
        let lyrics = lrc("Hello darkness\nMy old friend");

        assert!(!lyrics.is_synced());
        assert_eq!(lyrics.lines.len(), 2);
        assert!(lyrics.lines.iter().all(|line| line.at.is_none()));
        assert_eq!(lyrics.lines[0].text, "Hello darkness");
    }

    /// The headers real `.lrc` files start with are not part of the song.
    #[test]
    fn metadata_headers_are_dropped() {
        let lyrics = lrc("[ti:Song]\n[ar:Someone]\n[by:A Transcriber]\n[00:01.00]Words");

        assert_eq!(lyrics.lines.len(), 1);
        assert_eq!(lyrics.lines[0].text, "Words");
    }

    /// But a lyric that genuinely begins with a bracket must survive.
    #[test]
    fn a_bracketed_lyric_is_not_mistaken_for_a_header() {
        let lyrics = lrc("[Chorus]\nSing along");

        assert_eq!(lyrics.lines.len(), 2);
        assert_eq!(lyrics.lines[0].text, "[Chorus]");
    }

    /// A repeated chorus is written once with several stamps.
    #[test]
    fn one_line_with_several_stamps_becomes_several_lines() {
        let lyrics = lrc("[00:30.00][01:30.00][02:30.00]Chorus");

        assert_eq!(lyrics.lines.len(), 3);
        assert!(lyrics.lines.iter().all(|line| line.text == "Chorus"));
        assert_eq!(lyrics.lines[1].at, Some(secs(90.0)));
    }

    /// Stamps written out of order still have to end up in time order, or the
    /// highlight walks backwards.
    #[test]
    fn lines_are_sorted_by_time() {
        let lyrics = lrc("[00:40.00]Later\n[00:10.00]Earlier");

        assert_eq!(lyrics.lines[0].text, "Earlier");
        assert_eq!(lyrics.lines[1].text, "Later");
    }

    #[test]
    fn the_several_ways_of_writing_a_timestamp_all_work() {
        assert_eq!(parse_timestamp("00:12"), Some(secs(12.0)));
        assert_eq!(parse_timestamp("00:12.5"), Some(secs(12.5)));
        assert_eq!(parse_timestamp("00:12.50"), Some(secs(12.5)));
        assert_eq!(parse_timestamp("00:12.500"), Some(secs(12.5)));
        assert_eq!(parse_timestamp("00:12:50"), Some(secs(12.5)));
        assert_eq!(parse_timestamp("02:03.00"), Some(secs(123.0)));
    }

    #[test]
    fn nonsense_timestamps_are_refused() {
        assert_eq!(parse_timestamp("hello"), None);
        assert_eq!(parse_timestamp("00:99"), None, "there is no 99th second");
        assert_eq!(parse_timestamp("00:12.ab"), None);
        assert_eq!(parse_timestamp(""), None);
    }

    /// A line that looks stamped but is not keeps its text intact rather than
    /// losing the front of it.
    #[test]
    fn an_unparseable_stamp_leaves_the_line_alone() {
        let lyrics = lrc("[not a time]Still words");

        assert_eq!(lyrics.lines.len(), 1);
        assert_eq!(lyrics.lines[0].text, "[not a time]Still words");
        assert_eq!(lyrics.lines[0].at, None);
    }

    #[test]
    fn the_active_line_is_the_last_one_reached() {
        let lyrics = lrc("[00:10.00]One\n[00:20.00]Two\n[00:30.00]Three");

        assert_eq!(lyrics.active_at(secs(0.0)), None, "nothing sung yet");
        assert_eq!(lyrics.active_at(secs(10.0)), Some(0), "exactly on the cue");
        assert_eq!(lyrics.active_at(secs(15.0)), Some(0));
        assert_eq!(lyrics.active_at(secs(29.9)), Some(1));
        assert_eq!(lyrics.active_at(secs(30.0)), Some(2));
        assert_eq!(
            lyrics.active_at(secs(600.0)),
            Some(2),
            "the last line stays lit through the outro"
        );
    }

    /// Nothing to follow means nothing highlighted — a guess would be worse
    /// than an honest blank.
    #[test]
    fn unsynced_lyrics_have_no_active_line() {
        let lyrics = lrc("Just\nsome\nwords");
        assert_eq!(lyrics.active_at(secs(30.0)), None);
    }

    #[test]
    fn blank_padding_at_the_edges_is_removed() {
        let lyrics = lrc("\n\nWords\n\nMore words\n\n\n");

        assert_eq!(lyrics.lines.first().map(|l| l.text.as_str()), Some("Words"));
        assert_eq!(
            lyrics.lines.last().map(|l| l.text.as_str()),
            Some("More words")
        );
        // The gap in the middle is deliberate spacing and stays.
        assert_eq!(lyrics.lines.len(), 3);
    }

    #[test]
    fn an_empty_file_produces_no_lyrics() {
        assert!(lrc("").is_empty());
        assert!(lrc("\n\n\n").is_empty());
    }

    #[test]
    fn windows_line_endings_do_not_leave_a_stray_return() {
        let lyrics = lrc("[00:05.00]One\r\n[00:09.00]Two\r\n");
        assert_eq!(lyrics.lines[0].text, "One");
        assert_eq!(lyrics.lines[1].text, "Two");
    }

    #[test]
    fn a_sidecar_is_found_beside_the_track() {
        let scratch = tempfile::Builder::new()
            .prefix("resonance-lyrics-")
            .tempdir()
            .unwrap();
        let dir = scratch.path();

        let track = dir.join("Song.mp3");
        std::fs::write(&track, b"not really audio").unwrap();
        std::fs::write(dir.join("Song.lrc"), "[00:03.00]Found me").unwrap();

        let lyrics = sidecar_for(&track).expect("the sidecar is right there");
        assert_eq!(lyrics.lines[0].text, "Found me");
        assert!(matches!(lyrics.source, Source::Sidecar(_)));
    }

    /// A `.txt` only counts when its name matches the track exactly, so a
    /// `readme.txt` in the folder is not mistaken for the words to a song.
    #[test]
    fn an_unrelated_text_file_is_not_treated_as_lyrics() {
        let scratch = tempfile::Builder::new()
            .prefix("resonance-lyrics-txt-")
            .tempdir()
            .unwrap();
        let dir = scratch.path();

        let track = dir.join("Song.mp3");
        std::fs::write(&track, b"not really audio").unwrap();
        std::fs::write(dir.join("readme.txt"), "buy our album").unwrap();

        assert!(sidecar_for(&track).is_none());
    }

    #[test]
    fn a_track_with_nothing_beside_it_has_no_lyrics() {
        let missing = Path::new("no-such-directory-here/Song.mp3");
        assert!(sidecar_for(missing).is_none());
        assert!(for_track(missing).is_none());
    }

    /// Words that arrived over the network must say so. Showing them exactly
    /// like the ones found on disk would be the build quietly passing off a
    /// lookup as something it already had.
    #[test]
    fn fetched_lyrics_say_where_they_came_from() {
        let fetched = Source::Fetched("LRCLIB".to_owned());

        assert!(fetched.is_fetched());
        assert_eq!(fetched.describe(), "Fetched from LRCLIB");
    }

    #[test]
    fn local_lyrics_are_not_reported_as_fetched() {
        assert!(!Source::Embedded.is_fetched());
        assert!(!Source::Sidecar(PathBuf::from("Song.lrc")).is_fetched());

        assert_eq!(Source::Embedded.describe(), "From this file's tags");
        assert_eq!(
            Source::Sidecar(PathBuf::from("C:/music/Song.lrc")).describe(),
            "From Song.lrc",
            "the folder is noise; the filename is the useful part"
        );
    }
}
