//! Keeping a track fetched from a link: making it a file in the library.
//!
//! This is the one place Resonance puts a file into the user's music folders,
//! and it is held to rules of its own, separate from the tag editor's:
//!
//! - **Only when asked.** Nothing calls this except an explicit save.
//! - **Only new files, only in one folder.** Everything goes under
//!   [`FOLDER_NAME`] inside a watched folder the user chose, so what Resonance
//!   fetched never mingles with what the user put there themselves, and all of
//!   it can be found — or deleted — in one place.
//! - **Never over anything.** A file already at the destination is left
//!   exactly as it is, whoever put it there. Saving the same track twice is a
//!   no-op, not a second copy and not a replacement.
//! - **Never half a file.** The copy is made and tagged under a name the
//!   scanner passes over, beside where it is going, and only renamed into
//!   place once it is complete. A scan or a crash in the middle sees either
//!   nothing or the finished track.
//!
//! The audio is copied byte for byte from the cache; only the tag block of the
//! new copy is written. The cached original is not touched.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::TaggedFileExt;
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::prelude::{Accessor, ItemKey, TagExt};
use lofty::probe::Probe;

/// The folder saved tracks go into, inside the chosen watched folder.
pub const FOLDER_NAME: &str = "Resonance Downloads";

/// What a partial copy is called while it is being made.
///
/// An extension the scanner has never heard of, so a copy in progress is not
/// indexed as a track and then found to be unreadable.
const PARTIAL_SUFFIX: &str = "resonance-part";

/// The longest a file or folder name made from a title may be, in characters.
///
/// Well inside Windows' limits even several folders deep, and longer than any
/// title that is not mostly decoration.
const MAX_NAME_CHARS: usize = 120;

/// Where saved tracks go.
///
/// Inside `chosen` when that is still one of the watched folders, and inside
/// the first watched folder otherwise — a choice that names a folder since
/// removed from the library must not send files somewhere the library no
/// longer looks. `None` when there are no watched folders at all, because
/// then there is no library for a saved track to join.
pub fn downloads_dir(watched: &[PathBuf], chosen: Option<&Path>) -> Option<PathBuf> {
    let root = chosen
        .and_then(|chosen| watched.iter().find(|folder| folder.as_path() == chosen))
        .or_else(|| watched.first())?;

    Some(root.join(FOLDER_NAME))
}

/// What goes into the saved file's tags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Details {
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    /// The link the track came from, noted in the file's comment so it is
    /// always possible to tell where a download came from.
    pub source: Option<String>,
    /// The cover, as an encoded image.
    pub cover: Option<Vec<u8>>,
}

/// What saving did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kept {
    Saved(PathBuf),
    /// Something was already at the destination and was left alone.
    AlreadyThere(PathBuf),
}

impl Kept {
    pub fn path(&self) -> &Path {
        match self {
            Self::Saved(path) | Self::AlreadyThere(path) => path,
        }
    }
}

/// Where a track would be saved.
///
/// `<downloads>/<Artist> - <Title>.<ext>` for a single track, and inside a
/// folder named after the playlist when it came from one — so an album saved
/// from a link stays together, the way an album ripped from a disc would.
pub fn destination(
    downloads: &Path,
    collection: Option<&str>,
    details: &Details,
    extension: &str,
) -> PathBuf {
    let folder = match collection.map(safe_name) {
        Some(collection) => downloads.join(collection),
        None => downloads.to_path_buf(),
    };

    let stem = if details.artist.trim().is_empty() {
        safe_name(&details.title)
    } else {
        safe_name(&format!(
            "{} - {}",
            details.artist.trim(),
            details.title.trim()
        ))
    };

    folder.join(format!("{stem}.{extension}"))
}

/// Text made safe to use as one file or folder name, on Windows especially.
///
/// Characters no file system accepts become an underscore, runs of whitespace
/// become one space, and the trailing dots and spaces Windows silently strips
/// go before it can. A name Windows reserves for a device is given a leading
/// underscore rather than refused, since `Con.m4a` is a perfectly good song.
pub fn safe_name(text: &str) -> String {
    let replaced: String = text
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();

    let collapsed = replaced.split_whitespace().collect::<Vec<_>>().join(" ");
    let shortened: String = collapsed.chars().take(MAX_NAME_CHARS).collect();
    let trimmed = shortened.trim_end_matches(['.', ' ']).trim().to_owned();

    if trimmed.is_empty() {
        return "Untitled".to_owned();
    }

    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];

    let device = trimmed
        .split('.')
        .next()
        .is_some_and(|stem| RESERVED.contains(&stem.to_ascii_uppercase().as_str()));

    if device {
        format!("_{trimmed}")
    } else {
        trimmed
    }
}

/// Save `source` at `destination`, tagged.
///
/// Returns [`Kept::AlreadyThere`] without touching anything if the destination
/// exists. On failure nothing is left behind: not at the destination, and not
/// as a partial copy beside it.
pub fn keep(source: &Path, destination: &Path, details: &Details) -> Result<Kept> {
    if destination.exists() {
        return Ok(Kept::AlreadyThere(destination.to_path_buf()));
    }

    let folder = destination
        .parent()
        .context("a destination with no folder")?;
    std::fs::create_dir_all(folder).with_context(|| format!("making {}", folder.display()))?;

    let partial = partial_path(destination);

    let made = std::fs::copy(source, &partial)
        .with_context(|| format!("copying {}", source.display()))
        .and_then(|_| tag(&partial, details));

    if let Err(err) = made {
        let _ = std::fs::remove_file(&partial);
        return Err(err);
    }

    // Checked again at the last moment: a rename on Windows replaces what it
    // lands on, and something may have arrived while the copy was made.
    if destination.exists() {
        let _ = std::fs::remove_file(&partial);
        return Ok(Kept::AlreadyThere(destination.to_path_buf()));
    }

    if let Err(err) = std::fs::rename(&partial, destination) {
        let _ = std::fs::remove_file(&partial);
        return Err(err).with_context(|| format!("moving into {}", destination.display()));
    }

    Ok(Kept::Saved(destination.to_path_buf()))
}

fn partial_path(destination: &Path) -> PathBuf {
    let mut name = destination
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    name.push(".");
    name.push(PARTIAL_SUFFIX);
    destination.with_file_name(name)
}

/// Write the details into a file this module has just made.
///
/// The file is read by its contents rather than its name, because the partial
/// copy's extension is deliberately one nothing recognises.
fn tag(path: &Path, details: &Details) -> Result<()> {
    let options = ParseOptions::new()
        .read_properties(false)
        .read_tags(true)
        .read_cover_art(true);

    let probe = Probe::open(path)
        .with_context(|| format!("opening {}", path.display()))?
        .options(options)
        .guess_file_type()
        .with_context(|| format!("reading {}", path.display()))?;

    let mut tagged = probe
        .read()
        .with_context(|| format!("reading {}", path.display()))?;

    if tagged.primary_tag_mut().is_none() {
        let kind = tagged.primary_tag_type();
        tagged.insert_tag(lofty::tag::Tag::new(kind));
    }

    let tag = tagged
        .primary_tag_mut()
        .context("the file has no tag block and one could not be made")?;

    tag.set_title(details.title.trim().to_owned());

    if !details.artist.trim().is_empty() {
        tag.set_artist(details.artist.trim().to_owned());
    }

    if let Some(album) = details
        .album
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty())
    {
        tag.set_album(album.to_owned());
    }

    if let Some(source) = &details.source {
        tag.insert_text(
            ItemKey::Comment,
            format!("Saved by Resonance from {source}"),
        );
    }

    if let Some(cover) = &details.cover
        && let Some(mime) = image_type(cover)
    {
        tag.remove_picture_type(PictureType::CoverFront);
        tag.push_picture(
            Picture::unchecked(cover.clone())
                .pic_type(PictureType::CoverFront)
                .mime_type(mime)
                .build(),
        );
    }

    tag.save_to_path(path, WriteOptions::default())
        .with_context(|| format!("writing tags to {}", path.display()))
}

/// The kind of image, from its first bytes. `None` for anything that is not a
/// JPEG or a PNG, which is then simply not embedded.
fn image_type(bytes: &[u8]) -> Option<MimeType> {
    match bytes {
        [0xFF, 0xD8, 0xFF, ..] => Some(MimeType::Jpeg),
        [0x89, b'P', b'N', b'G', ..] => Some(MimeType::Png),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::tags::{self, Editable};

    /// A small but genuinely valid WAV, so lofty tags it for real.
    fn write_wav(path: &Path) {
        const FRAMES: u32 = 1_024;
        let mut samples = Vec::with_capacity(FRAMES as usize * 2);
        for index in 0..FRAMES {
            let value = (index as i32 % 4_096 - 2_048) as i16;
            samples.extend_from_slice(&value.to_le_bytes());
        }

        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + samples.len() as u32).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&44_100u32.to_le_bytes());
        out.extend_from_slice(&88_200u32.to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(samples.len() as u32).to_le_bytes());
        out.extend_from_slice(&samples);

        std::fs::write(path, out).unwrap();
    }

    fn details() -> Details {
        Details {
            title: "Together Forever".into(),
            artist: "Rick Astley".into(),
            album: Some("Whenever You Need Somebody".into()),
            source: Some("https://www.youtube.com/watch?v=yPYZpwSpKmA".into()),
            cover: None,
        }
    }

    fn value(values: &tags::Values, field: Editable) -> Option<String> {
        values
            .iter()
            .find(|(f, _)| *f == field)
            .and_then(|(_, v)| v.clone())
    }

    // -- where --------------------------------------------------------------

    #[test]
    fn downloads_go_in_the_chosen_folder() {
        let watched = vec![PathBuf::from("/music/a"), PathBuf::from("/music/b")];

        assert_eq!(
            downloads_dir(&watched, Some(Path::new("/music/b"))),
            Some(PathBuf::from("/music/b").join(FOLDER_NAME))
        );
    }

    #[test]
    fn with_no_choice_downloads_go_in_the_first_folder() {
        let watched = vec![PathBuf::from("/music/a"), PathBuf::from("/music/b")];

        assert_eq!(
            downloads_dir(&watched, None),
            Some(PathBuf::from("/music/a").join(FOLDER_NAME))
        );
    }

    /// A folder removed from the library since it was chosen must not keep
    /// receiving files the library will never see.
    #[test]
    fn a_choice_that_is_no_longer_watched_is_not_used() {
        let watched = vec![PathBuf::from("/music/a")];

        assert_eq!(
            downloads_dir(&watched, Some(Path::new("/elsewhere"))),
            Some(PathBuf::from("/music/a").join(FOLDER_NAME))
        );
    }

    #[test]
    fn with_no_library_there_is_nowhere_to_save() {
        assert_eq!(downloads_dir(&[], Some(Path::new("/music"))), None);
    }

    #[test]
    fn a_single_track_is_named_artist_then_title() {
        let path = destination(Path::new("/dl"), None, &details(), "m4a");

        assert_eq!(
            path,
            Path::new("/dl").join("Rick Astley - Together Forever.m4a")
        );
    }

    #[test]
    fn a_track_from_a_playlist_goes_in_the_playlists_folder() {
        let path = destination(Path::new("/dl"), Some("Road trip: 2026"), &details(), "m4a");

        assert_eq!(
            path,
            Path::new("/dl")
                .join("Road trip_ 2026")
                .join("Rick Astley - Together Forever.m4a")
        );
    }

    #[test]
    fn a_track_with_no_artist_is_named_by_its_title() {
        let mut details = details();
        details.artist = String::new();

        let path = destination(Path::new("/dl"), None, &details, "m4a");

        assert_eq!(path, Path::new("/dl").join("Together Forever.m4a"));
    }

    #[test]
    fn names_are_made_safe() {
        assert_eq!(safe_name("AC/DC: Back in Black?"), "AC_DC_ Back in Black_");
        assert_eq!(safe_name("  lots   of\tspace  "), "lots of space");
        assert_eq!(safe_name("ends with dots..."), "ends with dots");
        assert_eq!(safe_name(""), "Untitled");
        assert_eq!(safe_name("..."), "Untitled");
        assert_eq!(safe_name("con"), "_con");
        assert_eq!(safe_name("Con.m4a"), "_Con.m4a");
        assert_eq!(safe_name("Contact"), "Contact");
        assert_eq!(safe_name(&"x".repeat(500)).chars().count(), MAX_NAME_CHARS);
    }

    // -- the file -------------------------------------------------------------

    #[test]
    fn a_saved_track_is_a_tagged_copy() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("cache.wav");
        write_wav(&source);
        let before = std::fs::read(&source).unwrap();
        let destination = dir
            .path()
            .join("dl")
            .join("Rick Astley - Together Forever.wav");

        let kept = keep(&source, &destination, &details()).unwrap();

        assert_eq!(kept, Kept::Saved(destination.clone()));
        let values = tags::read(&destination).unwrap();
        assert_eq!(
            value(&values, Editable::Title).as_deref(),
            Some("Together Forever")
        );
        assert_eq!(
            value(&values, Editable::Artist).as_deref(),
            Some("Rick Astley")
        );
        assert_eq!(
            value(&values, Editable::Album).as_deref(),
            Some("Whenever You Need Somebody")
        );

        // The cached original is exactly as it was.
        assert_eq!(std::fs::read(&source).unwrap(), before);
    }

    #[test]
    fn nothing_is_left_beside_a_saved_track() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("cache.wav");
        write_wav(&source);
        let destination = dir.path().join("dl").join("track.wav");

        keep(&source, &destination, &details()).unwrap();

        let names: Vec<_> = std::fs::read_dir(dir.path().join("dl"))
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name())
            .collect();
        assert_eq!(names, ["track.wav"]);
    }

    /// Whoever put it there, a file already at the destination is not
    /// replaced, and not touched.
    #[test]
    fn an_existing_file_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("cache.wav");
        write_wav(&source);
        let destination = dir.path().join("mine.wav");
        std::fs::write(&destination, b"the user's own file").unwrap();

        let kept = keep(&source, &destination, &details()).unwrap();

        assert_eq!(kept, Kept::AlreadyThere(destination.clone()));
        assert_eq!(std::fs::read(&destination).unwrap(), b"the user's own file");
    }

    #[test]
    fn a_failed_save_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("dl").join("track.wav");

        assert!(keep(&dir.path().join("missing.wav"), &destination, &details()).is_err());

        assert!(!destination.exists());
        assert!(!partial_path(&destination).exists());
    }

    /// Something that is not audio cannot be tagged. It must not end up in the
    /// library anyway, as an unreadable track.
    #[test]
    fn a_file_that_cannot_be_tagged_is_not_saved() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("cache.wav");
        std::fs::write(&source, b"this is not audio").unwrap();
        let destination = dir.path().join("dl").join("track.wav");

        assert!(keep(&source, &destination, &details()).is_err());

        assert!(!destination.exists());
        assert!(!partial_path(&destination).exists());
    }

    #[test]
    fn the_cover_and_the_source_go_in_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("cache.wav");
        write_wav(&source);
        let destination = dir.path().join("track.wav");

        let mut details = details();
        details.cover = Some(vec![0xFF, 0xD8, 0xFF, 0xE0, 0, 16, b'J', b'F', b'I', b'F']);

        keep(&source, &destination, &details).unwrap();

        let tagged = Probe::open(&destination).unwrap().read().unwrap();
        let tag = tagged.primary_tag().unwrap();
        assert_eq!(tag.pictures().len(), 1);
        assert_eq!(tag.pictures()[0].pic_type(), PictureType::CoverFront);
        assert!(
            tag.get_string(ItemKey::Comment)
                .is_some_and(|comment| comment.contains("yPYZpwSpKmA")),
            "{:?}",
            tag.get_string(ItemKey::Comment)
        );
    }

    #[test]
    fn an_image_that_is_neither_jpeg_nor_png_is_left_out() {
        assert_eq!(image_type(b"GIF89a"), None);
        assert_eq!(image_type(&[0xFF, 0xD8, 0xFF, 0xE0]), Some(MimeType::Jpeg));
        assert_eq!(image_type(b"\x89PNG\r\n"), Some(MimeType::Png));
    }
}
