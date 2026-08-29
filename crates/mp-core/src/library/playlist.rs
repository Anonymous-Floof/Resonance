//! Playlists, ordinary and smart.
//!
//! Both kinds are the same row. An ordinary playlist owns an explicit, ordered
//! list of tracks in `playlist_items`; a smart one owns a [`SmartRules`]
//! document and derives its tracks on every read. To everything that displays
//! them they are alike — a name, a description, and a list — which is why they
//! are not separate tables.
//!
//! # Positions
//!
//! Item positions are kept **dense and zero-based** after every mutation. That
//! costs a rewrite of the affected rows on each edit and buys two things worth
//! more: the UI can treat position as a list index without a translation step,
//! and a reorder is a single well-defined operation rather than a search for a
//! free gap. At playlist sizes — hundreds, not millions — the rewrite is
//! irrelevant.

use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

use super::model::{Track, TrackId};
use super::query;
use super::smart::SmartRules;

pub type PlaylistId = i64;

/// A playlist as a list row needs it.
#[derive(Debug, Clone)]
pub struct Playlist {
    pub id: PlaylistId,
    pub name: String,
    pub description: String,
    pub created_at: i64,
    pub updated_at: i64,
    /// `Some` when this is a smart playlist.
    pub rules: Option<SmartRules>,
    /// How many tracks it currently holds — evaluated, for a smart one.
    pub track_count: u32,
    pub total_duration: Duration,
}

impl Playlist {
    pub fn is_smart(&self) -> bool {
        self.rules.is_some()
    }

    /// Second line of a playlist row.
    pub fn subtitle(&self) -> String {
        let tracks = match self.track_count {
            1 => "1 track".to_owned(),
            other => format!("{other} tracks"),
        };

        if self.total_duration.is_zero() {
            tracks
        } else {
            format!("{tracks} · {}", human_duration(self.total_duration))
        }
    }
}

/// A duration as "1 hr 12 min" or "4 min".
fn human_duration(duration: Duration) -> String {
    let minutes = duration.as_secs() / 60;

    if minutes >= 60 {
        format!("{} hr {} min", minutes / 60, minutes % 60)
    } else {
        format!("{minutes} min")
    }
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// Every playlist, newest edit first.
pub fn list(connection: &Connection, now: i64) -> Result<Vec<Playlist>> {
    let mut statement = connection.prepare(
        "SELECT id, name, description, created_at, updated_at, rules
           FROM playlists
          ORDER BY updated_at DESC, name COLLATE NOCASE",
    )?;

    let rows: Vec<(PlaylistId, String, String, i64, i64, Option<String>)> = statement
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut out = Vec::with_capacity(rows.len());
    for (id, name, description, created_at, updated_at, rules) in rows {
        out.push(hydrate(
            connection,
            id,
            name,
            description,
            created_at,
            updated_at,
            rules,
            now,
        )?);
    }

    Ok(out)
}

/// One playlist by id.
pub fn get(connection: &Connection, id: PlaylistId, now: i64) -> Result<Option<Playlist>> {
    let row: Option<(String, String, i64, i64, Option<String>)> = connection
        .query_row(
            "SELECT name, description, created_at, updated_at, rules
               FROM playlists WHERE id = ?1",
            params![id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;

    let Some((name, description, created_at, updated_at, rules)) = row else {
        return Ok(None);
    };

    Ok(Some(hydrate(
        connection,
        id,
        name,
        description,
        created_at,
        updated_at,
        rules,
        now,
    )?))
}

/// Fill in the counts, evaluating the rules for a smart playlist.
#[allow(clippy::too_many_arguments)]
fn hydrate(
    connection: &Connection,
    id: PlaylistId,
    name: String,
    description: String,
    created_at: i64,
    updated_at: i64,
    rules: Option<String>,
    now: i64,
) -> Result<Playlist> {
    // A rule document that will not parse — written by a newer build, or
    // corrupted — degrades the playlist to an empty one rather than failing the
    // whole list and taking every other playlist down with it.
    let rules = rules.and_then(|text| match SmartRules::from_json(&text) {
        Ok(rules) => Some(rules),
        Err(err) => {
            tracing::warn!("playlist {id} has unreadable rules, ignoring them: {err}");
            None
        }
    });

    let (track_count, total_ms) = match &rules {
        Some(rules) => smart_totals(connection, rules, now)?,
        None => manual_totals(connection, id)?,
    };

    Ok(Playlist {
        id,
        name,
        description,
        created_at,
        updated_at,
        rules,
        track_count,
        total_duration: Duration::from_millis(total_ms.max(0) as u64),
    })
}

fn manual_totals(connection: &Connection, id: PlaylistId) -> Result<(u32, i64)> {
    let row = connection.query_row(
        "SELECT count(*), COALESCE(sum(t.duration_ms), 0)
           FROM playlist_items i
           JOIN tracks t ON t.id = i.track_id
          WHERE i.playlist_id = ?1",
        params![id],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
    )?;

    Ok((row.0.max(0) as u32, row.1))
}

fn smart_totals(connection: &Connection, rules: &SmartRules, now: i64) -> Result<(u32, i64)> {
    let compiled = rules.to_sql(now);

    // The count respects the limit: a "50 tracks" playlist holds fifty, and
    // reporting the number that merely *matched* would disagree with the list
    // the user is looking at.
    let sql = format!(
        "SELECT count(*), COALESCE(sum(duration_ms), 0) FROM (
             SELECT t.duration_ms AS duration_ms
               FROM tracks t
               LEFT JOIN artists ar ON ar.id = t.artist_id
               LEFT JOIN albums  al ON al.id = t.album_id
              WHERE {}
              {}
         )",
        compiled.where_clause,
        limit_clause(rules),
    );

    let row = connection.query_row(&sql, params_from_iter(compiled.params.iter()), |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
    })?;

    Ok((row.0.max(0) as u32, row.1))
}

/// `ORDER BY ... LIMIT ...` for a smart playlist, or empty when unlimited.
///
/// The ordering only matters when there is a limit — which fifty tracks you get
/// is the whole difference between "50 favourites" and "50 least played" — so
/// it is emitted together with it.
fn limit_clause(rules: &SmartRules) -> String {
    match rules.limit {
        Some(limit) if limit > 0 => {
            format!("ORDER BY {} LIMIT {}", rules.order.sql(false), limit)
        }
        _ => String::new(),
    }
}

/// The tracks in a playlist, in playing order.
pub fn tracks(connection: &Connection, id: PlaylistId, now: i64) -> Result<Vec<Track>> {
    let Some(playlist) = get(connection, id, now)? else {
        return Ok(Vec::new());
    };

    match &playlist.rules {
        Some(rules) => smart_tracks(connection, rules, now),
        None => manual_tracks(connection, id),
    }
}

fn manual_tracks(connection: &Connection, id: PlaylistId) -> Result<Vec<Track>> {
    let sql = format!(
        "SELECT {} {} JOIN playlist_items pi ON pi.track_id = t.id
          WHERE pi.playlist_id = ?1
          ORDER BY pi.position",
        query::TRACK_COLUMNS,
        query::TRACK_JOINS,
    );

    let mut statement = connection.prepare(&sql)?;
    let rows = statement
        .query_map(params![id], query::track_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(rows)
}

fn smart_tracks(connection: &Connection, rules: &SmartRules, now: i64) -> Result<Vec<Track>> {
    let compiled = rules.to_sql(now);

    let sql = format!(
        "SELECT {} {} WHERE {} ORDER BY {} {}",
        query::TRACK_COLUMNS,
        query::TRACK_JOINS,
        compiled.where_clause,
        rules.order.sql(false),
        match rules.limit {
            Some(limit) if limit > 0 => format!("LIMIT {limit}"),
            _ => String::new(),
        }
    );

    let mut statement = connection.prepare(&sql)?;
    let rows = statement
        .query_map(
            params_from_iter(compiled.params.iter()),
            query::track_from_row,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(rows)
}

/// Whether a manual playlist already holds this track.
pub fn contains(connection: &Connection, id: PlaylistId, track: TrackId) -> Result<bool> {
    let found: Option<i64> = connection
        .query_row(
            "SELECT 1 FROM playlist_items WHERE playlist_id = ?1 AND track_id = ?2 LIMIT 1",
            params![id, track],
            |row| row.get(0),
        )
        .optional()?;

    Ok(found.is_some())
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

/// Create an ordinary playlist and return its id.
pub fn create(connection: &Connection, name: &str, now: i64) -> Result<PlaylistId> {
    insert(connection, name, "", None, now)
}

/// Create a smart playlist.
pub fn create_smart(
    connection: &Connection,
    name: &str,
    rules: &SmartRules,
    now: i64,
) -> Result<PlaylistId> {
    let json = rules.to_json().context("encoding the playlist rules")?;
    insert(connection, name, "", Some(json), now)
}

fn insert(
    connection: &Connection,
    name: &str,
    description: &str,
    rules: Option<String>,
    now: i64,
) -> Result<PlaylistId> {
    connection.execute(
        "INSERT INTO playlists (name, description, created_at, updated_at, rules)
         VALUES (?1, ?2, ?3, ?3, ?4)",
        params![clean_name(name), description, now, rules],
    )?;

    Ok(connection.last_insert_rowid())
}

/// A playlist name that is safe to display.
///
/// Trimmed, capped, and never empty — an unnamed row in the sidebar is
/// unclickable in practice because there is nothing to aim at.
fn clean_name(name: &str) -> String {
    let trimmed = name.trim();

    if trimmed.is_empty() {
        return "Untitled playlist".to_owned();
    }

    trimmed.chars().take(200).collect()
}

pub fn rename(connection: &Connection, id: PlaylistId, name: &str, now: i64) -> Result<()> {
    connection.execute(
        "UPDATE playlists SET name = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, clean_name(name), now],
    )?;
    Ok(())
}

pub fn set_description(
    connection: &Connection,
    id: PlaylistId,
    description: &str,
    now: i64,
) -> Result<()> {
    connection.execute(
        "UPDATE playlists SET description = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, description.trim(), now],
    )?;
    Ok(())
}

/// Replace a smart playlist's rules.
pub fn set_rules(
    connection: &Connection,
    id: PlaylistId,
    rules: &SmartRules,
    now: i64,
) -> Result<()> {
    let json = rules.to_json().context("encoding the playlist rules")?;
    connection.execute(
        "UPDATE playlists SET rules = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, json, now],
    )?;
    Ok(())
}

pub fn delete(connection: &Connection, id: PlaylistId) -> Result<()> {
    // Items go with it through the foreign key's cascade.
    connection.execute("DELETE FROM playlists WHERE id = ?1", params![id])?;
    Ok(())
}

/// Append tracks to the end of a playlist.
///
/// Returns how many were actually added. Duplicates within the playlist are
/// allowed — the same track twice in a set is a real thing to want — but a
/// track that no longer exists is skipped rather than inserted as a dangling
/// reference.
pub fn add_tracks(
    connection: &mut Connection,
    id: PlaylistId,
    track_ids: &[TrackId],
    now: i64,
) -> Result<usize> {
    if track_ids.is_empty() {
        return Ok(0);
    }

    let transaction = connection.transaction()?;
    let mut next = next_position(&transaction, id)?;
    let mut added = 0;

    {
        let mut statement = transaction.prepare(
            "INSERT INTO playlist_items (playlist_id, track_id, position, added_at)
             SELECT ?1, ?2, ?3, ?4 WHERE EXISTS (SELECT 1 FROM tracks WHERE id = ?2)",
        )?;

        for track in track_ids {
            if statement.execute(params![id, track, next, now])? > 0 {
                next += 1;
                added += 1;
            }
        }
    }

    touch(&transaction, id, now)?;
    transaction.commit()?;

    Ok(added)
}

fn next_position(connection: &Connection, id: PlaylistId) -> Result<i64> {
    let highest: Option<i64> = connection.query_row(
        "SELECT max(position) FROM playlist_items WHERE playlist_id = ?1",
        params![id],
        |row| row.get(0),
    )?;

    Ok(highest.map_or(0, |value| value + 1))
}

/// Remove the item at `position`, closing the gap behind it.
pub fn remove_at(
    connection: &mut Connection,
    id: PlaylistId,
    position: usize,
    now: i64,
) -> Result<()> {
    let transaction = connection.transaction()?;

    transaction.execute(
        "DELETE FROM playlist_items WHERE playlist_id = ?1 AND position = ?2",
        params![id, position as i64],
    )?;

    renumber(&transaction, id)?;
    touch(&transaction, id, now)?;
    transaction.commit()?;

    Ok(())
}

/// Remove every copy of a track from a playlist.
pub fn remove_track(
    connection: &mut Connection,
    id: PlaylistId,
    track: TrackId,
    now: i64,
) -> Result<()> {
    let transaction = connection.transaction()?;

    transaction.execute(
        "DELETE FROM playlist_items WHERE playlist_id = ?1 AND track_id = ?2",
        params![id, track],
    )?;

    renumber(&transaction, id)?;
    touch(&transaction, id, now)?;
    transaction.commit()?;

    Ok(())
}

/// Move the item at `from` so that it sits at `to`.
///
/// Both are list indices as the UI sees them, and out-of-range values are
/// clamped rather than rejected: a drag that ends past the last row means "put
/// it at the end", which is what the gesture looks like it should do.
pub fn move_item(
    connection: &mut Connection,
    id: PlaylistId,
    from: usize,
    to: usize,
    now: i64,
) -> Result<()> {
    let transaction = connection.transaction()?;

    let mut ids: Vec<i64> = {
        let mut statement = transaction
            .prepare("SELECT id FROM playlist_items WHERE playlist_id = ?1 ORDER BY position")?;
        statement
            .query_map(params![id], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };

    if ids.is_empty() {
        return Ok(());
    }

    let from = from.min(ids.len() - 1);
    let to = to.min(ids.len() - 1);

    if from != to {
        let moved = ids.remove(from);
        ids.insert(to, moved);

        let mut statement =
            transaction.prepare("UPDATE playlist_items SET position = ?2 WHERE id = ?1")?;
        for (position, item) in ids.iter().enumerate() {
            statement.execute(params![item, position as i64])?;
        }
    }

    touch(&transaction, id, now)?;
    transaction.commit()?;

    Ok(())
}

pub fn clear(connection: &mut Connection, id: PlaylistId, now: i64) -> Result<()> {
    let transaction = connection.transaction()?;
    transaction.execute(
        "DELETE FROM playlist_items WHERE playlist_id = ?1",
        params![id],
    )?;
    touch(&transaction, id, now)?;
    transaction.commit()?;
    Ok(())
}

/// Close any gaps in the position sequence.
fn renumber(connection: &Connection, id: PlaylistId) -> Result<()> {
    // Rewriting through a window function keeps this one statement rather than
    // a read, a loop and N updates.
    connection.execute(
        "UPDATE playlist_items
            SET position = (
                SELECT rank - 1 FROM (
                    SELECT id AS row_id,
                           row_number() OVER (ORDER BY position) AS rank
                      FROM playlist_items
                     WHERE playlist_id = ?1
                ) WHERE row_id = playlist_items.id
            )
          WHERE playlist_id = ?1",
        params![id],
    )?;

    Ok(())
}

fn touch(connection: &Connection, id: PlaylistId, now: i64) -> Result<()> {
    connection.execute(
        "UPDATE playlists SET updated_at = ?2 WHERE id = ?1",
        params![id, now],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// History
// ---------------------------------------------------------------------------

/// Record that a track was played.
///
/// Writes both the counter on `tracks` and a row in `play_history`: the counter
/// answers "how often", the history answers "when", and recency-aware shuffle
/// needs the second one.
pub fn record_play(connection: &Connection, track: TrackId, now: i64) -> Result<()> {
    query::record_play(connection, track, now)?;

    connection.execute(
        "INSERT INTO play_history (track_id, played_at) VALUES (?1, ?2)",
        params![track, now],
    )?;

    Ok(())
}

/// Add to the time a track has been listened to.
///
/// Deliberately separate from [`record_play`]. A play is a threshold decision
/// made once; listening is a quantity that keeps growing for as long as the
/// audio keeps playing, and time spent on a track that was skipped before it
/// counted as a play is still time spent listening.
///
/// Callers accumulate in memory and flush periodically, so this is an
/// occasional indexed update rather than something on a hot path.
pub fn add_listening(connection: &Connection, track: TrackId, seconds: f64) -> Result<()> {
    if !seconds.is_finite() || seconds <= 0.0 {
        return Ok(());
    }

    // Milliseconds, so the running total stays an exact integer no matter how
    // many thousands of small flushes accumulate into it.
    let millis = (seconds * 1000.0).round() as i64;
    connection.execute(
        "UPDATE tracks SET listened_ms = listened_ms + ?2 WHERE id = ?1",
        params![track, millis],
    )?;

    Ok(())
}

/// Trim the history to the most recent `keep` entries.
///
/// The history is only ever read for recency, so an unbounded log of every play
/// forever is a slowly growing file that answers no question the last few
/// thousand entries do not.
pub fn trim_history(connection: &Connection, keep: usize) -> Result<usize> {
    let removed = connection.execute(
        "DELETE FROM play_history
          WHERE id NOT IN (
              SELECT id FROM play_history ORDER BY played_at DESC, id DESC LIMIT ?1
          )",
        params![keep as i64],
    )?;

    Ok(removed)
}

/// Track ids played most recently, newest first.
pub fn recently_played(connection: &Connection, limit: usize) -> Result<Vec<TrackId>> {
    let mut statement = connection.prepare(
        "SELECT DISTINCT track_id FROM play_history
          ORDER BY played_at DESC, id DESC LIMIT ?1",
    )?;

    let rows = statement
        .query_map(params![limit as i64], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;

    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::db;
    use crate::library::model::Order;
    use crate::library::smart::{Field, Group, Node, Op, Rule};

    const NOW: i64 = 1_700_000_000;

    /// A library with a handful of tracks to arrange.
    fn fixture() -> Connection {
        let db = db::open_in_memory().unwrap();

        db.execute_batch(
            "INSERT INTO artists (id, name, sort_name) VALUES
                 (1, 'Alpha', 'alpha'), (2, 'Beta', 'beta');
             INSERT INTO albums (id, title, sort_title, artist_id) VALUES
                 (1, 'First', 'first', 1);",
        )
        .unwrap();

        for id in 1..=5 {
            db.execute(
                "INSERT INTO tracks (
                     id, path, folder, file_name, mtime, size,
                     title, sort_title, artist_id, album_id,
                     duration_ms, play_count, added_at, last_seen_at
                 ) VALUES (?1, ?2, '/m', ?3, 1, 2, ?4, ?5, ?6, 1, ?7, ?8, ?9, 0)",
                params![
                    id,
                    format!("/m/{id}.mp3"),
                    format!("{id}.mp3"),
                    format!("Track {id}"),
                    format!("track {id}"),
                    if id <= 3 { 1 } else { 2 },
                    60_000 * id,
                    id,
                    NOW - id * 86_400,
                ],
            )
            .unwrap();
        }

        db
    }

    fn positions(db: &Connection, id: PlaylistId) -> Vec<(i64, i64)> {
        let mut statement = db
            .prepare("SELECT position, track_id FROM playlist_items WHERE playlist_id = ?1 ORDER BY position")
            .unwrap();
        statement
            .query_map(params![id], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn a_new_playlist_is_empty_and_listed() {
        let db = fixture();
        let id = create(&db, "Evening", NOW).unwrap();

        let all = list(&db, NOW).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, id);
        assert_eq!(all[0].name, "Evening");
        assert_eq!(all[0].track_count, 0);
        assert!(!all[0].is_smart());
    }

    #[test]
    fn tracks_are_appended_in_the_order_given() {
        let mut db = fixture();
        let id = create(&db, "Set", NOW).unwrap();

        assert_eq!(add_tracks(&mut db, id, &[3, 1, 2], NOW).unwrap(), 3);

        assert_eq!(positions(&db, id), vec![(0, 3), (1, 1), (2, 2)]);

        let listed = tracks(&db, id, NOW).unwrap();
        let ids: Vec<TrackId> = listed.iter().map(|track| track.id).collect();
        assert_eq!(ids, vec![3, 1, 2], "playlist order was not preserved");
    }

    /// The same track twice in one playlist is a legitimate thing to want.
    #[test]
    fn a_track_may_appear_more_than_once() {
        let mut db = fixture();
        let id = create(&db, "Set", NOW).unwrap();

        add_tracks(&mut db, id, &[1, 2, 1], NOW).unwrap();

        assert_eq!(positions(&db, id).len(), 3);
        assert!(contains(&db, id, 1).unwrap());
    }

    /// A track id that does not exist must not be inserted as a dangling row.
    #[test]
    fn adding_a_missing_track_is_skipped_rather_than_stored() {
        let mut db = fixture();
        let id = create(&db, "Set", NOW).unwrap();

        let added = add_tracks(&mut db, id, &[1, 9_999, 2], NOW).unwrap();

        assert_eq!(added, 2, "the missing track was counted as added");
        assert_eq!(positions(&db, id), vec![(0, 1), (1, 2)]);
    }

    /// Positions stay dense, so the UI can treat them as list indices.
    #[test]
    fn removing_an_item_closes_the_gap() {
        let mut db = fixture();
        let id = create(&db, "Set", NOW).unwrap();
        add_tracks(&mut db, id, &[1, 2, 3, 4], NOW).unwrap();

        remove_at(&mut db, id, 1, NOW).unwrap();

        assert_eq!(positions(&db, id), vec![(0, 1), (1, 3), (2, 4)]);
    }

    #[test]
    fn removing_a_track_removes_every_copy_of_it() {
        let mut db = fixture();
        let id = create(&db, "Set", NOW).unwrap();
        add_tracks(&mut db, id, &[1, 2, 1, 3, 1], NOW).unwrap();

        remove_track(&mut db, id, 1, NOW).unwrap();

        assert_eq!(positions(&db, id), vec![(0, 2), (1, 3)]);
    }

    #[test]
    fn an_item_can_be_dragged_to_a_new_place() {
        let mut db = fixture();
        let id = create(&db, "Set", NOW).unwrap();
        add_tracks(&mut db, id, &[1, 2, 3, 4], NOW).unwrap();

        // Move the first to the end.
        move_item(&mut db, id, 0, 3, NOW).unwrap();
        assert_eq!(positions(&db, id), vec![(0, 2), (1, 3), (2, 4), (3, 1)]);

        // And back to the front.
        move_item(&mut db, id, 3, 0, NOW).unwrap();
        assert_eq!(positions(&db, id), vec![(0, 1), (1, 2), (2, 3), (3, 4)]);
    }

    /// A drag that ends past the last row means "put it at the end".
    #[test]
    fn dragging_past_the_end_clamps_instead_of_failing() {
        let mut db = fixture();
        let id = create(&db, "Set", NOW).unwrap();
        add_tracks(&mut db, id, &[1, 2, 3], NOW).unwrap();

        move_item(&mut db, id, 0, 99, NOW).unwrap();
        assert_eq!(positions(&db, id), vec![(0, 2), (1, 3), (2, 1)]);

        // And an empty playlist must not panic.
        let empty = create(&db, "Empty", NOW).unwrap();
        move_item(&mut db, empty, 0, 5, NOW).unwrap();
    }

    #[test]
    fn deleting_a_playlist_takes_its_items_with_it() {
        let mut db = fixture();
        let id = create(&db, "Set", NOW).unwrap();
        add_tracks(&mut db, id, &[1, 2], NOW).unwrap();

        delete(&db, id).unwrap();

        assert!(list(&db, NOW).unwrap().is_empty());
        let left: i64 = db
            .query_row("SELECT count(*) FROM playlist_items", [], |row| row.get(0))
            .unwrap();
        assert_eq!(left, 0);
    }

    /// Losing a file should shorten the playlist, not break it.
    #[test]
    fn deleting_a_track_removes_it_from_playlists() {
        let mut db = fixture();
        let id = create(&db, "Set", NOW).unwrap();
        add_tracks(&mut db, id, &[1, 2, 3], NOW).unwrap();

        db.execute("DELETE FROM tracks WHERE id = 2", []).unwrap();

        let listed = tracks(&db, id, NOW).unwrap();
        let ids: Vec<TrackId> = listed.iter().map(|track| track.id).collect();
        assert_eq!(ids, vec![1, 3]);
    }

    #[test]
    fn totals_add_up() {
        let mut db = fixture();
        let id = create(&db, "Set", NOW).unwrap();
        add_tracks(&mut db, id, &[1, 2, 3], NOW).unwrap();

        let playlist = get(&db, id, NOW).unwrap().unwrap();
        assert_eq!(playlist.track_count, 3);
        // 60 + 120 + 180 seconds.
        assert_eq!(playlist.total_duration, Duration::from_secs(360));
        assert_eq!(playlist.subtitle(), "3 tracks · 6 min");
    }

    #[test]
    fn a_nameless_playlist_still_gets_a_name() {
        let db = fixture();
        let id = create(&db, "   ", NOW).unwrap();

        assert_eq!(
            get(&db, id, NOW).unwrap().unwrap().name,
            "Untitled playlist"
        );
    }

    // -- smart playlists ---------------------------------------------------

    fn smart(nodes: Vec<Node>, limit: Option<u32>, order: Order) -> SmartRules {
        SmartRules {
            root: Group::all(nodes),
            limit,
            order,
        }
    }

    #[test]
    fn a_smart_playlist_finds_its_tracks_by_rule() {
        let db = fixture();

        let rules = smart(
            vec![Node::Rule(Rule::new(Field::Artist, Op::Is, "Alpha"))],
            None,
            Order::Title,
        );
        let id = create_smart(&db, "By Alpha", &rules, NOW).unwrap();

        let playlist = get(&db, id, NOW).unwrap().unwrap();
        assert!(playlist.is_smart());
        assert_eq!(playlist.track_count, 3);

        let ids: Vec<TrackId> = tracks(&db, id, NOW)
            .unwrap()
            .iter()
            .map(|track| track.id)
            .collect();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    /// The count has to agree with the list, which means it has to respect the
    /// limit rather than reporting everything that matched.
    #[test]
    fn a_limited_smart_playlist_counts_what_it_shows() {
        let db = fixture();

        let rules = smart(vec![], Some(2), Order::Title);
        let id = create_smart(&db, "Any two", &rules, NOW).unwrap();

        let playlist = get(&db, id, NOW).unwrap().unwrap();
        assert_eq!(playlist.track_count, 2);
        assert_eq!(tracks(&db, id, NOW).unwrap().len(), 2);
    }

    /// The whole point of a smart playlist: it follows the library.
    #[test]
    fn a_smart_playlist_updates_when_the_library_does() {
        let db = fixture();

        let rules = smart(
            vec![Node::Rule(Rule::new(Field::Artist, Op::Is, "Beta"))],
            None,
            Order::Title,
        );
        let id = create_smart(&db, "By Beta", &rules, NOW).unwrap();
        assert_eq!(get(&db, id, NOW).unwrap().unwrap().track_count, 2);

        db.execute("UPDATE tracks SET artist_id = 2 WHERE id = 1", [])
            .unwrap();

        assert_eq!(
            get(&db, id, NOW).unwrap().unwrap().track_count,
            3,
            "the smart playlist did not follow the change"
        );
    }

    #[test]
    fn smart_rules_can_be_replaced() {
        let db = fixture();

        let id = create_smart(&db, "Changing", &smart(vec![], None, Order::Title), NOW).unwrap();
        assert_eq!(get(&db, id, NOW).unwrap().unwrap().track_count, 5);

        set_rules(
            &db,
            id,
            &smart(
                vec![Node::Rule(Rule::new(
                    Field::PlayCount,
                    Op::GreaterThan,
                    "3",
                ))],
                None,
                Order::Title,
            ),
            NOW,
        )
        .unwrap();

        assert_eq!(get(&db, id, NOW).unwrap().unwrap().track_count, 2);
    }

    /// A rule document written by a newer build must not take down the whole
    /// playlist list.
    #[test]
    fn unreadable_rules_degrade_to_an_empty_playlist() {
        let db = fixture();
        let id = create(&db, "Broken", NOW).unwrap();

        db.execute(
            "UPDATE playlists SET rules = '{not valid json' WHERE id = ?1",
            params![id],
        )
        .unwrap();

        let all = list(&db, NOW).unwrap();
        assert_eq!(all.len(), 1, "one bad playlist hid the rest");
        assert!(!all[0].is_smart());
    }

    // -- history -----------------------------------------------------------

    #[test]
    fn playing_a_track_records_both_a_count_and_a_time() {
        let db = fixture();

        record_play(&db, 1, NOW).unwrap();
        record_play(&db, 1, NOW + 60).unwrap();

        let count: i64 = db
            .query_row("SELECT play_count FROM tracks WHERE id = 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            count, 3,
            "the counter did not advance from its fixture value"
        );

        let entries: i64 = db
            .query_row("SELECT count(*) FROM play_history", [], |row| row.get(0))
            .unwrap();
        assert_eq!(entries, 2);
    }

    #[test]
    fn recent_plays_come_back_newest_first_and_deduplicated() {
        let db = fixture();

        record_play(&db, 1, NOW).unwrap();
        record_play(&db, 2, NOW + 10).unwrap();
        record_play(&db, 1, NOW + 20).unwrap();

        assert_eq!(recently_played(&db, 10).unwrap(), vec![1, 2]);
    }

    #[test]
    fn the_history_is_trimmed_to_the_most_recent_entries() {
        let db = fixture();

        for step in 0..50 {
            record_play(&db, (step % 5) + 1, NOW + step).unwrap();
        }

        let removed = trim_history(&db, 10).unwrap();
        assert_eq!(removed, 40);

        let left: i64 = db
            .query_row("SELECT count(*) FROM play_history", [], |row| row.get(0))
            .unwrap();
        assert_eq!(left, 10);

        // And what survived is the newest.
        let oldest: i64 = db
            .query_row("SELECT min(played_at) FROM play_history", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(oldest, NOW + 40);
    }
}
