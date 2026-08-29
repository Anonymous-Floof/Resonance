//! The music library: a SQLite index of everything in the watched folders.
//!
//! The index is a cache, not a record of anything irreplaceable. Deleting
//! `library.db` costs a rescan and nothing else, which is deliberate: it means
//! the recovery path for any corruption is to throw the file away rather than
//! to repair it, and it means Resonance never becomes the only place some piece
//! of the user's music information exists.
//!
//! Threading: [`Library`] owns one connection and is not `Send`, because
//! `rusqlite::Connection` is not. Scanning runs on a background thread with its
//! own connection to the same file — WAL mode lets that writer and the UI's
//! reader work at the same time without either blocking the other.

pub mod accent;
pub mod art;
pub mod db;
pub mod duplicates;
pub mod features;
pub mod ingest;
pub mod lyrics;
pub mod m3u;
pub mod model;
pub mod names;
pub mod playlist;
pub mod query;
pub mod similar;
pub mod smart;
pub mod stats;
pub mod tags;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension};

pub use accent::{CoverPalette, Swatch};
pub use art::{ArtCache, ArtSize};

pub use features::Features;
pub use ingest::{Phase, Progress, ScanOptions, Summary};
pub use lyrics::Lyrics;
pub use model::{
    Album, AlbumId, Artist, ArtistId, Filter, Folder, Genre, GenreId, Order, Stats, Track, TrackId,
};
pub use playlist::{Playlist, PlaylistId};
pub use similar::{Reason, Seed, Suggestion};
pub use smart::{
    Field as RuleField, Group as RuleGroup, Match as RuleMatch, Node as RuleNode, Op as RuleOp,
    Rule, SmartRules,
};

use crate::paths::AppPaths;

/// One journalled write to a music file.
#[derive(Debug, Clone)]
pub struct TagEdit {
    pub id: i64,
    pub track: TrackId,
    /// The file as it was when the edit was made.
    pub path: PathBuf,
    /// Unix seconds.
    pub edited_at: i64,
    pub changes: Vec<tags::Change>,
    /// Set once the edit has been undone.
    pub reverted_at: Option<i64>,
}

impl TagEdit {
    pub fn is_reverted(&self) -> bool {
        self.reverted_at.is_some()
    }

    /// A one-line description, for the history list.
    pub fn summary(&self) -> String {
        match self.changes.as_slice() {
            [] => "no changes".to_owned(),
            [only] => format!(
                "{}: {} to {}",
                only.field.label(),
                quoted(only.before.as_deref()),
                quoted(only.after.as_deref())
            ),
            many => format!(
                "{} fields: {}",
                many.len(),
                many.iter()
                    .map(|change| change.field.label())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

fn quoted(value: Option<&str>) -> String {
    match value {
        Some(value) => format!("\"{value}\""),
        None => "nothing".to_owned(),
    }
}

/// What came of importing a playlist file.
#[derive(Debug, Clone)]
pub struct ImportReport {
    pub playlist: PlaylistId,
    pub name: String,
    /// Tracks that were found in the index and added.
    pub added: usize,
    /// Entries the index has never seen, usually an unscanned folder.
    pub missing: Vec<std::path::PathBuf>,
}

impl ImportReport {
    /// A one-line summary for a toast.
    pub fn summary(&self) -> String {
        let total = self.added + self.missing.len();

        match self.missing.len() {
            0 => format!("Imported {} into \"{}\"", tracks(self.added), self.name),
            _ => format!(
                "Imported {} of {total} into \"{}\" — the rest are not in your library",
                tracks(self.added),
                self.name
            ),
        }
    }
}

fn tracks(count: usize) -> String {
    match count {
        1 => "1 track".to_owned(),
        other => format!("{other} tracks"),
    }
}

/// Where this library's data lives.
#[derive(Debug, Clone)]
enum Source {
    File(PathBuf),
    /// Used by tests, and as the fallback when the real file cannot be opened —
    /// the app stays usable, it just forgets the index on exit.
    Memory,
}

/// A handle to the library index.
pub struct Library {
    connection: Connection,
    cache: ArtCache,
    source: Source,
}

impl Library {
    /// Open the index at its standard location.
    ///
    /// A database that cannot be opened is moved aside rather than reported as
    /// a fatal error: the user came here to listen to music, and an index that
    /// rebuilds itself in a few seconds is not worth refusing to start over.
    pub fn open(paths: &AppPaths) -> Result<Self> {
        let path = paths.library_db();
        let cache = ArtCache::new(paths.art_cache_dir());

        match db::open(&path) {
            Ok(connection) => Ok(Self {
                connection,
                cache,
                source: Source::File(path),
            }),
            Err(err) => {
                tracing::error!("could not open the library index: {err:#}");
                Self::quarantine(&path);

                match db::open(&path) {
                    Ok(connection) => Ok(Self {
                        connection,
                        cache,
                        source: Source::File(path),
                    }),
                    Err(err) => {
                        tracing::error!("falling back to an in-memory library: {err:#}");
                        Ok(Self {
                            connection: db::open_in_memory()?,
                            cache,
                            source: Source::Memory,
                        })
                    }
                }
            }
        }
    }

    /// An index that exists only for this process. For tests.
    pub fn in_memory() -> Result<Self> {
        Ok(Self {
            connection: db::open_in_memory()?,
            cache: ArtCache::new(std::env::temp_dir().join("resonance-art-test")),
            source: Source::Memory,
        })
    }

    /// An index at a specific path, with its art cache beside it. For tests and
    /// for portable mode.
    pub fn open_at(path: impl AsRef<Path>, art_dir: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        Ok(Self {
            connection: db::open(&path)?,
            cache: ArtCache::new(art_dir.as_ref()),
            source: Source::File(path),
        })
    }

    /// Rename a database we cannot open, so the next attempt starts clean.
    fn quarantine(path: &Path) {
        if !path.exists() {
            return;
        }
        let aside = path.with_extension("corrupt.db");
        let _ = std::fs::remove_file(&aside);
        match std::fs::rename(path, &aside) {
            Ok(()) => tracing::warn!("moved the unreadable index to {}", aside.display()),
            Err(err) => tracing::error!("could not move the unreadable index aside: {err}"),
        }
        // WAL sidecars belong to the file we just moved; leaving them would
        // make the fresh database look like it had uncommitted work.
        for suffix in ["-wal", "-shm"] {
            let mut sidecar = path.as_os_str().to_owned();
            sidecar.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(sidecar));
        }
    }

    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    pub fn art(&self) -> &ArtCache {
        &self.cache
    }

    // -- scanning ----------------------------------------------------------

    /// A scanner that can be moved to another thread.
    ///
    /// `None` for an in-memory library, which has nothing a second connection
    /// could attach to; call [`scan_blocking`](Self::scan_blocking) instead.
    pub fn detached_scanner(&self, options: ScanOptions) -> Option<Scanner> {
        match &self.source {
            Source::File(path) => Some(Scanner {
                path: path.clone(),
                cache: self.cache.clone(),
                options,
            }),
            Source::Memory => None,
        }
    }

    /// Open a second connection to the same index, for a background job.
    ///
    /// The analysis pass runs for minutes and writes as it goes, so it cannot
    /// share the UI's connection — SQLite would serialise the two and the
    /// interface would stall behind a decode. WAL mode is what makes a second
    /// writer workable here: readers never block, and the pass writes one small
    /// row at a time.
    ///
    /// `None` for an in-memory library, which has no file for a second
    /// connection to open.
    pub fn detached_connection(&self) -> Option<Result<Connection>> {
        match &self.source {
            Source::File(path) => Some(db::open(path)),
            Source::Memory => None,
        }
    }

    /// Scan on the calling thread, using this library's own connection.
    pub fn scan_blocking(&mut self, options: &ScanOptions, progress: &Progress) -> Result<Summary> {
        ingest::scan(&mut self.connection, Some(&self.cache), options, progress)
    }

    // -- reading -----------------------------------------------------------

    pub fn stats(&self) -> Result<Stats> {
        query::stats(&self.connection)
    }

    pub fn tracks(&self, filter: &Filter, order: Order, descending: bool) -> Result<Vec<Track>> {
        query::tracks(&self.connection, filter, order, descending)
    }

    pub fn track(&self, id: TrackId) -> Result<Option<Track>> {
        query::track(&self.connection, id)
    }

    pub fn search(&self, text: &str, limit: Option<usize>) -> Result<Vec<Track>> {
        query::search(&self.connection, text, limit)
    }

    pub fn artists(&self) -> Result<Vec<Artist>> {
        query::artists(&self.connection)
    }

    /// Albums with at least `min_tracks` tracks. Pass 1 for everything.
    pub fn albums(&self, artist: Option<ArtistId>, min_tracks: u32) -> Result<Vec<Album>> {
        query::albums(&self.connection, artist, min_tracks)
    }

    pub fn genres(&self) -> Result<Vec<Genre>> {
        query::genres(&self.connection)
    }

    pub fn folders(&self) -> Result<Vec<Folder>> {
        query::folders(&self.connection)
    }

    pub fn unplayable(&self) -> Result<Vec<(PathBuf, String)>> {
        query::unplayable(&self.connection)
    }

    pub fn unreadable(&self) -> Result<Vec<PathBuf>> {
        query::unreadable(&self.connection)
    }

    /// Where a cached cover lives, if it has been extracted.
    pub fn art_path(&self, art_id: &str, size: ArtSize) -> Option<PathBuf> {
        let path = self.cache.path(art_id, size);
        path.is_file().then_some(path)
    }

    /// The colours of a cached cover, for adaptive theming.
    ///
    /// Cheap after the first call per cover — the result is a small file
    /// written during the scan — but not free, so the UI caches it per
    /// `art_id` rather than asking every frame.
    pub fn art_palette(&self, art_id: &str) -> Option<CoverPalette> {
        self.cache.palette(art_id)
    }

    /// Note that a track was played.
    ///
    /// Goes through the playlist module rather than straight to `query`,
    /// because a play is two facts: the counter on the track, and an entry in
    /// the history that recency-aware shuffle and auto-radio read.
    pub fn record_play(&self, id: TrackId) -> Result<()> {
        playlist::record_play(&self.connection, id, now_unix())
    }

    /// Add to the time a track has been listened to.
    ///
    /// Separate from [`record_play`](Self::record_play) because a play is a
    /// one-off threshold decision and listening is a quantity that keeps
    /// growing while the audio plays.
    pub fn add_listening(&self, id: TrackId, seconds: f64) -> Result<()> {
        playlist::add_listening(&self.connection, id, seconds)
    }

    // -- statistics --------------------------------------------------------

    /// A summary of the library and everything listened to in it.
    pub fn totals(&self) -> Result<stats::Totals> {
        stats::totals(&self.connection)
    }

    /// The most played tracks, best first.
    pub fn top_tracks(&self, limit: usize) -> Result<Vec<stats::PlayedTrack>> {
        stats::top_tracks(&self.connection, limit)
    }

    /// The most played artists, best first.
    pub fn top_artists(&self, limit: usize) -> Result<Vec<stats::Ranked>> {
        stats::top_artists(&self.connection, limit)
    }

    /// The most played albums, best first.
    pub fn top_albums(&self, limit: usize) -> Result<Vec<stats::Ranked>> {
        stats::top_albums(&self.connection, limit)
    }

    /// The tracks played most recently, newest first.
    pub fn recently_played_tracks(&self, limit: usize) -> Result<Vec<Track>> {
        stats::recent(&self.connection, limit)
    }

    /// Plays per day over the last `days` days, oldest bucket first.
    pub fn activity(&self, days: usize) -> Result<Vec<u32>> {
        stats::activity(&self.connection, days, now_unix())
    }

    /// The id of a track by path, for reconciling a queue with the index.
    pub fn id_for_path(&self, path: &Path) -> Result<Option<TrackId>> {
        ingest::track_id_for_path(&self.connection, path)
    }

    // -- playlists ---------------------------------------------------------

    pub fn playlists(&self) -> Result<Vec<Playlist>> {
        playlist::list(&self.connection, now_unix())
    }

    pub fn playlist(&self, id: PlaylistId) -> Result<Option<Playlist>> {
        playlist::get(&self.connection, id, now_unix())
    }

    pub fn playlist_tracks(&self, id: PlaylistId) -> Result<Vec<Track>> {
        playlist::tracks(&self.connection, id, now_unix())
    }

    pub fn create_playlist(&self, name: &str) -> Result<PlaylistId> {
        playlist::create(&self.connection, name, now_unix())
    }

    pub fn create_smart_playlist(&self, name: &str, rules: &SmartRules) -> Result<PlaylistId> {
        playlist::create_smart(&self.connection, name, rules, now_unix())
    }

    pub fn rename_playlist(&self, id: PlaylistId, name: &str) -> Result<()> {
        playlist::rename(&self.connection, id, name, now_unix())
    }

    pub fn set_playlist_rules(&self, id: PlaylistId, rules: &SmartRules) -> Result<()> {
        playlist::set_rules(&self.connection, id, rules, now_unix())
    }

    // -- listening statistics ----------------------------------------------

    /// Play counts and history for every track that has one.
    ///
    /// Keyed by path rather than id, because this is only ever read to be
    /// written somewhere else — a bundle, a backup — where ids from this
    /// index have no meaning.
    pub fn play_statistics(&self) -> Result<Vec<crate::bundle::StoredStats>> {
        let mut statement = self.connection.prepare(
            "SELECT t.path, t.play_count, h.played_at
             FROM tracks t
             LEFT JOIN play_history h ON h.track_id = t.id
             WHERE t.play_count > 0 OR h.played_at IS NOT NULL
             ORDER BY t.id, h.played_at",
        )?;

        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u32>(1)?,
                row.get::<_, Option<i64>>(2)?,
            ))
        })?;

        // The join produces one row per play, so consecutive rows for the same
        // track are folded back together as they arrive.
        let mut out: Vec<crate::bundle::StoredStats> = Vec::new();

        for row in rows {
            let (path, play_count, played_at) = row?;

            match out.last_mut() {
                Some(last) if last.path == Path::new(&path) => {
                    if let Some(at) = played_at {
                        last.plays.push(at);
                    }
                }
                _ => out.push(crate::bundle::StoredStats {
                    path: PathBuf::from(path),
                    play_count,
                    plays: played_at.into_iter().collect(),
                }),
            }
        }

        Ok(out)
    }

    /// Fold imported statistics into this library.
    ///
    /// Idempotent by construction: a play count takes whichever side is
    /// higher, and a history entry is inserted only if this library has no
    /// play for that track at that second. Importing the same bundle twice
    /// therefore changes nothing the second time, which is what makes it safe
    /// to retry an import you are not sure completed.
    ///
    /// Returns how many tracks were touched.
    pub fn merge_play_statistics(&mut self, stats: &[crate::bundle::StoredStats]) -> Result<usize> {
        let transaction = self.connection.transaction()?;
        let mut touched = 0;

        for entry in stats {
            let id: Option<TrackId> = transaction
                .query_row(
                    "SELECT id FROM tracks WHERE path = ?1",
                    rusqlite::params![entry.path.to_string_lossy()],
                    |row| row.get(0),
                )
                .optional()?;

            // A track this library does not have is not an error: the bundle
            // may cover a wider collection than this machine holds.
            let Some(id) = id else {
                continue;
            };

            transaction.execute(
                "UPDATE tracks SET play_count = MAX(play_count, ?2) WHERE id = ?1",
                rusqlite::params![id, entry.play_count],
            )?;

            for played_at in &entry.plays {
                transaction.execute(
                    "INSERT INTO play_history (track_id, played_at)
                     SELECT ?1, ?2
                     WHERE NOT EXISTS (
                         SELECT 1 FROM play_history WHERE track_id = ?1 AND played_at = ?2
                     )",
                    rusqlite::params![id, played_at],
                )?;
            }

            touched += 1;
        }

        transaction.commit()?;
        Ok(touched)
    }

    // -- tag editing -------------------------------------------------------

    /// What an edit would change, without touching anything.
    ///
    /// The confirmation step's source of truth. It runs exactly the same
    /// comparison the write does, so what the user is shown and asked to
    /// approve is what will happen.
    pub fn preview_tag_edit(&self, id: TrackId, edit: &tags::Edit) -> Result<Vec<tags::Change>> {
        let track = self
            .track(id)?
            .with_context(|| format!("track {id} is not in the library"))?;

        tags::preview(&track.path, edit)
    }

    /// Write tags to a file, journalling the change first.
    ///
    /// The order is the whole point: the journal row is committed *before* the
    /// file is written, so a crash mid-write leaves a record of what was
    /// attempted rather than a changed file nobody can undo. If the write then
    /// fails the row is removed again, because an edit that never happened
    /// must not appear in the history as though it did.
    ///
    /// Returns `None` when the edit asks for values the file already has, in
    /// which case nothing is written and nothing is journalled.
    pub fn edit_tags(&mut self, id: TrackId, edit: &tags::Edit) -> Result<Option<TagEdit>> {
        let track = self
            .track(id)?
            .with_context(|| format!("track {id} is not in the library"))?;

        let changes = tags::preview(&track.path, edit)?;
        if changes.is_empty() {
            return Ok(None);
        }

        let now = now_unix();
        let record_id = self.record_tag_edit(id, &track.path, now, &changes)?;

        if let Err(err) = tags::write(&track.path, edit) {
            // The file is unchanged, so the journal must be too.
            let _ = self.connection.execute(
                "DELETE FROM tag_edits WHERE id = ?1",
                rusqlite::params![record_id],
            );
            return Err(err);
        }

        // The index still holds the old values, and the file has just changed
        // under it, so this row is re-read now rather than leaving stale text
        // on screen until the next scan.
        self.refresh_track(id)?;

        Ok(Some(TagEdit {
            id: record_id,
            track: id,
            path: track.path,
            edited_at: now,
            changes,
            reverted_at: None,
        }))
    }

    fn record_tag_edit(
        &self,
        track: TrackId,
        path: &Path,
        at: i64,
        changes: &[tags::Change],
    ) -> Result<i64> {
        let json = serde_json::to_string(changes).context("encoding a tag edit for the journal")?;

        self.connection.execute(
            "INSERT INTO tag_edits (track_id, path, edited_at, changes) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![track, path.to_string_lossy(), at, json],
        )?;

        Ok(self.connection.last_insert_rowid())
    }

    /// Recent tag edits, newest first.
    pub fn tag_history(&self, limit: usize) -> Result<Vec<TagEdit>> {
        let mut statement = self.connection.prepare(
            "SELECT id, track_id, path, edited_at, changes, reverted_at
             FROM tag_edits ORDER BY edited_at DESC, id DESC LIMIT ?1",
        )?;

        let rows = statement.query_map(rusqlite::params![limit as i64], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<i64>>(5)?,
            ))
        })?;

        let mut out = Vec::new();
        for row in rows {
            let (id, track, path, edited_at, json, reverted_at) = row?;

            // A row we cannot decode is skipped rather than fatal: the history
            // panel showing one entry fewer beats it refusing to open.
            let Ok(changes) = serde_json::from_str::<Vec<tags::Change>>(&json) else {
                tracing::warn!("tag edit {id} has an unreadable change list");
                continue;
            };

            out.push(TagEdit {
                id,
                track,
                path: PathBuf::from(path),
                edited_at,
                changes,
                reverted_at,
            });
        }

        Ok(out)
    }

    /// Put a journalled edit back the way it was.
    ///
    /// Refuses if the file has changed since — by another edit, another
    /// program, or a re-tag — because reverting would then silently discard
    /// work this journal knows nothing about.
    pub fn revert_tag_edit(&mut self, record: i64) -> Result<()> {
        let entry = self
            .tag_edit(record)?
            .with_context(|| format!("tag edit {record} is not in the journal"))?;

        if entry.reverted_at.is_some() {
            bail!("that edit has already been undone");
        }

        tags::revert(&entry.path, &entry.changes)?;

        self.connection.execute(
            "UPDATE tag_edits SET reverted_at = ?2 WHERE id = ?1",
            rusqlite::params![record, now_unix()],
        )?;

        self.refresh_track(entry.track)?;

        Ok(())
    }

    fn tag_edit(&self, record: i64) -> Result<Option<TagEdit>> {
        let row = self
            .connection
            .query_row(
                "SELECT id, track_id, path, edited_at, changes, reverted_at
                 FROM tag_edits WHERE id = ?1",
                rusqlite::params![record],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<i64>>(5)?,
                    ))
                },
            )
            .optional()?;

        let Some((id, track, path, edited_at, json, reverted_at)) = row else {
            return Ok(None);
        };

        Ok(Some(TagEdit {
            id,
            track,
            path: PathBuf::from(path),
            edited_at,
            changes: serde_json::from_str(&json).context("decoding a journalled tag edit")?,
            reverted_at,
        }))
    }

    /// Re-read one file into the index after its tags changed.
    ///
    /// Clearing the fingerprint is what does the work: the scanner skips a file
    /// whose `(mtime, size)` it has already seen, so zeroing it is how a single
    /// track is marked stale without touching the rest of the index.
    fn refresh_track(&mut self, id: TrackId) -> Result<()> {
        self.connection.execute(
            "UPDATE tracks SET mtime = 0, size = 0 WHERE id = ?1",
            rusqlite::params![id],
        )?;
        Ok(())
    }

    // -- playlist interchange ------------------------------------------------

    /// Write a playlist out as an M3U8 file.
    ///
    /// Returns how many tracks were written. A smart playlist exports whatever
    /// its rules currently match, because that is what the user sees in it —
    /// the rules themselves have no meaning outside Resonance.
    pub fn export_playlist(&self, id: PlaylistId, destination: &Path) -> Result<usize> {
        let tracks = self.playlist_tracks(id)?;
        let text = m3u::export(&tracks, Some(destination));

        std::fs::write(destination, text)
            .with_context(|| format!("writing {}", destination.display()))?;

        Ok(tracks.len())
    }

    /// Read an M3U8 file in as a new playlist.
    ///
    /// Tracks that are not in the index are reported rather than skipped
    /// silently: "imported 40 of 60" with the missing ones named is the honest
    /// answer, and usually means a folder has not been scanned yet.
    pub fn import_playlist(&mut self, source: &Path) -> Result<ImportReport> {
        let text = std::fs::read_to_string(source)
            .with_context(|| format!("reading {}", source.display()))?;

        let entries = m3u::parse(&text, source);

        let mut found = Vec::new();
        let mut missing = Vec::new();

        for entry in entries {
            match self.resolve_entry(&entry.path)? {
                Some(id) => found.push(id),
                None => missing.push(entry.path),
            }
        }

        let name = source.file_stem().map_or_else(
            || "Imported".to_owned(),
            |stem| stem.to_string_lossy().into(),
        );

        let playlist = self.create_playlist(&name)?;
        let added = self.add_to_playlist(playlist, &found)?;

        Ok(ImportReport {
            playlist,
            name,
            added,
            missing,
        })
    }

    /// Match a playlist entry against the index.
    fn resolve_entry(&self, path: &Path) -> Result<Option<TrackId>> {
        if let Some(id) = self.id_for_path(path)? {
            return Ok(Some(id));
        }

        // The exact path missed. That is normal for a playlist another program
        // wrote, where the same file can be spelled differently, so fall back
        // to the filename when it names exactly one track.
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            return Ok(None);
        };

        ingest::track_id_for_filename(&self.connection, name)
    }

    pub fn delete_playlist(&self, id: PlaylistId) -> Result<()> {
        playlist::delete(&self.connection, id)
    }

    pub fn add_to_playlist(&mut self, id: PlaylistId, tracks: &[TrackId]) -> Result<usize> {
        playlist::add_tracks(&mut self.connection, id, tracks, now_unix())
    }

    pub fn remove_from_playlist(&mut self, id: PlaylistId, position: usize) -> Result<()> {
        playlist::remove_at(&mut self.connection, id, position, now_unix())
    }

    pub fn move_in_playlist(&mut self, id: PlaylistId, from: usize, to: usize) -> Result<()> {
        playlist::move_item(&mut self.connection, id, from, to, now_unix())
    }

    /// Trim the play history to a bounded size.
    pub fn trim_history(&self, keep: usize) -> Result<usize> {
        playlist::trim_history(&self.connection, keep)
    }

    // -- discovery ---------------------------------------------------------

    /// Tracks that suit `seed`, ranked and explained.
    pub fn suggest(&self, seed: Seed, limit: usize) -> Result<Vec<Suggestion>> {
        similar::suggest(&self.connection, seed, limit, similar::DEFAULT_PER_ARTIST)
    }

    /// What to play next when the queue runs dry.
    pub fn radio(&self, seed: Seed, queued: &[TrackId], count: usize) -> Result<Vec<Track>> {
        similar::radio(&self.connection, seed, queued, count)
    }

    /// Groups of tracks that look like the same recording.
    pub fn duplicates(&self, tolerance: std::time::Duration) -> Result<Vec<duplicates::Group>> {
        duplicates::find(&self.connection, tolerance)
    }

    // -- maintenance -------------------------------------------------------

    /// Delete cached covers nothing refers to any more.
    ///
    /// Cheap enough to run after any scan that removed tracks, and skipped
    /// entirely otherwise — walking the cache directory is the expensive part.
    pub fn prune_art_cache(&self) -> Result<usize> {
        let live: HashSet<String> = ingest::unreferenced_art(&self.connection)?
            .into_iter()
            .collect();

        let mut removed = 0;
        let entries = walkdir::WalkDir::new(self.cache.root())
            .max_depth(2)
            .into_iter()
            .flatten();

        for entry in entries {
            if !entry.file_type().is_file() {
                continue;
            }
            let Some(stem) = entry.path().file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            // Files are named `<art_id>-<size>.jpg`.
            let Some((art_id, _)) = stem.rsplit_once('-') else {
                continue;
            };
            if !live.contains(art_id) && std::fs::remove_file(entry.path()).is_ok() {
                removed += 1;
            }
        }

        if removed > 0 {
            tracing::info!("removed {removed} unreferenced cover files");
        }
        Ok(removed)
    }

    /// Compact the database after a large removal.
    /// Whether the database on disk is structurally sound.
    ///
    /// `quick_check` rather than `integrity_check`: the full check reads and
    /// verifies every index in the file, which on a large library is seconds
    /// of disk work. The quick form catches the damage that actually happens —
    /// a torn page, a truncated file — without the index cross-referencing,
    /// and is fast enough to run on startup after an unclean shutdown.
    pub fn is_intact(&self) -> Result<bool> {
        let verdict: String = self
            .connection
            .query_row("PRAGMA quick_check(1)", [], |row| row.get(0))?;
        Ok(verdict == "ok")
    }

    /// Fold the write-ahead log back into the database file.
    ///
    /// Worth doing on a clean exit: it keeps the WAL from being replayed on
    /// the next start, and leaves a single self-contained file behind — which
    /// is what makes the library safe to copy, back up, or carry on a stick in
    /// portable mode.
    pub fn checkpoint(&self) -> Result<()> {
        self.connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .context("checkpointing the library index")?;
        Ok(())
    }
}

/// A scan that can run on its own thread.
///
/// Holds no connection of its own until [`run`](Self::run) opens one, which is
/// what makes it `Send`.
pub struct Scanner {
    path: PathBuf,
    cache: ArtCache,
    options: ScanOptions,
}

impl Scanner {
    /// Open a private connection and scan. Blocks the calling thread.
    pub fn run(self, progress: &Progress) -> Result<Summary> {
        let mut connection = db::open(&self.path)?;
        ingest::scan(&mut connection, Some(&self.cache), &self.options, progress)
    }

    pub fn options(&self) -> &ScanOptions {
        &self.options
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A folder tree with real audio files is not something a unit test can
    /// conjure, so these exercise the index through its own API using files
    /// that are only recognised by extension. Tag reading is covered by the
    /// `library_probe` example against the real collection.
    fn fixture(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "resonance-lib-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_files(root: &Path, names: &[&str]) {
        for name in names {
            let path = root.join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, b"not decodable, but recognisable by extension").unwrap();
        }
    }

    fn options(root: &Path) -> ScanOptions {
        ScanOptions {
            roots: vec![root.to_path_buf()],
            // The fixture files have no readable duration, so nothing should be
            // filtered on length.
            min_duration: std::time::Duration::ZERO,
            extract_art: false,
            ..ScanOptions::default()
        }
    }

    // -- playlist interchange ------------------------------------------------

    /// A playlist has to survive leaving Resonance and coming back, or the
    /// export is decorative.
    #[test]
    fn a_playlist_round_trips_through_an_m3u_file() {
        let root = fixture("m3u-roundtrip");
        write_files(&root, &["a.mp3", "deep/b.flac", "c.mp3"]);

        let mut library = Library::in_memory().unwrap();
        library
            .scan_blocking(&options(&root), &Progress::new())
            .unwrap();

        let mut tracks: Vec<TrackId> = library
            .tracks(&Filter::default(), Order::Title, false)
            .unwrap()
            .iter()
            .map(|track| track.id)
            .collect();
        tracks.sort_unstable();
        assert_eq!(tracks.len(), 3);

        let original = library.create_playlist("Mixtape").unwrap();
        library.add_to_playlist(original, &tracks).unwrap();

        let file = root.join("Mixtape.m3u8");
        let written = library.export_playlist(original, &file).unwrap();
        assert_eq!(written, 3);
        assert!(file.is_file());

        let report = library.import_playlist(&file).unwrap();
        assert_eq!(report.added, 3);
        assert!(report.missing.is_empty(), "{:?}", report.missing);
        assert_eq!(report.name, "Mixtape");

        let mut imported: Vec<TrackId> = library
            .playlist_tracks(report.playlist)
            .unwrap()
            .iter()
            .map(|track| track.id)
            .collect();
        imported.sort_unstable();

        assert_eq!(imported, tracks, "the same tracks should come back");

        let _ = fs::remove_dir_all(&root);
    }

    /// An exported playlist must be portable, which means the paths in it are
    /// relative to the file itself wherever they can be.
    #[test]
    fn an_exported_playlist_uses_relative_paths() {
        let root = fixture("m3u-relative");
        write_files(&root, &["deep/b.flac"]);

        let mut library = Library::in_memory().unwrap();
        library
            .scan_blocking(&options(&root), &Progress::new())
            .unwrap();

        let tracks: Vec<TrackId> = library
            .tracks(&Filter::default(), Order::Title, false)
            .unwrap()
            .iter()
            .map(|track| track.id)
            .collect();

        let id = library.create_playlist("Portable").unwrap();
        library.add_to_playlist(id, &tracks).unwrap();

        let file = root.join("Portable.m3u8");
        library.export_playlist(id, &file).unwrap();

        let text = fs::read_to_string(&file).unwrap();
        assert!(text.contains("deep/b.flac"), "got:\n{text}");
        assert!(
            !text.contains(&root.to_string_lossy().replace('\\', "/")),
            "the absolute root leaked into the file:\n{text}"
        );

        let _ = fs::remove_dir_all(&root);
    }

    /// Tracks the index has never seen are reported, not quietly dropped.
    #[test]
    fn importing_reports_what_it_could_not_find() {
        let root = fixture("m3u-missing");
        write_files(&root, &["here.mp3"]);

        let mut library = Library::in_memory().unwrap();
        library
            .scan_blocking(&options(&root), &Progress::new())
            .unwrap();

        let file = root.join("Partial.m3u8");
        fs::write(
            &file,
            "#EXTM3U\r\n\
             #EXTINF:10,A - Here\r\nhere.mp3\r\n\
             #EXTINF:20,B - Gone\r\nsomewhere/gone.mp3\r\n",
        )
        .unwrap();

        let report = library.import_playlist(&file).unwrap();

        assert_eq!(report.added, 1);
        assert_eq!(report.missing.len(), 1);
        assert!(report.missing[0].ends_with("gone.mp3"));
        assert!(
            report.summary().contains("not in your library"),
            "the summary should say so: {}",
            report.summary()
        );

        let _ = fs::remove_dir_all(&root);
    }

    /// A playlist another program wrote spells the path differently. Matching
    /// on the filename is what rescues those.
    #[test]
    fn a_foreign_path_still_matches_by_filename() {
        let root = fixture("m3u-foreign");
        write_files(&root, &["deep/unique-name.mp3"]);

        let mut library = Library::in_memory().unwrap();
        library
            .scan_blocking(&options(&root), &Progress::new())
            .unwrap();

        let file = root.join("Foreign.m3u8");
        fs::write(&file, "Z:/some/other/mount/unique-name.mp3\r\n").unwrap();

        let report = library.import_playlist(&file).unwrap();

        assert_eq!(report.added, 1, "the filename should have matched");
        assert!(report.missing.is_empty());

        let _ = fs::remove_dir_all(&root);
    }

    /// But an ambiguous filename must not be guessed at — putting the wrong
    /// song in the playlist is worse than reporting it missing.
    #[test]
    fn an_ambiguous_filename_is_not_guessed() {
        let root = fixture("m3u-ambiguous");
        write_files(&root, &["one/same.mp3", "two/same.mp3"]);

        let mut library = Library::in_memory().unwrap();
        library
            .scan_blocking(&options(&root), &Progress::new())
            .unwrap();

        let file = root.join("Ambiguous.m3u8");
        fs::write(&file, "Z:/elsewhere/same.mp3\r\n").unwrap();

        let report = library.import_playlist(&file).unwrap();

        assert_eq!(report.added, 0);
        assert_eq!(report.missing.len(), 1);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_scan_indexes_what_it_finds_and_ignores_what_it_should() {
        let root = fixture("basic");
        write_files(
            &root,
            &["a.mp3", "sub/b.flac", "notes.txt", "unsupported.opus"],
        );

        let mut library = Library::in_memory().unwrap();
        let progress = Progress::new();
        let summary = library.scan_blocking(&options(&root), &progress).unwrap();

        assert_eq!(summary.added, 2, "two playable files");
        assert_eq!(summary.unplayable, 1, "the opus file is reported, not lost");

        let stats = library.stats().unwrap();
        assert_eq!(stats.tracks, 2);
        assert_eq!(stats.unplayable, 1);

        let _ = fs::remove_dir_all(&root);
    }

    /// The property the whole fingerprint scheme exists for.
    #[test]
    fn rescanning_an_unchanged_library_reads_nothing() {
        let root = fixture("incremental");
        write_files(&root, &["a.mp3", "b.mp3", "c.mp3"]);

        let mut library = Library::in_memory().unwrap();
        let progress = Progress::new();

        let first = library.scan_blocking(&options(&root), &progress).unwrap();
        assert_eq!(first.added, 3);

        let second = library.scan_blocking(&options(&root), &progress).unwrap();
        assert_eq!(second.added, 0);
        assert_eq!(second.updated, 0);
        assert_eq!(
            second.unchanged, 3,
            "nothing changed, so nothing was opened"
        );
        assert!(!second.changed_anything());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_deleted_file_leaves_the_library() {
        let root = fixture("deletion");
        write_files(&root, &["keep.mp3", "gone.mp3"]);

        let mut library = Library::in_memory().unwrap();
        let progress = Progress::new();
        library.scan_blocking(&options(&root), &progress).unwrap();
        assert_eq!(library.stats().unwrap().tracks, 2);

        fs::remove_file(root.join("gone.mp3")).unwrap();
        let summary = library.scan_blocking(&options(&root), &progress).unwrap();

        assert_eq!(summary.removed, 1);
        assert_eq!(library.stats().unwrap().tracks, 1);

        let _ = fs::remove_dir_all(&root);
    }

    /// An unplugged drive must not be mistaken for a deleted library.
    #[test]
    fn an_unavailable_folder_does_not_delete_its_tracks() {
        let root = fixture("offline");
        write_files(&root, &["a.mp3", "b.mp3"]);

        let mut library = Library::in_memory().unwrap();
        let progress = Progress::new();
        library.scan_blocking(&options(&root), &progress).unwrap();
        assert_eq!(library.stats().unwrap().tracks, 2);

        // Simulate the drive going away: the root no longer resolves.
        let _ = fs::remove_dir_all(&root);
        let summary = library.scan_blocking(&options(&root), &progress).unwrap();

        assert_eq!(summary.removed, 0, "an offline root must not prune");
        assert_eq!(summary.unreadable, 1);
        assert_eq!(library.stats().unwrap().tracks, 2);
    }

    #[test]
    fn untagged_files_still_get_a_readable_title() {
        let root = fixture("names");
        write_files(
            &root,
            &["Bitter Compass - Under Streetlights (Official Video) [aB3dEfGhIjK].mp3"],
        );

        let mut library = Library::in_memory().unwrap();
        let progress = Progress::new();
        library.scan_blocking(&options(&root), &progress).unwrap();

        let tracks = library.tracks(&Filter::All, Order::Title, false).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].title, "Under Streetlights");
        assert_eq!(tracks[0].artist, "Bitter Compass");
        assert!(!tracks[0].tagged, "the metadata came from the filename");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn search_finds_tracks_by_any_field() {
        let root = fixture("search");
        write_files(
            &root,
            &["Vellichor - Paper Lantern.mp3", "Someone - Other.mp3"],
        );

        let mut library = Library::in_memory().unwrap();
        let progress = Progress::new();
        library.scan_blocking(&options(&root), &progress).unwrap();

        assert_eq!(library.search("paper", None).unwrap().len(), 1);
        assert_eq!(library.search("velli", None).unwrap().len(), 1);
        assert_eq!(library.search("nothing here", None).unwrap().len(), 0);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn folders_are_grouped_with_their_counts() {
        let root = fixture("folders");
        write_files(&root, &["loose.mp3", "Calming/a.mp3", "Calming/b.mp3"]);

        let mut library = Library::in_memory().unwrap();
        let progress = Progress::new();
        library.scan_blocking(&options(&root), &progress).unwrap();

        let folders = library.folders().unwrap();
        assert_eq!(folders.len(), 2);
        let calming = folders.iter().find(|f| f.name == "Calming").unwrap();
        assert_eq!(calming.track_count, 2);

        let _ = fs::remove_dir_all(&root);
    }

    /// Removing a watched folder from the config must not leave its tracks
    /// behind as unplayable ghosts.
    #[test]
    fn dropping_a_root_removes_its_tracks() {
        let first = fixture("root-a");
        let second = fixture("root-b");
        write_files(&first, &["a.mp3"]);
        write_files(&second, &["b.mp3"]);

        let mut library = Library::in_memory().unwrap();
        let progress = Progress::new();

        let both = ScanOptions {
            roots: vec![first.clone(), second.clone()],
            min_duration: std::time::Duration::ZERO,
            extract_art: false,
            ..ScanOptions::default()
        };
        library.scan_blocking(&both, &progress).unwrap();
        assert_eq!(library.stats().unwrap().tracks, 2);

        library.scan_blocking(&options(&first), &progress).unwrap();
        assert_eq!(library.stats().unwrap().tracks, 1);

        let _ = fs::remove_dir_all(&first);
        let _ = fs::remove_dir_all(&second);
    }

    /// Cancelling has to leave the library exactly as it was, not half-written.
    #[test]
    fn a_cancelled_scan_writes_nothing() {
        let root = fixture("cancel");
        write_files(&root, &["a.mp3", "b.mp3"]);

        let mut library = Library::in_memory().unwrap();
        let progress = Progress::new();
        progress.cancel();

        let summary = library.scan_blocking(&options(&root), &progress).unwrap();

        assert!(summary.cancelled);
        assert_eq!(summary.added, 0);
        assert_eq!(library.stats().unwrap().tracks, 0);

        let _ = fs::remove_dir_all(&root);
    }
}
