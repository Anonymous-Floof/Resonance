//! Which files this build can actually decode.
//!
//! The scanner and the queue both need to answer "can we play this?" *before*
//! trying, so an unplayable file can be reported to the user rather than
//! silently disappearing or failing at the moment they press play.
//!
//! Support here tracks the `symphonia` feature set enabled in the workspace
//! manifest (`features = ["all"]`), which is pure Rust and covers every codec
//! and container symphonia ships.

use std::path::Path;

/// Whether a path is something this build can decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
    /// A container and codec symphonia can decode.
    Supported,
    /// Recognisably an audio file, but not decodable by this build.
    ///
    /// `reason` is written for the user, not the log.
    Unsupported { reason: &'static str },
    /// Not an audio file as far as we are concerned.
    NotAudio,
}

impl Support {
    pub fn is_supported(self) -> bool {
        matches!(self, Self::Supported)
    }
}

/// Extensions symphonia can open with the `all` feature enabled.
///
/// Containers: wav/aiff/caf (RIFF family), isomp4, mkv/webm, ogg, plus the
/// bare-stream formats (mp3, flac, aac/adts).
pub const SUPPORTED_EXTENSIONS: &[&str] = &[
    // MPEG audio
    "mp3", "mp2", "mp1", // AAC, bare ADTS and in MP4
    "aac", "adts", "m4a", "m4b", "m4p", "mp4", // Lossless
    "flac", "alac", // Uncompressed / PCM containers
    "wav", "wave", "aiff", "aif", "aifc", "caf", // Xiph and Matroska
    "ogg", "oga", "mka", "webm",
];

/// Audio extensions we deliberately recognise in order to *reject* them well.
///
/// Without this table an `.opus` file would look like an ordinary unknown file
/// and vanish from the library with no explanation.
const UNSUPPORTED_AUDIO: &[(&str, &str)] = &[
    ("opus", "Opus is not supported by this build"),
    ("wma", "Windows Media Audio is not supported"),
    ("ape", "Monkey's Audio is not supported"),
    ("wv", "WavPack is not supported"),
    ("tta", "True Audio is not supported"),
    ("mpc", "Musepack is not supported"),
    ("spx", "Speex is not supported"),
    ("dsf", "DSD audio is not supported"),
    ("dff", "DSD audio is not supported"),
    ("ra", "RealAudio is not supported"),
    ("amr", "AMR is not supported"),
    ("mid", "MIDI is a score, not recorded audio"),
    ("midi", "MIDI is a score, not recorded audio"),
];

/// Classify a path by its extension.
///
/// This is deliberately extension-based rather than content-sniffing: the
/// scanner calls it once per file across a whole library, so it must not open
/// anything. A file that lies about its extension is caught later, when the
/// decoder actually probes it.
pub fn classify(path: &Path) -> Support {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return Support::NotAudio;
    };

    // Extensions are compared lowercase; `TRACK.MP3` is common on old rips.
    let ext = ext.to_ascii_lowercase();

    if SUPPORTED_EXTENSIONS.contains(&ext.as_str()) {
        return Support::Supported;
    }

    if let Some((_, reason)) = UNSUPPORTED_AUDIO.iter().find(|(e, _)| *e == ext) {
        return Support::Unsupported { reason };
    }

    Support::NotAudio
}

/// Identify what a file *actually* is by reading its header.
///
/// [`classify`] goes by extension because it runs over a whole library and must
/// not open anything. This is the slow path, used only when a file has already
/// failed to decode, so that the error can say something useful.
///
/// Mislabelled files are common in collections assembled from downloads: a
/// container saved with the wrong extension decodes fine (symphonia sniffs
/// content and treats the extension as a hint only) right up until the codec
/// inside is one this build has no decoder for. At that point the user deserves
/// better than "unsupported audio codec".
///
/// Returns a human-readable description, or `None` if the header is not
/// recognised.
pub fn sniff(path: &Path) -> Option<&'static str> {
    use std::io::Read;

    let mut header = [0u8; 64];
    let read = std::fs::File::open(path)
        .and_then(|mut f| f.read(&mut header))
        .ok()?;
    let header = &header[..read];

    if header.len() < 4 {
        return None;
    }

    // Ogg: the codec is named in the first page, after the 28-byte page header.
    if header.starts_with(b"OggS") {
        if contains(header, b"OpusHead") {
            return Some("Ogg Opus");
        }
        if contains(header, b"vorbis") {
            return Some("Ogg Vorbis");
        }
        if contains(header, b"FLAC") {
            return Some("Ogg FLAC");
        }
        return Some("an Ogg container");
    }

    if header.starts_with(b"fLaC") {
        return Some("FLAC");
    }
    if header.starts_with(b"ID3") {
        return Some("MP3");
    }
    if header.starts_with(b"RIFF") {
        return Some("a RIFF/WAV container");
    }
    if header.starts_with(b"FORM") {
        return Some("an AIFF container");
    }
    // EBML header, shared by Matroska and WebM.
    if header.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        return Some("a Matroska/WebM container");
    }
    if header.starts_with(&[0x30, 0x26, 0xb2, 0x75]) {
        return Some("Windows Media (ASF)");
    }
    if header.starts_with(b"wvpk") {
        return Some("WavPack");
    }
    if header.starts_with(b"MAC ") {
        return Some("Monkey's Audio");
    }
    // MP4/M4A place a `ftyp` box a few bytes in rather than at offset zero.
    if header.len() >= 12 && &header[4..8] == b"ftyp" {
        return Some("an MP4/M4A container");
    }
    // A raw MPEG audio frame begins with 11 set sync bits.
    if header[0] == 0xff && (header[1] & 0xe0) == 0xe0 {
        return Some("MP3");
    }

    None
}

/// Naive substring search; the haystack here is 64 bytes.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn classify_str(s: &str) -> Support {
        classify(&PathBuf::from(s))
    }

    #[test]
    fn common_formats_are_supported() {
        for name in ["a.mp3", "a.flac", "a.wav", "a.m4a", "a.ogg", "a.aiff"] {
            assert!(
                classify_str(name).is_supported(),
                "{name} should be playable"
            );
        }
    }

    #[test]
    fn extension_case_is_ignored() {
        assert!(classify_str("TRACK.MP3").is_supported());
        assert!(classify_str("Track.FlAc").is_supported());
    }

    /// The one format in the target collection symphonia cannot handle. It must
    /// report a reason rather than being silently dropped.
    #[test]
    fn opus_is_rejected_with_a_reason() {
        match classify_str("mix.opus") {
            Support::Unsupported { reason } => assert!(reason.contains("Opus")),
            other => panic!("expected an explained rejection, got {other:?}"),
        }
    }

    #[test]
    fn non_audio_is_not_audio() {
        assert_eq!(classify_str("cover.jpg"), Support::NotAudio);
        assert_eq!(classify_str("notes.txt"), Support::NotAudio);
        assert_eq!(classify_str("no_extension"), Support::NotAudio);
    }

    /// The real case from the target collection: an Opus file named `.mp3`.
    #[test]
    fn sniff_sees_through_a_wrong_extension() {
        let dir = std::env::temp_dir().join(format!("resonance-sniff-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mislabelled.mp3");

        // An Ogg page header followed by the Opus codec identifier.
        let mut bytes = b"OggS".to_vec();
        bytes.extend_from_slice(&[0u8; 24]);
        bytes.extend_from_slice(b"OpusHead");
        bytes.resize(64, 0);
        std::fs::write(&path, &bytes).unwrap();

        // Extension alone says "playable", which is exactly the blind spot.
        assert!(classify(&path).is_supported());
        // Reading the header tells the truth.
        assert_eq!(sniff(&path), Some("Ogg Opus"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sniff_returns_nothing_for_a_missing_file() {
        assert_eq!(sniff(&PathBuf::from("does-not-exist.mp3")), None);
    }
}
