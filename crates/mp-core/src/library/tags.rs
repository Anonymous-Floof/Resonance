//! Reading and writing the tags inside a music file.
//!
//! This is the only code in Resonance that writes to the user's music, and it
//! is built to be boring about it.
//!
//! The rules it enforces, none of which the caller can opt out of:
//!
//! - **Only tags.** The file is never renamed, moved, re-encoded or
//!   re-containerised. The audio stream is untouched.
//! - **Only named fields.** [`Editable`] is a closed list. Anything else in
//!   the file — embedded art, ReplayGain, comments, custom frames — is read
//!   back and written out unchanged, because the whole tag object is loaded,
//!   modified and saved rather than rebuilt.
//! - **Nothing without a record.** Every write returns the exact before-and-
//!   after of each field it changed, so the caller can journal it *before*
//!   committing. A write that cannot be described cannot happen.
//! - **No-ops do nothing.** Setting a field to what it already is does not
//!   touch the file at all, so an accidental Save leaves the mtime alone and
//!   the scanner never re-reads it.
//!
//! ## What undo can and cannot promise
//!
//! Reverting restores the *values* of the fields that were changed. It does
//! not promise a byte-identical file: writing a tag hands the whole block to
//! lofty, which may reorder frames, change padding, or upgrade an ID3v2.3 tag
//! it had to parse. The audio is bit-identical; the tag block may not be. That
//! is worth being clear about rather than implying more than is delivered.

use std::path::Path;

use anyhow::{Context, Result, bail};
use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::TaggedFileExt;
use lofty::prelude::{ItemKey, TagExt};
use lofty::probe::Probe;
use serde::{Deserialize, Serialize};

/// The fields the editor is allowed to touch.
///
/// A closed list on purpose. Everything the interface can change is here, and
/// anything not here cannot be changed by any path through this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Editable {
    Title,
    Artist,
    AlbumArtist,
    Album,
    Genre,
    Year,
    TrackNumber,
    DiscNumber,
}

impl Editable {
    pub const ALL: [Self; 8] = [
        Self::Title,
        Self::Artist,
        Self::AlbumArtist,
        Self::Album,
        Self::Genre,
        Self::Year,
        Self::TrackNumber,
        Self::DiscNumber,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Title => "Title",
            Self::Artist => "Artist",
            Self::AlbumArtist => "Album artist",
            Self::Album => "Album",
            Self::Genre => "Genre",
            Self::Year => "Year",
            Self::TrackNumber => "Track number",
            Self::DiscNumber => "Disc number",
        }
    }

    /// Whether the field only accepts a number.
    pub fn is_numeric(self) -> bool {
        matches!(self, Self::Year | Self::TrackNumber | Self::DiscNumber)
    }

    /// The tag keys this field can live under, best first.
    ///
    /// Year needs a list rather than a single key. ID3v2.4 retired `TYER` in
    /// favour of the combined recording-date frame, so writing `Year` to a
    /// modern MP3 tag is silently refused and reading it back finds nothing.
    /// Trying the alternatives in order is what makes the field work across
    /// ID3v2.3, ID3v2.4, Vorbis comments and MP4 atoms alike — and it mirrors
    /// what the scanner already does when it reads a year out of a file.
    fn keys(self) -> &'static [ItemKey] {
        match self {
            Self::Title => &[ItemKey::TrackTitle],
            Self::Artist => &[ItemKey::TrackArtist],
            Self::AlbumArtist => &[ItemKey::AlbumArtist],
            Self::Album => &[ItemKey::AlbumTitle],
            Self::Genre => &[ItemKey::Genre],
            Self::Year => &[
                ItemKey::Year,
                ItemKey::RecordingDate,
                ItemKey::OriginalReleaseDate,
            ],
            Self::TrackNumber => &[ItemKey::TrackNumber],
            Self::DiscNumber => &[ItemKey::DiscNumber],
        }
    }
}

/// The current value of every editable field.
pub type Values = Vec<(Editable, Option<String>)>;

/// One field's move from one value to another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub field: Editable,
    /// `None` where the field was absent, which is different from empty.
    pub before: Option<String>,
    pub after: Option<String>,
}

/// A requested edit: only the fields present are touched.
///
/// `Some(None)` clears a field; a field that is absent from the list is left
/// exactly as it is, including any oddities the file already carries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Edit {
    pub fields: Vec<(Editable, Option<String>)>,
}

impl Edit {
    pub fn set(mut self, field: Editable, value: impl Into<String>) -> Self {
        let value = value.into();
        let value = (!value.trim().is_empty()).then(|| value.trim().to_owned());
        self.fields.push((field, value));
        self
    }

    pub fn clear(mut self, field: Editable) -> Self {
        self.fields.push((field, None));
        self
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}

/// Read every editable field out of a file.
pub fn read(path: &Path) -> Result<Values> {
    let tagged = open(path)?;

    let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) else {
        // No tag block at all. Every field is absent, which is a perfectly
        // good answer and lets the editor open on an untagged file.
        return Ok(Editable::ALL.iter().map(|field| (*field, None)).collect());
    };

    Ok(Editable::ALL
        .iter()
        .map(|field| {
            let value = field.keys().iter().find_map(|key| {
                tag.get_string(*key)
                    .map(str::to_owned)
                    .filter(|value| !value.trim().is_empty())
            });
            (*field, value)
        })
        .collect())
}

/// Work out what an edit would change, without touching the file.
///
/// The preview the confirmation step shows. It is the same computation the
/// write performs, so what the user confirms is what happens.
pub fn preview(path: &Path, edit: &Edit) -> Result<Vec<Change>> {
    let current = read(path)?;
    Ok(diff(&current, edit))
}

/// Apply an edit, returning exactly what changed.
///
/// Returns an empty list — and writes nothing — when the edit asks for values
/// the file already has.
pub fn write(path: &Path, edit: &Edit) -> Result<Vec<Change>> {
    if edit.is_empty() {
        return Ok(Vec::new());
    }

    let changes = preview(path, edit)?;
    if changes.is_empty() {
        return Ok(changes);
    }

    apply(path, &changes)?;
    Ok(changes)
}

/// Put a set of changes back the way they were.
///
/// Takes the same `Change` list the write produced and applies it backwards.
/// Refuses if the file no longer holds the values the edit left behind, since
/// that means something else has edited it since and reverting would throw
/// away a change this journal knows nothing about.
pub fn revert(path: &Path, changes: &[Change]) -> Result<()> {
    let current = read(path)?;

    for change in changes {
        let actual = current
            .iter()
            .find(|(field, _)| *field == change.field)
            .and_then(|(_, value)| value.clone());

        if actual != change.after {
            bail!(
                "{} has changed since that edit (expected {:?}, found {:?}), \
                 so reverting would discard the newer change",
                change.field.label(),
                change.after.as_deref().unwrap_or("nothing"),
                actual.as_deref().unwrap_or("nothing")
            );
        }
    }

    let backwards: Vec<Change> = changes
        .iter()
        .map(|change| Change {
            field: change.field,
            before: change.after.clone(),
            after: change.before.clone(),
        })
        .collect();

    apply(path, &backwards)
}

/// The one place a file is written.
///
/// Everything above funnels through here so there is a single point to audit,
/// and so the "only tags, never the container" property is enforced once
/// rather than at every call site.
fn apply(path: &Path, changes: &[Change]) -> Result<()> {
    let mut tagged = open(path)?;

    // A file with no tag block yet needs one before anything can be set. The
    // format's own preferred tag type is used, so an MP3 gets ID3v2 and a
    // FLAC gets Vorbis comments rather than something the file type merely
    // tolerates.
    if tagged.primary_tag_mut().is_none() {
        let kind = tagged.primary_tag_type();
        tagged.insert_tag(lofty::tag::Tag::new(kind));
    }

    let tag = tagged
        .primary_tag_mut()
        .context("the file has no tag block and one could not be created")?;

    for change in changes {
        // Cleared either way. Setting a value has to remove the alternatives
        // too, or a year written as a recording date would sit behind the new
        // one and reappear the moment the new key is dropped.
        for key in change.field.keys() {
            tag.remove_key(*key);
        }

        if let Some(value) = &change.after {
            let keys = change.field.keys();

            // The first key this tag format actually accepts. `insert_text`
            // answers false for a key the format has no frame for, which is
            // exactly the ID3v2.4 year case.
            let written = keys.iter().any(|key| tag.insert_text(*key, value.clone()));

            if !written {
                bail!(
                    "this file's tag format has nowhere to store {}",
                    change.field.label()
                );
            }
        }
    }

    // `save_to_path` rewrites the tag block in place. The audio stream is not
    // decoded, re-encoded or moved.
    tag.save_to_path(path, WriteOptions::default())
        .with_context(|| format!("writing tags to {}", path.display()))
}

fn open(path: &Path) -> Result<lofty::file::TaggedFile> {
    let options = ParseOptions::new()
        .read_properties(false)
        .read_tags(true)
        // The cover has to be read, or saving would drop it: lofty writes back
        // the tag object it parsed, and a tag parsed without its pictures has
        // no pictures to write.
        .read_cover_art(true);

    Probe::open(path)
        .and_then(|probe| probe.options(options).read())
        .with_context(|| format!("reading tags from {}", path.display()))
}

/// Which of the requested fields actually differ from what is there.
fn diff(current: &Values, edit: &Edit) -> Vec<Change> {
    let mut changes = Vec::new();

    for (field, wanted) in &edit.fields {
        let before = current
            .iter()
            .find(|(existing, _)| existing == field)
            .and_then(|(_, value)| value.clone());

        // Trimmed on both sides, so re-saving a value that only differs by
        // trailing whitespace is not treated as an edit.
        let wanted = wanted
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);

        if before.as_deref().map(str::trim) == wanted.as_deref() {
            continue;
        }

        changes.push(Change {
            field: *field,
            before,
            after: wanted,
        });
    }

    changes
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real MP3 is not something a unit test can conjure, so the pure
    /// decision-making — what counts as a change, what a revert would do — is
    /// tested directly. Writing to an actual file is covered by the
    /// `tag_editor` example against a copy of a real track.
    fn values(pairs: &[(Editable, Option<&str>)]) -> Values {
        Editable::ALL
            .iter()
            .map(|field| {
                let value = pairs
                    .iter()
                    .find(|(other, _)| other == field)
                    .and_then(|(_, value)| value.map(str::to_owned));
                (*field, value)
            })
            .collect()
    }

    #[test]
    fn setting_a_field_to_what_it_already_is_changes_nothing() {
        let current = values(&[(Editable::Title, Some("A Song"))]);
        let edit = Edit::default().set(Editable::Title, "A Song");

        assert!(
            diff(&current, &edit).is_empty(),
            "an unchanged value must not become a write"
        );
    }

    /// Because a no-op write would still bump the mtime and make the scanner
    /// re-read the file for nothing.
    #[test]
    fn surrounding_whitespace_is_not_a_change() {
        let current = values(&[(Editable::Artist, Some("Someone"))]);
        let edit = Edit::default().set(Editable::Artist, "  Someone  ");

        assert!(diff(&current, &edit).is_empty());
    }

    #[test]
    fn a_real_change_is_reported_with_both_sides() {
        let current = values(&[(Editable::Title, Some("Wrong"))]);
        let edit = Edit::default().set(Editable::Title, "Right");

        let changes = diff(&current, &edit);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].field, Editable::Title);
        assert_eq!(changes[0].before.as_deref(), Some("Wrong"));
        assert_eq!(changes[0].after.as_deref(), Some("Right"));
    }

    /// An absent field and an empty one are the same thing to the user, and
    /// treating them differently would produce phantom edits on every save.
    #[test]
    fn filling_in_a_missing_field_is_a_change_and_blanking_it_again_is_too() {
        let empty = values(&[]);

        let fill = diff(&empty, &Edit::default().set(Editable::Album, "An Album"));
        assert_eq!(fill.len(), 1);
        assert_eq!(fill[0].before, None);

        let current = values(&[(Editable::Album, Some("An Album"))]);
        let clear = diff(&current, &Edit::default().clear(Editable::Album));
        assert_eq!(clear.len(), 1);
        assert_eq!(clear[0].after, None);
    }

    #[test]
    fn setting_a_field_to_blank_is_the_same_as_clearing_it() {
        let current = values(&[(Editable::Genre, Some("Rock"))]);

        let blanked = diff(&current, &Edit::default().set(Editable::Genre, "   "));
        assert_eq!(blanked.len(), 1);
        assert_eq!(blanked[0].after, None);
    }

    #[test]
    fn clearing_a_field_that_is_already_absent_changes_nothing() {
        let empty = values(&[]);
        assert!(diff(&empty, &Edit::default().clear(Editable::Year)).is_empty());
    }

    /// Fields not named in the edit must be left completely alone.
    #[test]
    fn untouched_fields_are_never_written() {
        let current = values(&[
            (Editable::Title, Some("Keep")),
            (Editable::Artist, Some("Keep")),
            (Editable::Album, Some("Change")),
        ]);

        let changes = diff(&current, &Edit::default().set(Editable::Album, "Changed"));

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].field, Editable::Album);
    }

    #[test]
    fn every_editable_field_has_a_label_and_a_distinct_key() {
        let mut seen = std::collections::HashSet::new();

        for field in Editable::ALL {
            assert!(!field.label().is_empty(), "{field:?} needs a label");

            let keys = field.keys();
            assert!(!keys.is_empty(), "{field:?} has nowhere to be stored");
            assert!(seen.insert(keys[0]), "{field:?} reuses a tag key");
        }
    }

    #[test]
    fn the_numeric_fields_are_the_ones_you_would_expect() {
        assert!(Editable::Year.is_numeric());
        assert!(Editable::TrackNumber.is_numeric());
        assert!(Editable::DiscNumber.is_numeric());
        assert!(!Editable::Title.is_numeric());
        assert!(!Editable::Genre.is_numeric());
    }

    #[test]
    fn a_change_survives_a_round_trip_through_json() {
        let change = Change {
            field: Editable::AlbumArtist,
            before: None,
            after: Some("Various Artists".into()),
        };

        let text = serde_json::to_string(&change).unwrap();
        let back: Change = serde_json::from_str(&text).unwrap();

        assert_eq!(change, back);
    }

    /// The journal stores these, so every field name has to be stable across
    /// versions or an old entry cannot be reverted.
    #[test]
    fn field_names_serialise_as_stable_snake_case() {
        assert_eq!(
            serde_json::to_string(&Editable::AlbumArtist).unwrap(),
            "\"album_artist\""
        );
        assert_eq!(
            serde_json::to_string(&Editable::TrackNumber).unwrap(),
            "\"track_number\""
        );
    }
}
