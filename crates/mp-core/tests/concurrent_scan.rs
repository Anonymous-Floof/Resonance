//! The threading claim the UI depends on, tested rather than assumed.
//!
//! `LibraryState` reads from one connection on the UI thread while a scan
//! writes through a second connection on a worker thread. That is only safe
//! because the database is in WAL mode with a busy timeout; get either wrong
//! and the symptom is a UI that freezes for seconds at a time partway through a
//! scan, which is miserable to diagnose after the fact and trivial to catch
//! here.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mp_core::library::{Filter, Library, Order, Progress, ScanOptions};

/// Longest a single read is allowed to take while a scan is running.
///
/// Generous — the point is to catch blocking on the writer's transaction, which
/// shows up as hundreds of milliseconds or a hard stall, not as jitter.
const MAX_READ: Duration = Duration::from_millis(750);

/// A scratch tree that removes itself when the guard is dropped.
fn fixture(name: &str, count: usize) -> tempfile::TempDir {
    let guard = tempfile::Builder::new()
        .prefix(&format!("resonance-concurrent-{name}-"))
        .tempdir()
        .unwrap();
    let root = guard.path();

    for index in 0..count {
        // Spread across folders so the folder view has something to group.
        let folder = root.join(format!("disc-{}", index % 8));
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join(format!("Artist {} - Track {index}.mp3", index % 40)),
            b"not decodable, but indexable",
        )
        .unwrap();
    }

    guard
}

fn options(root: &Path) -> ScanOptions {
    ScanOptions {
        roots: vec![root.to_path_buf()],
        min_duration: Duration::ZERO,
        extract_art: false,
        ..ScanOptions::default()
    }
}

#[test]
fn the_ui_can_keep_reading_while_a_scan_writes() {
    let scratch_root = fixture("reads", 1_200);
    let root = scratch_root.path();
    // Its own guard rather than a sibling path built by hand: the sibling was
    // never anybody's to delete, so nobody did.
    let scratch_db = tempfile::Builder::new()
        .prefix("resonance-concurrent-db-")
        .tempdir()
        .unwrap();
    let dir = scratch_db.path();

    let library = Library::open_at(dir.join("library.db"), dir.join("art")).unwrap();

    let scanner = library
        .detached_scanner(options(root))
        .expect("a file-backed library must be able to detach a scanner");

    let progress = Arc::new(Progress::new());
    let worker_progress = Arc::clone(&progress);

    let handle = std::thread::spawn(move || scanner.run(&worker_progress));

    // Hammer the read side the way a UI would, for as long as the scan runs.
    let mut reads = 0_u32;
    let mut slowest = Duration::ZERO;

    while !handle.is_finished() {
        for _ in 0..8 {
            let started = Instant::now();

            let _ = library.stats().expect("stats must not fail mid-scan");
            let _ = library
                .tracks(&Filter::All, Order::Title, false)
                .expect("a track query must not fail mid-scan");
            let _ = library.artists().expect("artists must not fail mid-scan");
            let _ = library
                .albums(None, 1)
                .expect("albums must not fail mid-scan");
            let _ = library.folders().expect("folders must not fail mid-scan");
            let _ = library
                .search("track", Some(50))
                .expect("search must not fail");

            slowest = slowest.max(started.elapsed());
            reads += 1;
        }
    }

    let summary = handle
        .join()
        .expect("the scan thread must not panic")
        .expect("the scan must not fail");

    assert!(reads > 0, "the test never actually read anything");
    assert!(
        slowest < MAX_READ,
        "a read blocked for {slowest:?} while the scan was writing \
         (limit {MAX_READ:?}); the UI would visibly freeze"
    );
    assert_eq!(summary.added, 1_200);

    // And the reader sees the writer's committed work without reopening.
    let stats = library.stats().unwrap();
    assert_eq!(
        stats.tracks, 1_200,
        "the reading connection must see the scan's commit"
    );

    let _ = std::fs::remove_dir_all(root);
    let _ = std::fs::remove_dir_all(dir);
}

/// Cancelling has to actually stop the worker, not merely set a flag that
/// nothing reads until the scan would have finished anyway.
#[test]
fn a_cancelled_scan_stops_promptly() {
    let scratch_root = fixture("cancel", 4_000);
    let root = scratch_root.path();
    let scratch_db = tempfile::Builder::new()
        .prefix("resonance-concurrent-cancel-db-")
        .tempdir()
        .unwrap();
    let dir = scratch_db.path();

    let library = Library::open_at(dir.join("library.db"), dir.join("art")).unwrap();
    let scanner = library.detached_scanner(options(root)).unwrap();

    let progress = Arc::new(Progress::new());
    let worker_progress = Arc::clone(&progress);

    let started = Instant::now();
    let handle = std::thread::spawn(move || scanner.run(&worker_progress));

    progress.cancel();
    let summary = handle.join().unwrap().unwrap();

    assert!(summary.cancelled, "the scan should report being cancelled");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "cancellation took {:?}",
        started.elapsed()
    );

    let _ = std::fs::remove_dir_all(root);
    let _ = std::fs::remove_dir_all(dir);
}
