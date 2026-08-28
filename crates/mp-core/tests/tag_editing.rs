//! End-to-end checks on the one part of Resonance that writes to the user's
//! music.
//!
//! These use a real audio file — a small WAV built here rather than committed
//! as a fixture — because the properties that matter cannot be tested against
//! a mock. "Does the audio survive a tag edit" and "does undo actually put it
//! back" are questions about a file on disk.
//!
//! The strongest guarantee this suite pins down is that **the audio is
//! untouched**. The tag block itself is rewritten by lofty and is not promised
//! to come back byte-identical — frame ordering and padding are its business —
//! but not one sample of the music may change.

use std::path::{Path, PathBuf};

use mp_core::library::tags::{self, Editable};
use mp_core::library::{Library, Progress, ScanOptions};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "resonance-tagedit-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A minimal but genuinely valid WAV, so lofty parses and tags it for real.
///
/// The samples are a recognisable ramp rather than silence, so a corrupted
/// audio chunk shows up as a difference rather than as more zeroes.
fn write_wav(path: &Path) {
    const SAMPLE_RATE: u32 = 44_100;
    const CHANNELS: u16 = 1;
    const BITS: u16 = 16;
    const FRAMES: u32 = 2_048;

    let mut samples = Vec::with_capacity(FRAMES as usize * 2);
    for index in 0..FRAMES {
        let value = (index as i32 % 8_192 - 4_096) as i16;
        samples.extend_from_slice(&value.to_le_bytes());
    }

    let block_align = CHANNELS * BITS / 8;
    let byte_rate = SAMPLE_RATE * u32::from(block_align);

    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + samples.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");

    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&CHANNELS.to_le_bytes());
    out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&BITS.to_le_bytes());

    out.extend_from_slice(b"data");
    out.extend_from_slice(&(samples.len() as u32).to_le_bytes());
    out.extend_from_slice(&samples);

    std::fs::write(path, out).unwrap();
}

/// Pull the `data` chunk back out, so the audio can be compared across edits.
fn audio_of(path: &Path) -> Vec<u8> {
    let bytes = std::fs::read(path).unwrap();

    let mut cursor = 12; // past "RIFF" <size> "WAVE"
    while cursor + 8 <= bytes.len() {
        let id = &bytes[cursor..cursor + 4];
        let size = u32::from_le_bytes(bytes[cursor + 4..cursor + 8].try_into().unwrap()) as usize;
        let body = cursor + 8;

        if id == b"data" {
            return bytes[body..(body + size).min(bytes.len())].to_vec();
        }

        // Chunks are word-aligned.
        cursor = body + size + (size % 2);
    }

    panic!("no data chunk in {}", path.display());
}

fn value_of(path: &Path, field: Editable) -> Option<String> {
    tags::read(path)
        .unwrap()
        .into_iter()
        .find(|(other, _)| *other == field)
        .and_then(|(_, value)| value)
}

/// The single most important property in the whole feature.
#[test]
fn editing_a_tag_does_not_touch_one_sample_of_audio() {
    let dir = scratch("audio-intact");
    let file = dir.join("song.wav");
    write_wav(&file);

    let before = audio_of(&file);

    tags::write(
        &file,
        &tags::Edit::default()
            .set(Editable::Title, "A New Title")
            .set(Editable::Artist, "A New Artist")
            .set(Editable::Album, "A New Album"),
    )
    .unwrap();

    let after = audio_of(&file);

    assert_eq!(before.len(), after.len(), "the audio changed length");
    assert_eq!(before, after, "the audio was modified by a tag edit");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_written_tag_can_be_read_back() {
    let dir = scratch("roundtrip");
    let file = dir.join("song.wav");
    write_wav(&file);

    let changes = tags::write(
        &file,
        &tags::Edit::default()
            .set(Editable::Title, "Read Me Back")
            .set(Editable::Artist, "Someone")
            .set(Editable::Year, "1997"),
    )
    .unwrap();

    assert_eq!(changes.len(), 3);
    assert_eq!(
        value_of(&file, Editable::Title).as_deref(),
        Some("Read Me Back")
    );
    assert_eq!(
        value_of(&file, Editable::Artist).as_deref(),
        Some("Someone")
    );
    assert_eq!(value_of(&file, Editable::Year).as_deref(), Some("1997"));

    let _ = std::fs::remove_dir_all(&dir);
}

/// Writing the values a file already has must not touch it at all — not the
/// bytes, and not the modification time the scanner watches.
#[test]
fn a_no_op_edit_does_not_write_the_file() {
    let dir = scratch("noop");
    let file = dir.join("song.wav");
    write_wav(&file);

    tags::write(&file, &tags::Edit::default().set(Editable::Title, "Fixed")).unwrap();

    let before = std::fs::read(&file).unwrap();
    let mtime = std::fs::metadata(&file).unwrap().modified().unwrap();

    let changes = tags::write(&file, &tags::Edit::default().set(Editable::Title, "Fixed")).unwrap();

    assert!(changes.is_empty(), "nothing should have been reported");
    assert_eq!(
        std::fs::read(&file).unwrap(),
        before,
        "the file was rewritten"
    );
    assert_eq!(
        std::fs::metadata(&file).unwrap().modified().unwrap(),
        mtime,
        "the modification time moved, which would make the scanner re-read it"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The promise the undo journal makes: the values come back.
#[test]
fn reverting_restores_every_field_it_changed() {
    let dir = scratch("revert");
    let file = dir.join("song.wav");
    write_wav(&file);

    // Give it something to be restored *to*, including a field that will be
    // cleared and one that starts absent.
    tags::write(
        &file,
        &tags::Edit::default()
            .set(Editable::Title, "Original Title")
            .set(Editable::Genre, "Original Genre"),
    )
    .unwrap();

    let audio = audio_of(&file);

    let changes = tags::write(
        &file,
        &tags::Edit::default()
            .set(Editable::Title, "Edited Title")
            .clear(Editable::Genre)
            .set(Editable::Album, "Added Album"),
    )
    .unwrap();

    assert_eq!(changes.len(), 3);
    assert_eq!(
        value_of(&file, Editable::Title).as_deref(),
        Some("Edited Title")
    );
    assert_eq!(value_of(&file, Editable::Genre), None);
    assert_eq!(
        value_of(&file, Editable::Album).as_deref(),
        Some("Added Album")
    );

    tags::revert(&file, &changes).unwrap();

    assert_eq!(
        value_of(&file, Editable::Title).as_deref(),
        Some("Original Title"),
        "an overwritten field was not restored"
    );
    assert_eq!(
        value_of(&file, Editable::Genre).as_deref(),
        Some("Original Genre"),
        "a cleared field was not restored"
    );
    assert_eq!(
        value_of(&file, Editable::Album),
        None,
        "a field that was added should be gone again"
    );

    assert_eq!(audio_of(&file), audio, "the audio changed during undo");

    let _ = std::fs::remove_dir_all(&dir);
}

/// If something else has edited the file since, undo would throw that away.
#[test]
fn reverting_refuses_when_the_file_has_moved_on() {
    let dir = scratch("stale");
    let file = dir.join("song.wav");
    write_wav(&file);

    let changes = tags::write(&file, &tags::Edit::default().set(Editable::Title, "First")).unwrap();

    // Something else — another program, or a later edit — changes it again.
    tags::write(&file, &tags::Edit::default().set(Editable::Title, "Second")).unwrap();

    let refused = tags::revert(&file, &changes);

    assert!(refused.is_err(), "reverting should have been refused");
    assert_eq!(
        value_of(&file, Editable::Title).as_deref(),
        Some("Second"),
        "the newer value must be left alone"
    );

    let message = refused.unwrap_err().to_string();
    assert!(
        message.contains("changed since"),
        "the refusal should say why: {message}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Fields the edit never mentions must survive it untouched.
#[test]
fn an_edit_leaves_the_fields_it_did_not_name_alone() {
    let dir = scratch("untouched");
    let file = dir.join("song.wav");
    write_wav(&file);

    tags::write(
        &file,
        &tags::Edit::default()
            .set(Editable::Title, "Keep This")
            .set(Editable::Artist, "And This")
            .set(Editable::Genre, "And This Too"),
    )
    .unwrap();

    tags::write(
        &file,
        &tags::Edit::default().set(Editable::Album, "Only This"),
    )
    .unwrap();

    assert_eq!(
        value_of(&file, Editable::Title).as_deref(),
        Some("Keep This")
    );
    assert_eq!(
        value_of(&file, Editable::Artist).as_deref(),
        Some("And This")
    );
    assert_eq!(
        value_of(&file, Editable::Genre).as_deref(),
        Some("And This Too")
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Through the library, with the journal
// ---------------------------------------------------------------------------

fn library_with(dir: &Path) -> Library {
    let mut library = Library::in_memory().unwrap();
    let options = ScanOptions {
        roots: vec![dir.to_path_buf()],
        min_duration: std::time::Duration::ZERO,
        extract_art: false,
        ..ScanOptions::default()
    };
    library.scan_blocking(&options, &Progress::new()).unwrap();
    library
}

#[test]
fn an_edit_through_the_library_is_journalled_and_undoable() {
    let dir = scratch("journal");
    let file = dir.join("song.wav");
    write_wav(&file);

    let mut library = library_with(&dir);
    let id = library
        .id_for_path(&file)
        .unwrap()
        .expect("the scan found it");

    let record = library
        .edit_tags(
            id,
            &tags::Edit::default().set(Editable::Title, "Journalled"),
        )
        .unwrap()
        .expect("a real change should produce a record");

    assert_eq!(record.changes.len(), 1);
    assert!(!record.is_reverted());
    assert_eq!(
        value_of(&file, Editable::Title).as_deref(),
        Some("Journalled")
    );

    let history = library.tag_history(10).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].id, record.id);
    assert!(history[0].summary().contains("Title"));

    library.revert_tag_edit(record.id).unwrap();

    assert_ne!(
        value_of(&file, Editable::Title).as_deref(),
        Some("Journalled"),
        "the edit was not undone on disk"
    );

    let history = library.tag_history(10).unwrap();
    assert!(
        history[0].is_reverted(),
        "the journal should record the undo"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn undoing_the_same_edit_twice_is_refused() {
    let dir = scratch("double-undo");
    let file = dir.join("song.wav");
    write_wav(&file);

    let mut library = library_with(&dir);
    let id = library.id_for_path(&file).unwrap().unwrap();

    let record = library
        .edit_tags(id, &tags::Edit::default().set(Editable::Artist, "Once"))
        .unwrap()
        .unwrap();

    library.revert_tag_edit(record.id).unwrap();
    let second = library.revert_tag_edit(record.id);

    assert!(second.is_err(), "the second undo should have been refused");

    let _ = std::fs::remove_dir_all(&dir);
}

/// An edit that changes nothing must not leave a row in the history, or the
/// journal fills up with entries that did not happen.
#[test]
fn a_no_op_edit_is_not_journalled() {
    let dir = scratch("journal-noop");
    let file = dir.join("song.wav");
    write_wav(&file);

    let mut library = library_with(&dir);
    let id = library.id_for_path(&file).unwrap().unwrap();

    library
        .edit_tags(id, &tags::Edit::default().set(Editable::Title, "Same"))
        .unwrap()
        .expect("the first one is a real change");

    let repeat = library
        .edit_tags(id, &tags::Edit::default().set(Editable::Title, "Same"))
        .unwrap();

    assert!(repeat.is_none(), "nothing changed, so nothing happened");
    assert_eq!(
        library.tag_history(10).unwrap().len(),
        1,
        "the no-op should not be in the history"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The preview is what the confirmation step shows, so it has to agree with
/// what the write then does.
#[test]
fn the_preview_matches_what_the_write_reports() {
    let dir = scratch("preview");
    let file = dir.join("song.wav");
    write_wav(&file);

    let mut library = library_with(&dir);
    let id = library.id_for_path(&file).unwrap().unwrap();

    let edit = tags::Edit::default()
        .set(Editable::Title, "Shown")
        .set(Editable::Album, "Also Shown");

    let preview = library.preview_tag_edit(id, &edit).unwrap();
    let record = library.edit_tags(id, &edit).unwrap().unwrap();

    assert_eq!(preview, record.changes, "the user was shown something else");

    let _ = std::fs::remove_dir_all(&dir);
}
