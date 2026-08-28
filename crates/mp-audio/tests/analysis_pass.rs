//! The analysis pass end to end, against real files and a real index.
//!
//! The unit tests measure a buffer that was handed straight to the analyser.
//! This checks the parts they cannot: that a file on disk is decoded, measured,
//! written to the database, and taken out of the queue — and that the pass is
//! genuinely resumable and genuinely gives up on a file it cannot read.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use mp_audio::analysis;
use mp_core::library::{db, features};
use rusqlite::params;

const RATE: u32 = 44_100;

fn scratch(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("resonance-analysis-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Write a mono 16-bit WAV.
fn write_wav(path: &Path, samples: &[f32]) {
    let data_bytes = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_bytes as usize);

    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_bytes.to_le_bytes());

    for sample in samples {
        let scaled = (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16;
        out.extend_from_slice(&scaled.to_le_bytes());
    }

    std::fs::write(path, out).unwrap();
}

fn tone(freq: f32, seconds: f32) -> Vec<f32> {
    let count = (seconds * RATE as f32) as usize;
    (0..count)
        .map(|n| {
            let phase = std::f32::consts::TAU * freq * n as f32 / RATE as f32;
            phase.sin() * 0.5
        })
        .collect()
}

/// An index pointing at real files on disk.
fn index_with(files: &[(i64, PathBuf)]) -> db::Handle {
    let connection = db::open_in_memory().unwrap();

    for (id, path) in files {
        let metadata = std::fs::metadata(path).ok();
        let size = metadata.as_ref().map_or(0, |meta| meta.len() as i64);

        connection
            .execute(
                "INSERT INTO tracks (
                     id, path, folder, file_name, mtime, size,
                     title, sort_title, added_at, last_seen_at
                 ) VALUES (?1, ?2, '/m', 'x.wav', 1, ?3, 'T', 't', ?1, 0)",
                params![id, path.to_string_lossy(), size],
            )
            .unwrap();
    }

    connection
}

#[test]
fn the_pass_analyses_real_files_and_empties_the_queue() {
    let dir = scratch("basic");

    let files: Vec<(i64, PathBuf)> = (1..=3)
        .map(|id| {
            let path = dir.join(format!("{id}.wav"));
            write_wav(&path, &tone(200.0 * id as f32, 4.0));
            (id, path)
        })
        .collect();

    let connection = index_with(&files);
    let cancel = AtomicBool::new(false);

    assert_eq!(features::progress(&connection).unwrap(), (0, 3));

    let batch = analysis::run_batch(&connection, 10, &cancel).unwrap();

    assert_eq!(batch.analysed, 3, "not every file was analysed");
    assert_eq!(batch.failed, 0);
    assert_eq!(batch.remaining, 0);
    assert!(!batch.has_more());

    assert_eq!(features::progress(&connection).unwrap(), (3, 3));

    let _ = std::fs::remove_dir_all(&dir);
}

/// The results have to describe the actual audio, not just be present.
#[test]
fn what_is_stored_describes_the_file_that_was_read() {
    let dir = scratch("meaning");

    let low = dir.join("low.wav");
    let high = dir.join("high.wav");
    write_wav(&low, &tone(80.0, 4.0));
    write_wav(&high, &tone(7_000.0, 4.0));

    let connection = index_with(&[(1, low), (2, high)]);
    analysis::run_batch(&connection, 10, &AtomicBool::new(false)).unwrap();

    let low = features::get(&connection, 1).unwrap().unwrap();
    let high = features::get(&connection, 2).unwrap().unwrap();

    assert!(
        high.centroid > low.centroid + 0.3,
        "the stored features do not tell the two files apart: {:.3} against {:.3}",
        low.centroid,
        high.centroid
    );
    assert!(low.bass > high.bass);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Stopping halfway must lose nothing and pick up where it left off.
#[test]
fn the_pass_is_resumable() {
    let dir = scratch("resume");

    let files: Vec<(i64, PathBuf)> = (1..=5)
        .map(|id| {
            let path = dir.join(format!("{id}.wav"));
            write_wav(&path, &tone(300.0, 3.0));
            (id, path)
        })
        .collect();

    let connection = index_with(&files);
    let cancel = AtomicBool::new(false);

    let first = analysis::run_batch(&connection, 2, &cancel).unwrap();
    assert_eq!(first.analysed, 2);
    assert_eq!(first.remaining, 3);
    assert!(first.has_more());

    let second = analysis::run_batch(&connection, 2, &cancel).unwrap();
    assert_eq!(second.analysed, 2);
    assert_eq!(second.remaining, 1);

    let third = analysis::run_batch(&connection, 10, &cancel).unwrap();
    assert_eq!(
        third.analysed, 1,
        "the last track was analysed twice or not at all"
    );
    assert_eq!(third.remaining, 0);

    // And a further pass has nothing left to do.
    let fourth = analysis::run_batch(&connection, 10, &cancel).unwrap();
    assert_eq!(fourth.analysed, 0);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cancelling_stops_the_batch_without_losing_what_was_done() {
    let dir = scratch("cancel");

    let files: Vec<(i64, PathBuf)> = (1..=4)
        .map(|id| {
            let path = dir.join(format!("{id}.wav"));
            write_wav(&path, &tone(300.0, 3.0));
            (id, path)
        })
        .collect();

    let connection = index_with(&files);

    // Already cancelled: nothing should be attempted.
    let cancel = AtomicBool::new(true);
    let batch = analysis::run_batch(&connection, 10, &cancel).unwrap();

    assert_eq!(batch.analysed, 0);
    assert_eq!(
        batch.remaining, 4,
        "the queue was disturbed by a cancelled run"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// One unreadable file must not be retried on every pass forever.
#[test]
fn a_file_that_will_not_decode_leaves_the_queue() {
    let dir = scratch("broken");

    let good = dir.join("good.wav");
    write_wav(&good, &tone(440.0, 3.0));

    let broken = dir.join("broken.wav");
    std::fs::write(&broken, b"this is not a wav file at all").unwrap();

    let missing = dir.join("gone.wav");

    let connection = index_with(&[(1, good), (2, broken), (3, missing)]);
    let cancel = AtomicBool::new(false);

    let batch = analysis::run_batch(&connection, 10, &cancel).unwrap();

    assert_eq!(batch.analysed, 1);
    assert_eq!(batch.failed, 2);
    assert_eq!(batch.remaining, 0, "a broken file stayed in the queue");

    // A second pass finds nothing to do, which is the point.
    let again = analysis::run_batch(&connection, 10, &cancel).unwrap();
    assert_eq!(again.analysed, 0);
    assert_eq!(again.failed, 0);

    let _ = std::fs::remove_dir_all(&dir);
}

/// A re-encoded file has to be measured again.
#[test]
fn a_changed_file_comes_back_to_the_queue() {
    let dir = scratch("changed");

    let path = dir.join("a.wav");
    write_wav(&path, &tone(100.0, 3.0));

    let connection = index_with(&[(1, path.clone())]);
    let cancel = AtomicBool::new(false);

    analysis::run_batch(&connection, 10, &cancel).unwrap();
    let before = features::get(&connection, 1).unwrap().unwrap();

    // The file is replaced with different audio, and the index notices.
    write_wav(&path, &tone(6_000.0, 3.0));
    let size = std::fs::metadata(&path).unwrap().len() as i64;
    connection
        .execute(
            "UPDATE tracks SET mtime = 2, size = ?1 WHERE id = 1",
            params![size],
        )
        .unwrap();

    assert_eq!(features::progress(&connection).unwrap(), (0, 1));

    analysis::run_batch(&connection, 10, &cancel).unwrap();
    let after = features::get(&connection, 1).unwrap().unwrap();

    assert!(
        after.centroid > before.centroid + 0.3,
        "the re-analysis did not pick up the new audio: {:.3} against {:.3}",
        before.centroid,
        after.centroid
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// An empty library is not an error.
#[test]
fn an_empty_library_analyses_nothing_and_says_so() {
    let connection = db::open_in_memory().unwrap();
    let batch = analysis::run_batch(&connection, 10, &AtomicBool::new(false)).unwrap();

    assert_eq!(batch, analysis::Batch::default());
    assert!(!batch.has_more());
}
