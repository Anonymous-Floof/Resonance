//! Opening the index, and keeping its schema current.
//!
//! Migrations key off SQLite's own `user_version`, so there is no bootstrap
//! problem: an empty file reports version 0 and every step runs in order. Each
//! step is applied inside a transaction, so an interrupted upgrade leaves the
//! database at the last version that fully succeeded rather than half-way
//! through one.

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;

/// The database handle, re-exported.
///
/// Callers outside this crate — the analysis pass in `mp-audio`, for one —
/// need to name the type to take a connection, and adding a direct rusqlite
/// dependency to an audio crate to spell one type would be the wrong trade.
pub use rusqlite::Connection as Handle;

/// Bump this and add a step to [`MIGRATIONS`] for every schema change.
pub const SCHEMA_VERSION: u32 = 5;

/// Ordered schema steps. Index `n` upgrades version `n` to `n + 1`.
const MIGRATIONS: &[&str] = &[V1, V2, V3, V4, V5];

/// Open (or create) the index at `path`.
pub fn open(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let connection = Connection::open(path)
        .with_context(|| format!("opening the library index at {}", path.display()))?;

    configure(&connection)?;
    migrate(&connection)?;
    Ok(connection)
}

/// An index that lives only for the lifetime of the process. Used by tests and
/// as the fallback when the real file cannot be opened.
pub fn open_in_memory() -> Result<Connection> {
    let connection = Connection::open_in_memory()?;
    configure(&connection)?;
    migrate(&connection)?;
    Ok(connection)
}

/// Per-connection settings. These are not persisted in the file (except
/// `journal_mode`), so they must be set every time a connection is opened.
fn configure(connection: &Connection) -> Result<()> {
    connection
        .execute_batch(
            "
        -- Survives a crash or a power cut without losing the index, and lets
        -- the scanner write while the UI reads.
        PRAGMA journal_mode = WAL;
        -- WAL already gives durability across process crashes; NORMAL avoids an
        -- fsync per transaction, which matters when inserting 20k tracks.
        PRAGMA synchronous = NORMAL;
        PRAGMA foreign_keys = ON;
        PRAGMA temp_store = MEMORY;
        -- Negative means kibibytes rather than pages: a 32 MiB page cache.
        PRAGMA cache_size = -32768;
        PRAGMA busy_timeout = 5000;
        ",
        )
        .context("configuring the library connection")?;
    Ok(())
}

/// Run any migration steps this database has not seen.
pub fn migrate(connection: &Connection) -> Result<()> {
    let mut version: u32 =
        connection.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))? as u32;

    if version > SCHEMA_VERSION {
        anyhow::bail!(
            "the library index was written by a newer version of Resonance \
             (schema {version}, this build understands {SCHEMA_VERSION})"
        );
    }

    while (version as usize) < MIGRATIONS.len() {
        let step = MIGRATIONS[version as usize];
        tracing::info!("upgrading the library index to schema {}", version + 1);

        connection.execute_batch("BEGIN")?;
        let applied = connection.execute_batch(step).and_then(|()| {
            connection.execute_batch(&format!("PRAGMA user_version = {}", version + 1))
        });

        match applied {
            Ok(()) => connection.execute_batch("COMMIT")?,
            Err(err) => {
                let _ = connection.execute_batch("ROLLBACK");
                return Err(err)
                    .with_context(|| format!("applying library schema migration {}", version + 1));
            }
        }

        version += 1;
    }

    Ok(())
}

/// The initial schema.
///
/// Design notes worth keeping in view:
///
/// * Every name that is displayed is stored twice — once verbatim, once as a
///   `sort_*` key — because sorting has to ignore articles and leading
///   punctuation, and computing that per comparison would make ordering 20k
///   rows measurably slow.
/// * `mtime` and `size` together are the change fingerprint. A rescan reads
///   only the files whose fingerprint moved, which is what makes an unchanged
///   rescan near-instant.
/// * `unplayable` is a table, not a log line. A file the user expects to see
///   has to be explainable later, not only at the moment it was skipped.
const V1: &str = r#"
CREATE TABLE artists (
    id         INTEGER PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE,
    sort_name  TEXT NOT NULL
);
CREATE INDEX artists_sort ON artists(sort_name);

CREATE TABLE albums (
    id         INTEGER PRIMARY KEY,
    title      TEXT NOT NULL,
    sort_title TEXT NOT NULL,
    artist_id  INTEGER REFERENCES artists(id) ON DELETE SET NULL,
    year       INTEGER,
    art_id     TEXT,
    UNIQUE(title, artist_id)
);
CREATE INDEX albums_sort ON albums(sort_title);
CREATE INDEX albums_artist ON albums(artist_id);

CREATE TABLE genres (
    id        INTEGER PRIMARY KEY,
    name      TEXT NOT NULL UNIQUE,
    sort_name TEXT NOT NULL
);

CREATE TABLE tracks (
    id          INTEGER PRIMARY KEY,
    path        TEXT NOT NULL UNIQUE,
    folder      TEXT NOT NULL,
    file_name   TEXT NOT NULL,

    -- change fingerprint
    mtime       INTEGER NOT NULL,
    size        INTEGER NOT NULL,

    -- metadata
    title       TEXT NOT NULL,
    sort_title  TEXT NOT NULL,
    artist_id   INTEGER REFERENCES artists(id) ON DELETE SET NULL,
    album_id    INTEGER REFERENCES albums(id) ON DELETE SET NULL,
    track_no    INTEGER,
    disc_no     INTEGER,
    year        INTEGER,
    duration_ms INTEGER,
    sample_rate INTEGER,
    channels    INTEGER,
    bitrate     INTEGER,
    art_id      TEXT,
    gain_track  REAL,
    gain_album  REAL,

    -- 0 when title/artist were recovered from the filename
    tagged      INTEGER NOT NULL DEFAULT 0,

    -- history
    added_at       INTEGER NOT NULL,
    last_seen_at   INTEGER NOT NULL,
    play_count     INTEGER NOT NULL DEFAULT 0,
    last_played_at INTEGER,
    rating         INTEGER
);
CREATE INDEX tracks_sort_title ON tracks(sort_title);
CREATE INDEX tracks_artist ON tracks(artist_id);
CREATE INDEX tracks_album ON tracks(album_id);
CREATE INDEX tracks_folder ON tracks(folder);
CREATE INDEX tracks_added ON tracks(added_at);

CREATE TABLE track_genres (
    track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    genre_id INTEGER NOT NULL REFERENCES genres(id) ON DELETE CASCADE,
    PRIMARY KEY (track_id, genre_id)
);
CREATE INDEX track_genres_genre ON track_genres(genre_id);

-- Files that were found but cannot be decoded by this build. Kept so the UI
-- can say *why* a file the user expects is missing.
CREATE TABLE unplayable (
    path       TEXT PRIMARY KEY,
    folder     TEXT NOT NULL,
    reason     TEXT NOT NULL,
    seen_at    INTEGER NOT NULL
);

-- Directories that could not be read at all (permissions, unplugged drive).
CREATE TABLE unreadable (
    path    TEXT PRIMARY KEY,
    seen_at INTEGER NOT NULL
);

-- Search. Not contentless: the duplicated text costs a little disk and saves a
-- great deal of fiddly index maintenance, and the rows are short.
CREATE VIRTUAL TABLE tracks_fts USING fts5(
    title,
    artist,
    album,
    genre,
    tokenize = "unicode61 remove_diacritics 2"
);

CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

/// Playlists, listening history and the offline analysis cache.
///
/// Design notes:
///
/// * A smart playlist is an ordinary playlist row with a `rules` document
///   attached, rather than a separate table. They are the same thing to
///   everything that displays them — a name, a description, a set of tracks —
///   and splitting them would mean every list query became a union.
/// * `playlist_items` carries a surrogate id rather than keying on
///   `(playlist_id, position)`. Positions are rewritten on every reorder, and a
///   composite primary key turns a drag-and-drop into a dance around unique
///   constraint violations. It also lets the same track appear twice, which is
///   a legitimate thing to want in a playlist.
/// * `play_history` is kept alongside the `play_count` already on `tracks`.
///   The counter answers "how often"; the history answers "when", which is what
///   recency-aware shuffle and "least recently played" need, and what a count
///   cannot reconstruct.
/// * `audio_features` stores one column per feature rather than a blob. The
///   similarity query wants to read a subset and reason about it in SQL, and a
///   blob would mean deserialising every track in the library to compare two.
///   It carries the same `(mtime, size)` fingerprint the scanner uses, plus an
///   analyser version, so a re-encoded file is re-analysed and a changed
///   algorithm invalidates its own results.
const V2: &str = r#"
CREATE TABLE playlists (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    -- NULL for an ordinary playlist; a rule document for a smart one.
    rules       TEXT
);
CREATE INDEX playlists_name ON playlists(name);

CREATE TABLE playlist_items (
    id          INTEGER PRIMARY KEY,
    playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
    track_id    INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    position    INTEGER NOT NULL,
    added_at    INTEGER NOT NULL
);
CREATE INDEX playlist_items_order ON playlist_items(playlist_id, position);
CREATE INDEX playlist_items_track ON playlist_items(track_id);

CREATE TABLE play_history (
    id        INTEGER PRIMARY KEY,
    track_id  INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    played_at INTEGER NOT NULL
);
CREATE INDEX play_history_track ON play_history(track_id);
CREATE INDEX play_history_time ON play_history(played_at);

CREATE TABLE audio_features (
    track_id    INTEGER PRIMARY KEY REFERENCES tracks(id) ON DELETE CASCADE,

    -- Fingerprint of the file when it was analysed, matching the scanner's.
    mtime       INTEGER NOT NULL,
    size        INTEGER NOT NULL,
    -- Bumped when the analysis changes meaning, forcing a re-run.
    version     INTEGER NOT NULL,

    tempo       REAL,
    centroid    REAL,
    rolloff     REAL,
    loudness    REAL,
    bass        REAL,
    mid         REAL,
    treble      REAL,
    zero_cross  REAL,

    analysed_at INTEGER NOT NULL
);
"#;

/// Schema 3 — the tag-edit journal.
///
/// Every write to a music file is recorded here *before* it happens, so an
/// edit can always be undone. The row holds the whole change as JSON rather
/// than a column per field: the set of editable fields is defined in
/// `tags::Editable` and will grow, and a schema migration for each new field
/// would be a lot of ceremony for data nothing ever queries by field.
///
/// `path` is stored alongside `track_id` deliberately. A track can leave the
/// index — its folder unwatched, the file deleted — and the journal has to
/// survive that, both to show an honest history and because the file may come
/// back. The foreign key is therefore absent by design; this table outlives
/// the rows it refers to.
const V3: &str = r#"
CREATE TABLE tag_edits (
    id          INTEGER PRIMARY KEY,
    track_id    INTEGER NOT NULL,
    -- The file as it was when the edit was made.
    path        TEXT NOT NULL,
    edited_at   INTEGER NOT NULL,
    -- A JSON array of {field, before, after}.
    changes     TEXT NOT NULL,
    -- Set when the edit has been undone, so history shows what still stands.
    reverted_at INTEGER
);
CREATE INDEX tag_edits_recent ON tag_edits(edited_at DESC);
CREATE INDEX tag_edits_track ON tag_edits(track_id);
"#;

/// Measured listening time.
///
/// Added with threshold play counting. A play used to be recorded the moment a
/// track started, which counted a two-second skip the same as a full listen;
/// it is now recorded once the track has actually been heard, and how much was
/// heard is worth keeping so total listening time is a measurement rather than
/// an estimate from durations.
///
/// Rows written before this column existed stay NULL, which the statistics
/// treat as "unknown" rather than as a measured zero.
const V4: &str = r#"
ALTER TABLE play_history ADD COLUMN seconds REAL;
"#;

/// Listening time that is actually the time listened.
///
/// The first attempt at this recorded a listen's duration once, at the moment
/// it crossed the threshold that makes it count as a play — so a four-minute
/// track played in full was recorded as two minutes and the second half was
/// never counted at all. Totals came out at roughly half of reality, and the
/// error compounded with every track.
///
/// The mistake was letting one number answer two questions. The threshold
/// decides whether something counts as a *play*; it has no business deciding
/// how long you listened. Listening is now accumulated per track for every
/// second that actually plays, whether or not the listen was ever long enough
/// to count.
///
/// `play_history.seconds` is superseded and no longer written. It is left in
/// place because dropping a column rebuilds the whole table and would buy
/// nothing.
const V5: &str = r#"
ALTER TABLE tracks ADD COLUMN listened_ms INTEGER NOT NULL DEFAULT 0;
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// Migrating has to be additive: a v1 database with real rows in it must
    /// come out the other side with those rows intact. Testing only a fresh
    /// database would pass even if a migration dropped and recreated a table.
    #[test]
    fn upgrading_from_the_previous_schema_keeps_existing_rows() {
        let db = Connection::open_in_memory().unwrap();
        configure(&db).unwrap();

        // Build a v1 database by hand and stop there.
        db.execute_batch("BEGIN").unwrap();
        db.execute_batch(V1).unwrap();
        db.execute_batch("PRAGMA user_version = 1").unwrap();
        db.execute_batch("COMMIT").unwrap();

        db.execute(
            "INSERT INTO tracks (
                 id, path, folder, file_name, mtime, size, title, sort_title,
                 added_at, last_seen_at
             ) VALUES (1, '/a/b.mp3', '/a', 'b.mp3', 1, 2, 'B', 'b', 0, 0)",
            [],
        )
        .unwrap();

        migrate(&db).unwrap();

        let version: i64 = db
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, i64::from(SCHEMA_VERSION));

        let title: String = db
            .query_row("SELECT title FROM tracks WHERE id = 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(title, "B", "the migration lost an existing track");

        // And the new tables are usable.
        db.execute(
            "INSERT INTO playlists (id, name, created_at, updated_at)
             VALUES (1, 'Test', 0, 0)",
            [],
        )
        .unwrap();
    }

    /// The upgrade every existing installation actually takes.
    #[test]
    fn upgrading_from_three_adds_measured_listening_without_losing_history() {
        let db = Connection::open_in_memory().unwrap();
        configure(&db).unwrap();

        // Build a database at schema 3 — the version shipped before listening
        // time was measured — and put real history in it.
        db.execute_batch("BEGIN").unwrap();
        for step in &MIGRATIONS[..3] {
            db.execute_batch(step).unwrap();
        }
        db.execute_batch("PRAGMA user_version = 3").unwrap();
        db.execute_batch("COMMIT").unwrap();

        db.execute(
            "INSERT INTO tracks (
                 id, path, folder, file_name, mtime, size, title, sort_title,
                 play_count, added_at, last_seen_at
             ) VALUES (1, '/a/b.mp3', '/a', 'b.mp3', 1, 2, 'B', 'b', 3, 0, 0)",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO play_history (track_id, played_at) VALUES (1, 100), (1, 200)",
            [],
        )
        .unwrap();

        migrate(&db).unwrap();

        let version: i64 = db
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, i64::from(SCHEMA_VERSION));

        let (rows, plays): (i64, i64) = db
            .query_row(
                "SELECT (SELECT COUNT(*) FROM play_history),
                        (SELECT play_count FROM tracks WHERE id = 1)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(rows, 2, "the migration lost listening history");
        assert_eq!(plays, 3, "the migration lost a play count");

        // Listening starts from zero rather than being back-filled: the
        // figures the old model produced were wrong, and seeding from them
        // would bake that error in as data.
        let listened: i64 = db
            .query_row("SELECT listened_ms FROM tracks WHERE id = 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(listened, 0);

        // History that predates the column is unmeasured, not measured as zero.
        let unmeasured: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM play_history WHERE seconds IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(unmeasured, 2);

        // And the column accepts a measurement from here on.
        db.execute(
            "INSERT INTO play_history (track_id, played_at, seconds) VALUES (1, 300, 42.5)",
            [],
        )
        .unwrap();
        let secs: f64 = db
            .query_row(
                "SELECT seconds FROM play_history WHERE played_at = 300",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!((secs - 42.5).abs() < f64::EPSILON);
    }

    /// Deleting a track has to take its playlist entries, history and analysis
    /// with it, or the index accumulates rows pointing at nothing.
    #[test]
    fn removing_a_track_cascades_into_the_new_tables() {
        let db = open_in_memory().unwrap();

        db.execute_batch(
            "INSERT INTO tracks (
                 id, path, folder, file_name, mtime, size, title, sort_title,
                 added_at, last_seen_at
             ) VALUES (1, '/a/b.mp3', '/a', 'b.mp3', 1, 2, 'B', 'b', 0, 0);
             INSERT INTO playlists (id, name, created_at, updated_at)
                 VALUES (1, 'P', 0, 0);
             INSERT INTO playlist_items (playlist_id, track_id, position, added_at)
                 VALUES (1, 1, 0, 0);
             INSERT INTO play_history (track_id, played_at) VALUES (1, 0);
             INSERT INTO audio_features (track_id, mtime, size, version, analysed_at)
                 VALUES (1, 1, 2, 1, 0);",
        )
        .unwrap();

        db.execute("DELETE FROM tracks WHERE id = 1", []).unwrap();

        for table in ["playlist_items", "play_history", "audio_features"] {
            let left: i64 = db
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(left, 0, "{table} kept a row for a deleted track");
        }

        // The playlist itself survives — losing a file should not lose the list.
        let playlists: i64 = db
            .query_row("SELECT count(*) FROM playlists", [], |row| row.get(0))
            .unwrap();
        assert_eq!(playlists, 1);
    }

    #[test]
    fn a_fresh_database_lands_on_the_current_schema() {
        let db = open_in_memory().unwrap();
        let version: i64 = db
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, i64::from(SCHEMA_VERSION));
    }

    #[test]
    fn migrating_twice_is_a_no_op() {
        let db = open_in_memory().unwrap();
        migrate(&db).expect("re-running migrations must be safe");
        let tables: i64 = db
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'tracks'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 1);
    }

    /// A database from a future build must be refused rather than silently
    /// misread — the alternative is corrupting the user's library.
    #[test]
    fn a_newer_schema_is_refused() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("PRAGMA user_version = 9999").unwrap();
        let err = migrate(&db).unwrap_err();
        assert!(err.to_string().contains("newer version"), "{err}");
    }

    #[test]
    fn full_text_search_is_available() {
        let db = open_in_memory().unwrap();
        db.execute(
            "INSERT INTO tracks_fts(rowid, title, artist, album, genre) VALUES (1, ?, ?, ?, ?)",
            ["Paper Lantern", "Vellichor", "Quiet Machine", "Alternative"],
        )
        .unwrap();

        let hit: i64 = db
            .query_row(
                "SELECT rowid FROM tracks_fts WHERE tracks_fts MATCH 'velli*'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(hit, 1);
    }

    /// Deleting a track must take its genre links with it, or the genre counts
    /// drift upward every rescan.
    #[test]
    fn deleting_a_track_cascades_to_its_genres() {
        let db = open_in_memory().unwrap();
        db.execute_batch(
            "INSERT INTO genres(id, name, sort_name) VALUES (1, 'Rock', 'rock');
             INSERT INTO tracks(id, path, folder, file_name, mtime, size, title, sort_title,
                                added_at, last_seen_at)
                 VALUES (1, 'a.mp3', '.', 'a.mp3', 0, 0, 'A', 'a', 0, 0);
             INSERT INTO track_genres(track_id, genre_id) VALUES (1, 1);",
        )
        .unwrap();

        db.execute("DELETE FROM tracks WHERE id = 1", []).unwrap();

        let links: i64 = db
            .query_row("SELECT count(*) FROM track_genres", [], |row| row.get(0))
            .unwrap();
        assert_eq!(links, 0);
    }
}
