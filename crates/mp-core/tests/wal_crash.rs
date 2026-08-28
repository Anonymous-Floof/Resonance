//! What a crash does to the library index.
//!
//! The index is in WAL mode with `synchronous = NORMAL`, which is the
//! combination SQLite documents as safe against process death and against
//! power loss short of a disk that lies about flushing. That is a claim worth
//! testing rather than trusting, because the failure mode — a library that
//! will not open — is one the user notices and cannot fix.
//!
//! A crash is simulated by genuinely crashing: the test re-runs its own binary
//! as a child, and the child calls `abort()` in the middle of an open
//! transaction. Dropping a connection in-process would not do, because that
//! runs SQLite's cleanup and rolls the transaction back tidily, which is the
//! opposite of what is being tested.

use std::path::{Path, PathBuf};
use std::process::Command;

use mp_core::library::Library;

/// Set on the child, and holds the database it should crash while writing to.
const CHILD_ENV: &str = "RESONANCE_WAL_CRASH_DB";

/// Rows the child commits properly before opening the doomed transaction.
const COMMITTED: usize = 200;

/// Rows it writes inside the transaction it never commits.
const ABANDONED: usize = 50;

/// The child half of the crash test.
///
/// Runs as a normal (trivially passing) test in an ordinary run, and only does
/// anything when the parent re-executes it with the environment variable set.
#[test]
fn crash_child() {
    let Ok(path) = std::env::var(CHILD_ENV) else {
        return;
    };

    let library = Library::open_at(&path, Path::new(&path).with_extension("art"))
        .expect("the child should be able to open the library");
    let connection = library.connection();

    // Committed properly. These must survive.
    connection.execute_batch("BEGIN").unwrap();
    for index in 0..COMMITTED {
        insert(connection, index);
    }
    connection.execute_batch("COMMIT").unwrap();

    // Left open. These must not survive, and must not damage anything.
    connection.execute_batch("BEGIN").unwrap();
    for index in COMMITTED..COMMITTED + ABANDONED {
        insert(connection, index);
    }

    // No unwinding, no destructors, no SQLite cleanup — the process simply
    // stops, exactly as it would on a power cut or a kill.
    std::process::abort();
}

#[test]
fn a_crash_mid_write_leaves_the_library_intact() {
    // The child re-runs this same binary, so a nested run would recurse.
    if std::env::var(CHILD_ENV).is_ok() {
        return;
    }

    let dir = temp_dir("wal-crash");
    let db = dir.join("library.db");

    let status = Command::new(std::env::current_exe().expect("the test binary's own path"))
        .args(["--exact", "crash_child", "--nocapture"])
        .env(CHILD_ENV, &db)
        .status()
        .expect("the child should launch");

    assert!(
        !status.success(),
        "the child was supposed to abort, but exited cleanly ({status:?})"
    );

    // A crash in WAL mode leaves the log behind for the next open to replay.
    assert!(db.is_file(), "the database file should still be there");

    let library = Library::open_at(&db, dir.join("art")).expect(
        "a library that cannot be reopened after a crash is the failure this \
         whole test exists to catch",
    );

    assert!(
        library.is_intact().expect("the check itself should run"),
        "the database is structurally damaged after a crash"
    );

    let count: usize = library
        .connection()
        .query_row("SELECT COUNT(*) FROM tracks", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("counting should work") as usize;

    assert_eq!(
        count, COMMITTED,
        "the committed rows must survive and the abandoned transaction must not"
    );

    // And the index is not merely readable — it still takes writes.
    insert(library.connection(), 9_999);

    let _ = std::fs::remove_dir_all(&dir);
}

/// A library closed properly should leave nothing to replay.
///
/// This is what makes the file safe to copy or carry away: after a checkpoint
/// the database is self-contained, rather than needing a sidecar log that a
/// naive copy would leave behind.
#[test]
fn a_checkpoint_folds_the_log_back_into_the_file() {
    let dir = temp_dir("wal-checkpoint");
    let db = dir.join("library.db");

    {
        let library = Library::open_at(&db, dir.join("art")).unwrap();
        let connection = library.connection();

        connection.execute_batch("BEGIN").unwrap();
        for index in 0..COMMITTED {
            insert(connection, index);
        }
        connection.execute_batch("COMMIT").unwrap();

        let wal = db.with_extension("db-wal");
        let before = wal.metadata().map(|m| m.len()).unwrap_or(0);
        assert!(before > 0, "there should be a log to fold back");

        library.checkpoint().expect("checkpointing should succeed");

        let after = wal.metadata().map(|m| m.len()).unwrap_or(0);
        assert!(
            after < before,
            "the log was not truncated: {before} bytes before, {after} after"
        );
    }

    // And the data is all still there once the log is gone.
    let library = Library::open_at(&db, dir.join("art")).unwrap();
    let count: i64 = library
        .connection()
        .query_row("SELECT COUNT(*) FROM tracks", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count as usize, COMMITTED);

    let _ = std::fs::remove_dir_all(&dir);
}

/// One track row, with only the columns the schema insists on.
fn insert(connection: &rusqlite::Connection, index: usize) {
    connection
        .execute(
            "INSERT INTO tracks (
                 path, folder, file_name, mtime, size,
                 title, sort_title, added_at, last_seen_at
             ) VALUES (?1, ?2, ?3, 0, 0, ?4, ?5, 0, 0)",
            rusqlite::params![
                format!("D:\\Music\\{index:06}.mp3"),
                "D:\\Music",
                format!("{index:06}.mp3"),
                format!("Track {index}"),
                format!("track {index:06}"),
            ],
        )
        .expect("the insert should succeed");
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "resonance-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
