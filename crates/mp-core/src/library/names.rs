//! Making sense of filenames when the tags are missing or useless.
//!
//! A large share of this collection is untagged files sitting loose in one
//! folder, named the way a downloader leaves them:
//!
//! ```text
//! 03 - Artist Name - Song Title (Official Music Video) [aB3dEfGhIjK].mp3
//! ```
//!
//! Without this, every one of those tracks shows up as an unreadable filename
//! under "Unknown Artist", which is the difference between a library and a
//! folder listing. The parser is deliberately conservative: it only removes
//! decoration it recognises, because inventing an artist that is really part
//! of the title is worse than leaving the title long.

/// What could be recovered from a filename.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ParsedName {
    /// Present only when the name had a clear `Artist - Title` split.
    pub artist: Option<String>,
    /// Always set; falls back to the cleaned-up whole name.
    pub title: String,
    /// A leading track number, if the name started with one.
    pub track_no: Option<u32>,
}

/// Parenthesised or bracketed asides that carry no information about the song.
///
/// Matched against the *whole* aside, lowercased, after stripping punctuation —
/// so `(Official Video)` goes but `(Acoustic Version)` and `(feat. Someone)`
/// stay, because those genuinely distinguish one recording from another.
const NOISE: &[&str] = &[
    "official video",
    "official music video",
    "official audio",
    "official lyric video",
    "official lyrics video",
    "official visualizer",
    "official visualiser",
    "official",
    "music video",
    "lyric video",
    "lyrics video",
    "lyrics",
    "lyric",
    "audio",
    "video",
    "visualizer",
    "visualiser",
    "hd",
    "hq",
    "4k",
    "1080p",
    "720p",
    "full hd",
    "full song",
    "full version",
    "with lyrics",
    "free download",
    "download",
    "explicit",
    "clean",
    "no copyright music",
    "copyright free",
    "ncs release",
    "monstercat release",
];

/// Separators used between artist and title, longest first so `" -- "` is not
/// mistaken for `" - "` twice.
const SEPARATORS: &[&str] = &[" — ", " – ", " -- ", " - ", " ~ ", " _ "];

/// Recover artist, title and track number from a file stem.
///
/// `stem` should already have its extension removed.
pub fn parse(stem: &str) -> ParsedName {
    let mut text = normalise_whitespace(&stem.replace('_', " "));

    let track_no = strip_leading_track_number(&mut text);
    strip_asides(&mut text);

    let mut parsed = split_artist_title(&text);
    parsed.track_no = track_no;

    if parsed.title.trim().is_empty() {
        // Everything was decoration. Better a filename than a blank row.
        parsed.title = normalise_whitespace(stem);
        parsed.artist = None;
    }

    parsed
}

/// Collapse runs of whitespace and trim the ends.
fn normalise_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Remove a leading `01`, `01.`, `1-05` and friends, returning the number.
///
/// Requires a following separator or space so a song actually called "1979"
/// does not lose its title.
fn strip_leading_track_number(text: &mut String) -> Option<u32> {
    let trimmed = text.trim_start();

    let digits: String = trimmed.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() || digits.len() > 3 {
        return None;
    }

    let rest = &trimmed[digits.len()..];

    // `1-05 Title` is disc 1 track 5; the disc number is dropped here because
    // it belongs to the album, and a tagged file will carry it properly.
    if let Some(after_dash) = rest.strip_prefix('-') {
        let inner: String = after_dash
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if !inner.is_empty() && inner.len() <= 3 {
            let remainder = after_dash[inner.len()..].trim_start_matches(['.', ')', '-', ' ']);
            if !remainder.is_empty() && remainder != &after_dash[inner.len()..] {
                *text = remainder.to_owned();
                return inner.parse().ok();
            }
        }
    }

    let remainder = rest.trim_start_matches(['.', ')', '-', ' ']);

    // A bare number must actually be separated from what follows, otherwise
    // `1979` would be read as track 1979 of nothing.
    if remainder.len() == rest.len() || remainder.is_empty() {
        return None;
    }

    // A *single* digit separated only by a space is far more likely to be part
    // of the name than a track number: "8 Ravens", "4 Winter Roads", "2 Rivers".
    // Real track numbering either pads to two digits ("01 Title") or uses
    // punctuation ("1. Title"), so requiring one of those costs nothing and
    // stops the library inventing an artist called "Ravens".
    let separator = &rest[..rest.len() - remainder.len()];
    if digits.len() < 2 && !separator.contains(['.', ')', '-']) {
        return None;
    }

    *text = remainder.to_owned();
    digits.parse().ok()
}

/// Drop `(...)` and `[...]` groups that are pure decoration.
fn strip_asides(text: &mut String) {
    let mut out = String::with_capacity(text.len());
    let mut rest = text.as_str();

    while let Some(open) = rest.find(['(', '[']) {
        let opener = rest.as_bytes()[open];
        let closer = if opener == b'(' { ')' } else { ']' };

        let Some(close) = rest[open + 1..].find(closer) else {
            // Unbalanced: keep the remainder verbatim rather than guessing.
            break;
        };
        let close = open + 1 + close;

        let inner = &rest[open + 1..close];
        out.push_str(&rest[..open]);
        if !is_noise(inner, opener == b'[') {
            out.push_str(&rest[open..=close]);
        }
        rest = &rest[close + 1..];
    }

    out.push_str(rest);
    *text = normalise_whitespace(&out);
}

/// Whether an aside can be dropped without losing meaning.
///
/// Square brackets are treated more aggressively because that is where video
/// ids and site tags live, and almost never anything about the music.
fn is_noise(inner: &str, square: bool) -> bool {
    let cleaned: String = inner
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect();
    let cleaned = normalise_whitespace(&cleaned).to_lowercase();

    if cleaned.is_empty() {
        return true;
    }

    if NOISE.contains(&cleaned.as_str()) {
        return true;
    }

    if square {
        // An 11-character mixed-case token is a YouTube id; a run of digits is
        // a release number. Neither is worth showing.
        let compact = inner.trim();
        if compact.len() == 11
            && compact
                .chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
        {
            return true;
        }
        if cleaned.chars().all(|c| c.is_ascii_digit()) {
            return true;
        }
    }

    false
}

/// Split on the first artist/title separator, if there is one.
fn split_artist_title(text: &str) -> ParsedName {
    for separator in SEPARATORS {
        if let Some(index) = text.find(separator) {
            let artist = text[..index].trim();
            let title = text[index + separator.len()..].trim();

            // Both halves must be substantial. A stray dash inside a title
            // ("Nothing - " or " - Intro") should not create an empty artist.
            if artist.len() >= 2 && !title.is_empty() {
                return ParsedName {
                    artist: Some(artist.to_owned()),
                    title: title.to_owned(),
                    track_no: None,
                };
            }
        }
    }

    ParsedName {
        artist: None,
        title: text.trim().to_owned(),
        track_no: None,
    }
}

/// Suffixes YouTube and rippers append to a channel name.
///
/// `Nightgrove - Topic` is not a band called "Nightgrove - Topic"; it is YouTube's
/// auto-generated channel for Nightgrove. Left in place it splits one artist into
/// two entries in the artist list, which is exactly the mess this library is
/// meant to clean up.
const CHANNEL_SUFFIXES: &[&str] = &[
    " - topic",
    " - official channel",
    " - official",
    " official channel",
    "vevo",
];

/// Remove a channel suffix from an artist name.
pub fn strip_channel_suffix(artist: &str) -> String {
    let trimmed = artist.trim();
    let lowered = trimmed.to_lowercase();

    for suffix in CHANNEL_SUFFIXES {
        if let Some(head) = lowered.strip_suffix(suffix) {
            let stripped = trimmed[..head.len()]
                .trim_end_matches([' ', '-', '_'])
                .trim();
            // Never strip a name down to nothing: an artist genuinely called
            // "VEVO" is odd, but an empty row helps nobody.
            if !stripped.is_empty() {
                return stripped.to_owned();
            }
        }
    }

    trimmed.to_owned()
}

/// Top-level domains seen in the watermarks rippers leave in filenames.
const WATERMARK_TLDS: &[&str] = &[
    "com", "net", "org", "vip", "info", "xyz", "me", "cc", "to", "io", "ru", "biz", "top", "site",
    "club", "online", "mobi", "co", "pw",
];

/// Strip download-site watermarks and bitrate stamps from a title.
///
/// These are appended by the sites the files came from - `myfreemp3.vip`,
/// `my-free-mp3s.com` - and are pure noise in a track list.
pub fn strip_watermarks(title: &str) -> String {
    let kept: Vec<&str> = title
        .split_whitespace()
        .filter(|token| !is_watermark(token))
        .collect();

    let cleaned = kept.join(" ");
    let cleaned = cleaned.trim_matches(|c: char| c == '-' || c == '_' || c.is_whitespace());

    // A title that was nothing but a watermark keeps its original text.
    if cleaned.is_empty() {
        title.trim().to_owned()
    } else {
        cleaned.to_owned()
    }
}

/// Whether one whitespace-delimited token is site noise rather than words.
fn is_watermark(token: &str) -> bool {
    let bare = token.trim_matches(|c: char| !c.is_alphanumeric());
    let lowered = bare.to_lowercase();

    if let Some((host, tld)) = lowered.rsplit_once('.') {
        // Domain-shaped, and the host part has to look like a hostname rather
        // than an initialism - so `A.M.P.` and a trailing full stop survive.
        let host_is_plausible = host.len() >= 3 && host.contains(|c: char| c.is_ascii_alphabetic());
        if WATERMARK_TLDS.contains(&tld) && host_is_plausible {
            return true;
        }
    }

    // `320kbps` and friends.
    lowered
        .strip_suffix("kbps")
        .is_some_and(|digits| !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()))
}

/// Drop a leading `Artist - ` from a title that repeats it.
///
/// Common when a tagger copied the whole filename into the title field, which
/// leaves every row on an artist's page reading "Artist - Artist - Song".
pub fn strip_redundant_artist_prefix(title: &str, artist: &str) -> String {
    let title = title.trim();
    let artist = artist.trim();
    if artist.is_empty() {
        return title.to_owned();
    }

    let lowered_title = title.to_lowercase();
    let lowered_artist = artist.to_lowercase();

    let Some(rest) = lowered_title.strip_prefix(&lowered_artist) else {
        return title.to_owned();
    };

    for separator in SEPARATORS {
        if let Some(tail) = rest.strip_prefix(separator) {
            // Never leave the title empty - a self-titled track keeps its name.
            if tail.trim().is_empty() {
                break;
            }
            let start = title.len() - tail.len();
            return title[start..].trim().to_owned();
        }
    }

    title.to_owned()
}

/// Split `Artist-Title` where there are no spaces around the dash.
///
/// Wildly ambiguous on its own - `Winds-of-Fjord` is one title, not an artist
/// called "Winds" - so this only ever *proposes* a split. The caller accepts it
/// solely when the proposed artist is one it has already seen elsewhere in the
/// library, which is what makes it safe.
pub fn propose_tight_split(text: &str) -> Option<(&str, &str)> {
    let index = text.find('-')?;
    let (left, right) = (text[..index].trim(), text[index + 1..].trim());

    // Both halves must be substantial, and the artist half must be a single
    // word: a spaceless dash inside a phrase is punctuation, not a separator.
    if left.len() < 2 || right.len() < 2 || left.contains(' ') {
        return None;
    }

    Some((left, right))
}

/// Entities that turn up in tags scraped from web pages.
const ENTITIES: &[(&str, &str)] = &[
    ("&amp;", "&"),
    ("&quot;", "\""),
    ("&apos;", "'"),
    ("&#39;", "'"),
    ("&#039;", "'"),
    ("&lt;", "<"),
    ("&gt;", ">"),
    ("&nbsp;", " "),
    ("&hellip;", "\u{2026}"),
];

/// Decode the HTML entities that leak into tags from scraped metadata.
///
/// `Dynoro &amp; Gigi D'Agostino` is a real entry in this collection: some
/// tagger copied it straight out of a web page. Nothing else in the pipeline
/// would ever fix it, and it sorts and searches wrongly as well as reading
/// wrongly.
pub fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }

    let mut out = text.to_owned();
    for (entity, replacement) in ENTITIES {
        if out.contains(entity) {
            out = out.replace(entity, replacement);
        }
    }
    out
}

/// A comparison key that sorts the way a person expects a list to be sorted.
///
/// Lowercased, leading articles optionally moved out of the way, and leading
/// punctuation dropped so `"#YOLO"` files under Y rather than above every
/// letter.
pub fn sort_key(name: &str, ignore_articles: bool) -> String {
    let lowered = name.trim().to_lowercase();
    let trimmed = lowered.trim_start_matches(|c: char| !c.is_alphanumeric());
    let trimmed = if trimmed.is_empty() {
        &lowered
    } else {
        trimmed
    };

    if !ignore_articles {
        return trimmed.to_owned();
    }

    for article in ["the ", "a ", "an "] {
        if let Some(rest) = trimmed.strip_prefix(article)
            && !rest.trim().is_empty()
        {
            return rest.trim_start().to_owned();
        }
    }

    trimmed.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_downloader_filename_yields_artist_and_title() {
        let p = parse("Bitter Compass - Under Streetlights (Official Video) [aB3dEfGhIjK]");
        assert_eq!(p.artist.as_deref(), Some("Bitter Compass"));
        assert_eq!(p.title, "Under Streetlights");
    }

    #[test]
    fn leading_track_numbers_are_recovered_not_shown() {
        let p = parse("03 - Vellichor - Paper Lantern");
        assert_eq!(p.track_no, Some(3));
        assert_eq!(p.artist.as_deref(), Some("Vellichor"));
        assert_eq!(p.title, "Paper Lantern");
    }

    /// A band whose name starts with a digit is not track 8 of anything.
    ///
    /// This shipped wrong: "8 Ravens" was filed under an artist called
    /// "Ravens", splitting one artist across two entries.
    #[test]
    fn a_leading_digit_in_a_name_is_not_a_track_number() {
        let p = parse("8 Ravens - Lie");
        assert_eq!(p.artist.as_deref(), Some("8 Ravens"));
        assert_eq!(p.title, "Lie");
        assert_eq!(p.track_no, None);

        let p = parse("2 Rivers");
        assert_eq!(p.title, "2 Rivers");
        assert_eq!(p.track_no, None);

        let p = parse("4 Winter Roads - Kryptonite");
        assert_eq!(p.artist.as_deref(), Some("4 Winter Roads"));
    }

    /// Real track numbering still has to work: padded, or punctuated.
    #[test]
    fn conventional_track_numbering_is_still_recognised() {
        assert_eq!(parse("01 Some Title").track_no, Some(1));
        assert_eq!(parse("1. Some Title").track_no, Some(1));
        assert_eq!(parse("7 - Some Title").track_no, Some(7));
        assert_eq!(parse("12 Some Title").track_no, Some(12));
    }

    #[test]
    fn a_numeric_title_is_not_eaten_as_a_track_number() {
        let p = parse("1979");
        assert_eq!(p.track_no, None);
        assert_eq!(p.title, "1979");
    }

    /// The whole point of the conservative noise list: a parenthetical that
    /// distinguishes two recordings has to survive.
    #[test]
    fn meaningful_parentheticals_are_kept() {
        let p = parse("Someone - Song Title (Acoustic Version)");
        assert_eq!(p.title, "Song Title (Acoustic Version)");

        let p = parse("Someone - Song Title (feat. Guest)");
        assert_eq!(p.title, "Song Title (feat. Guest)");
    }

    #[test]
    fn underscores_become_spaces() {
        let p = parse("Some_Artist_-_Some_Song");
        assert_eq!(p.artist.as_deref(), Some("Some Artist"));
        assert_eq!(p.title, "Some Song");
    }

    #[test]
    fn a_name_with_no_separator_is_all_title() {
        let p = parse("As They Bloom");
        assert_eq!(p.artist, None);
        assert_eq!(p.title, "As They Bloom");
    }

    #[test]
    fn a_name_that_is_entirely_decoration_keeps_the_filename() {
        let p = parse("(Official Video)");
        assert_eq!(p.title, "(Official Video)");
    }

    #[test]
    fn stray_dashes_do_not_invent_an_artist() {
        let p = parse("2 Rivers");
        assert_eq!(p.artist, None);

        let p = parse("A - Title");
        assert_eq!(p.artist, None, "a one-letter artist is more likely a typo");
    }

    #[test]
    fn sort_keys_ignore_articles_and_leading_punctuation() {
        assert_eq!(sort_key("The Wandering Hours", true), "wandering hours");
        assert_eq!(
            sort_key("The Wandering Hours", false),
            "the wandering hours"
        );
        assert_eq!(sort_key("#YOLO", true), "yolo");
        assert_eq!(sort_key("  a Distant Signal", true), "distant signal");
    }

    #[test]
    fn youtube_channel_suffixes_are_removed() {
        assert_eq!(strip_channel_suffix("Nightgrove - Topic"), "Nightgrove");
        assert_eq!(strip_channel_suffix("SABLE - Topic"), "SABLE");
        assert_eq!(strip_channel_suffix("HalcyonVEVO"), "Halcyon");
        assert_eq!(strip_channel_suffix("Vellichor"), "Vellichor");
    }

    /// The suffix is the whole name here, so nothing should be stripped.
    #[test]
    fn a_name_that_is_only_a_suffix_survives() {
        assert_eq!(strip_channel_suffix("VEVO"), "VEVO");
    }

    #[test]
    fn download_site_watermarks_are_removed() {
        assert_eq!(strip_watermarks("Undertow myfreemp3.vip"), "Undertow");
        assert_eq!(
            strip_watermarks("Computer games my-free-mp3s.com"),
            "Computer games"
        );
        assert_eq!(strip_watermarks("Song 320kbps"), "Song");
    }

    /// A domain-shaped word is only noise at the edges; ordinary punctuation
    /// and real titles have to survive untouched.
    #[test]
    fn ordinary_titles_are_not_mistaken_for_watermarks() {
        assert_eq!(strip_watermarks("Yes. No. Maybe."), "Yes. No. Maybe.");
        assert_eq!(strip_watermarks("A.M.P."), "A.M.P.");
        assert_eq!(strip_watermarks("Nothing to strip"), "Nothing to strip");
    }

    #[test]
    fn a_title_repeating_its_artist_is_shortened() {
        assert_eq!(
            strip_redundant_artist_prefix("Silver Junction - Winter Signal", "Silver Junction"),
            "Winter Signal"
        );
        assert_eq!(
            strip_redundant_artist_prefix("Winter Signal", "Silver Junction"),
            "Winter Signal"
        );
    }

    /// Stripping must never empty the title.
    #[test]
    fn a_self_titled_track_keeps_its_title() {
        assert_eq!(
            strip_redundant_artist_prefix("Vellichor", "Vellichor"),
            "Vellichor"
        );
        assert_eq!(
            strip_redundant_artist_prefix("Vellichor - ", "Vellichor"),
            "Vellichor -"
        );
    }

    #[test]
    fn a_tight_split_is_only_ever_proposed() {
        assert_eq!(
            propose_tight_split("TryHardNinja-We Know What Scares You"),
            Some(("TryHardNinja", "We Know What Scares You"))
        );
        // Proposed, but the caller rejects it: "Winds" is not a known artist.
        assert_eq!(
            propose_tight_split("Winds-of-Fjord"),
            Some(("Winds", "of-Fjord"))
        );
        assert_eq!(propose_tight_split("No dash here"), None);
        assert_eq!(propose_tight_split("Two words-x"), None);
    }

    #[test]
    fn html_entities_in_tags_are_decoded() {
        assert_eq!(
            decode_entities("Dynoro &amp; Gigi D&#39;Agostino"),
            "Dynoro & Gigi D'Agostino"
        );
        assert_eq!(decode_entities("AT&T"), "AT&T", "a bare ampersand is fine");
        assert_eq!(decode_entities("no entities"), "no entities");
    }

    /// "The" on its own is a band name, not an article.
    #[test]
    fn an_article_alone_is_left_alone() {
        assert_eq!(sort_key("The", true), "the");
    }
}
