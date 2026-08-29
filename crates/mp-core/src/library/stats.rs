//! Listening statistics.
//!
//! The queries live here rather than in the UI so they stay testable and the
//! shell keeps holding no SQL.
//!
//! Two different facts are recorded about listening, and keeping them apart is
//! the whole design:
//!
//! * **Plays** are a threshold decision, made once per listen: did this run
//!   long enough to count? They live in `tracks.play_count`, which nothing ever
//!   trims, and in `play_history`, which records *when* each one happened.
//! * **Listening time** is a quantity that grows for every second that
//!   actually plays, whether or not the listen ever reached the threshold. It
//!   lives in `tracks.listened_ms`.
//!
//! An earlier version answered both questions with one number and got the
//! second badly wrong — see the note on the V5 migration in [`super::db`].
//! Totals now come from `tracks`, which is never trimmed, so history being
//! trimmed cannot erase them.

use anyhow::Result;
use rusqlite::{Connection, params};

use super::model::Track;
use super::query::{TRACK_COLUMNS, TRACK_JOINS, track_from_row};

/// Seconds in a day, for bucketing history by date.
const DAY: i64 = 86_400;

/// A library-wide summary.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Totals {
    pub tracks: u64,
    pub artists: u64,
    pub albums: u64,
    /// Combined duration of everything indexed.
    pub library_secs: f64,

    /// Distinct tracks played at least once.
    pub tracks_played: u64,
    /// Lifetime plays, from the per-track counters.
    pub plays: u64,
    /// Time actually spent listening.
    pub listened_secs: f64,

    /// Plays on tracks that carry no listening time at all.
    ///
    /// These are listens recorded before listening was measured, or imported
    /// from a bundle, which carries counts but no durations. Reported so the
    /// page can say the total is a floor rather than quietly under-reporting.
    pub unmeasured_plays: u64,
    /// Combined duration of those plays' tracks — an upper bound on what they
    /// would have contributed.
    pub unmeasured_secs: f64,

    pub first_play: Option<i64>,
    pub last_play: Option<i64>,
    /// Distinct days on which anything was played.
    pub active_days: u64,
}

impl Totals {
    /// Whether there is any history worth rendering.
    pub fn has_history(&self) -> bool {
        self.plays > 0 || self.listened_secs > 0.0
    }

    /// Share of the library that has ever been played, `0.0..=1.0`.
    pub fn explored(&self) -> f32 {
        if self.tracks == 0 {
            return 0.0;
        }
        self.tracks_played as f32 / self.tracks as f32
    }
}

/// One row of a "most played" ranking.
#[derive(Debug, Clone, PartialEq)]
pub struct Ranked {
    /// The artist or album this row is for, so it can be opened.
    pub id: i64,
    /// Artist or album name.
    pub name: String,
    /// Second line: the album's artist, or empty on an artist row.
    pub detail: String,
    pub plays: u64,
    pub secs: f64,
    pub art_id: Option<String>,
}

/// A track alongside how long it has actually been listened to.
#[derive(Debug, Clone)]
pub struct PlayedTrack {
    pub track: Track,
    pub secs: f64,
}

/// Everything the summary strip needs, in four queries.
pub fn totals(connection: &Connection) -> Result<Totals> {
    let (tracks, library_ms, tracks_played, plays, listened_ms): (i64, i64, i64, i64, i64) =
        connection.query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(duration_ms), 0),
                    COUNT(CASE WHEN play_count > 0 THEN 1 END),
                    COALESCE(SUM(play_count), 0),
                    COALESCE(SUM(listened_ms), 0)
               FROM tracks",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )?;

    // Only names that actually carry a track. An artist row left behind by a
    // removed file is not something the user has in their library.
    let (artists, albums): (i64, i64) = connection.query_row(
        "SELECT (SELECT COUNT(*) FROM artists ar
                  WHERE EXISTS (SELECT 1 FROM tracks WHERE artist_id = ar.id)),
                (SELECT COUNT(*) FROM albums al
                  WHERE EXISTS (SELECT 1 FROM tracks WHERE album_id = al.id))",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    let (first_play, last_play, active_days): (Option<i64>, Option<i64>, i64) = connection
        .query_row(
            "SELECT MIN(played_at), MAX(played_at), COUNT(DISTINCT played_at / 86400)
               FROM play_history",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;

    let (unmeasured_plays, unmeasured_ms): (i64, i64) = connection.query_row(
        "SELECT COALESCE(SUM(play_count), 0),
                COALESCE(SUM(duration_ms * play_count), 0)
           FROM tracks
          WHERE play_count > 0 AND listened_ms = 0",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    Ok(Totals {
        tracks: tracks as u64,
        artists: artists as u64,
        albums: albums as u64,
        library_secs: library_ms as f64 / 1000.0,
        tracks_played: tracks_played as u64,
        plays: plays as u64,
        listened_secs: listened_ms as f64 / 1000.0,
        unmeasured_plays: unmeasured_plays as u64,
        unmeasured_secs: unmeasured_ms as f64 / 1000.0,
        first_play,
        last_play,
        active_days: active_days as u64,
    })
}

/// The most played tracks, best first.
pub fn top_tracks(connection: &Connection, limit: usize) -> Result<Vec<PlayedTrack>> {
    let sql = format!(
        "SELECT {TRACK_COLUMNS}, t.listened_ms
         {TRACK_JOINS}
         WHERE t.play_count > 0 OR t.listened_ms > 0
         ORDER BY t.play_count DESC, t.listened_ms DESC, t.sort_title
         LIMIT ?1"
    );

    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(params![limit as i64], |row| {
        Ok(PlayedTrack {
            track: track_from_row(row)?,
            // One past the last column of TRACK_COLUMNS.
            secs: row.get::<_, i64>(14)? as f64 / 1000.0,
        })
    })?;

    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The most played artists, best first.
pub fn top_artists(connection: &Connection, limit: usize) -> Result<Vec<Ranked>> {
    let mut statement = connection.prepare(
        "SELECT ar.id,
                ar.name,
                SUM(t.play_count),
                COALESCE(SUM(t.listened_ms), 0),
                MAX(COALESCE(t.art_id, al.art_id))
           FROM tracks t
           JOIN artists ar ON ar.id = t.artist_id
           LEFT JOIN albums al ON al.id = t.album_id
          WHERE t.play_count > 0 OR t.listened_ms > 0
          GROUP BY ar.id
          ORDER BY 3 DESC, 4 DESC, ar.sort_name
          LIMIT ?1",
    )?;

    let rows = statement.query_map(params![limit as i64], |row| {
        Ok(Ranked {
            id: row.get(0)?,
            name: row.get(1)?,
            detail: String::new(),
            plays: row.get::<_, i64>(2)? as u64,
            secs: row.get::<_, i64>(3)? as f64 / 1000.0,
            art_id: row.get(4)?,
        })
    })?;

    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The most played albums, best first.
pub fn top_albums(connection: &Connection, limit: usize) -> Result<Vec<Ranked>> {
    let mut statement = connection.prepare(
        "SELECT al.id,
                al.title,
                COALESCE(ar.name, ''),
                SUM(t.play_count),
                COALESCE(SUM(t.listened_ms), 0),
                MAX(COALESCE(al.art_id, t.art_id))
           FROM tracks t
           JOIN albums al ON al.id = t.album_id
           LEFT JOIN artists ar ON ar.id = t.artist_id
          WHERE t.play_count > 0 OR t.listened_ms > 0
          GROUP BY al.id
          ORDER BY 4 DESC, 5 DESC, al.sort_title
          LIMIT ?1",
    )?;

    let rows = statement.query_map(params![limit as i64], |row| {
        Ok(Ranked {
            id: row.get(0)?,
            name: row.get(1)?,
            detail: row.get(2)?,
            plays: row.get::<_, i64>(3)? as u64,
            secs: row.get::<_, i64>(4)? as f64 / 1000.0,
            art_id: row.get(5)?,
        })
    })?;

    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The tracks played most recently, newest first, one row per track.
pub fn recent(connection: &Connection, limit: usize) -> Result<Vec<Track>> {
    let sql = format!(
        "SELECT {TRACK_COLUMNS}, MAX(h.played_at) AS heard_at
         {TRACK_JOINS}
         JOIN play_history h ON h.track_id = t.id
         GROUP BY t.id
         ORDER BY heard_at DESC
         LIMIT ?1"
    );

    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(params![limit as i64], track_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Plays per day over the last `days` days, oldest bucket first.
///
/// `now` is passed in rather than read from the clock so the bucketing is
/// testable, and so every panel on one page agrees about where "today" ends.
pub fn activity(connection: &Connection, days: usize, now: i64) -> Result<Vec<u32>> {
    let mut buckets = vec![0u32; days];
    if days == 0 {
        return Ok(buckets);
    }

    let oldest = now / DAY - (days as i64 - 1);

    let mut statement = connection.prepare(
        "SELECT played_at / 86400, COUNT(*)
           FROM play_history
          WHERE played_at >= ?1
          GROUP BY 1",
    )?;

    let rows = statement.query_map(params![oldest * DAY], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
    })?;

    for row in rows {
        let (day, count) = row?;
        // A clock that went backwards can date a play after "today"; drop it
        // rather than letting it wrap into the wrong end of the chart.
        if let Ok(slot) = usize::try_from(day - oldest)
            && slot < days
        {
            buckets[slot] = count.clamp(0, i64::from(u32::MAX)) as u32;
        }
    }

    Ok(buckets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::db;
    use crate::library::playlist::{add_listening, record_play, trim_history};

    const NOW: i64 = 1_700_000_000;

    /// Five tracks across two artists and one album.
    fn fixture() -> Connection {
        let db = db::open_in_memory().unwrap();

        db.execute_batch(
            "INSERT INTO artists (id, name, sort_name) VALUES
                 (1, 'Alpha', 'alpha'), (2, 'Beta', 'beta');
             INSERT INTO albums (id, title, sort_title, artist_id) VALUES
                 (1, 'First', 'first', 1);",
        )
        .unwrap();

        for id in 1..=5i64 {
            db.execute(
                "INSERT INTO tracks (
                     id, path, folder, file_name, mtime, size,
                     title, sort_title, artist_id, album_id,
                     duration_ms, play_count, added_at, last_seen_at
                 ) VALUES (?1, ?2, '/m', ?3, 1, 2, ?4, ?5, ?6, 1, ?7, 0, ?8, 0)",
                params![
                    id,
                    format!("/m/{id}.mp3"),
                    format!("{id}.mp3"),
                    format!("Track {id}"),
                    format!("track {id}"),
                    if id <= 3 { 1 } else { 2 },
                    60_000 * id,
                    NOW,
                ],
            )
            .unwrap();
        }

        db
    }

    #[test]
    fn totals_describe_an_untouched_library() {
        let db = fixture();
        let totals = totals(&db).unwrap();

        assert_eq!(totals.tracks, 5);
        assert_eq!(totals.artists, 2);
        assert_eq!(totals.albums, 1);
        // 60 + 120 + 180 + 240 + 300 seconds.
        assert!((totals.library_secs - 900.0).abs() < f64::EPSILON);

        assert!(!totals.has_history(), "nothing has been played yet");
        assert_eq!(totals.plays, 0);
        assert_eq!(totals.tracks_played, 0);
        assert_eq!(totals.first_play, None);
        assert!(totals.explored().abs() < f32::EPSILON);
    }

    #[test]
    fn listening_time_is_the_whole_track_not_the_threshold() {
        // The bug this exists to prevent: two tracks played in full, four
        // minutes and three, reported as three minutes total because each was
        // credited only up to the point where it started counting as a play.
        let db = fixture();

        record_play(&db, 1, NOW).unwrap();
        add_listening(&db, 1, 240.0).unwrap();

        record_play(&db, 2, NOW + 300).unwrap();
        add_listening(&db, 2, 180.0).unwrap();

        let totals = totals(&db).unwrap();
        assert_eq!(totals.plays, 2);
        assert!(
            (totals.listened_secs - 420.0).abs() < 1e-6,
            "seven minutes listened must read as seven minutes, got {}",
            totals.listened_secs
        );
    }

    #[test]
    fn listening_accumulates_across_many_flushes() {
        let db = fixture();
        for _ in 0..100 {
            add_listening(&db, 1, 2.5).unwrap();
        }

        let totals = totals(&db).unwrap();
        assert!(
            (totals.listened_secs - 250.0).abs() < 1e-6,
            "got {}",
            totals.listened_secs
        );
    }

    #[test]
    fn time_on_a_skipped_track_still_counts_as_listening() {
        // Thirty seconds of a track you skipped is thirty seconds you spent
        // listening, even though it never became a play.
        let db = fixture();
        add_listening(&db, 3, 30.0).unwrap();

        let totals = totals(&db).unwrap();
        assert_eq!(totals.plays, 0, "the threshold was never reached");
        assert!((totals.listened_secs - 30.0).abs() < 1e-6);
        assert!(totals.has_history(), "time spent is history worth showing");
    }

    #[test]
    fn a_nonsense_duration_is_ignored_rather_than_poisoning_the_total() {
        let db = fixture();
        add_listening(&db, 1, 60.0).unwrap();

        for bad in [f64::NAN, f64::INFINITY, -30.0, 0.0] {
            add_listening(&db, 1, bad).unwrap();
        }

        let totals = totals(&db).unwrap();
        assert!((totals.listened_secs - 60.0).abs() < 1e-6);
    }

    #[test]
    fn plays_on_tracks_with_no_listening_are_reported_as_unmeasured() {
        // The shape a real database was left in by the old model: play
        // counters with no trustworthy listening time behind them.
        let db = fixture();
        record_play(&db, 1, NOW).unwrap();
        record_play(&db, 2, NOW).unwrap();
        add_listening(&db, 2, 120.0).unwrap();

        let totals = totals(&db).unwrap();
        assert_eq!(totals.plays, 2);
        assert!((totals.listened_secs - 120.0).abs() < 1e-6);

        assert_eq!(totals.unmeasured_plays, 1, "track 1 has no listening time");
        // Track 1 is 60 s long: the most that play could have contributed.
        assert!((totals.unmeasured_secs - 60.0).abs() < f64::EPSILON);
    }

    #[test]
    fn the_listening_total_survives_the_history_being_trimmed() {
        let db = fixture();
        for step in 0..10i64 {
            let id = (step % 5) + 1;
            record_play(&db, id, NOW + step * 60).unwrap();
            add_listening(&db, id, 20.0).unwrap();
        }

        let before = totals(&db).unwrap();
        assert!((before.listened_secs - 200.0).abs() < 1e-6);

        trim_history(&db, 2).unwrap();

        let after = totals(&db).unwrap();
        assert!(
            (after.listened_secs - 200.0).abs() < 1e-6,
            "trimming the history must not touch listening time"
        );
        assert_eq!(after.plays, 10, "the per-track counters are never trimmed");
    }

    #[test]
    fn top_tracks_rank_by_play_count() {
        let db = fixture();
        for _ in 0..5 {
            record_play(&db, 3, NOW).unwrap();
        }
        add_listening(&db, 3, 50.0).unwrap();
        record_play(&db, 1, NOW).unwrap();

        let top = top_tracks(&db, 10).unwrap();
        assert_eq!(top.len(), 2, "only tracks with something to show appear");
        assert_eq!(top[0].track.id, 3);
        assert_eq!(top[0].track.play_count, 5);
        assert!((top[0].secs - 50.0).abs() < 1e-6);
        assert_eq!(top[1].track.id, 1);
    }

    #[test]
    fn a_track_listened_to_but_never_played_still_ranks() {
        let db = fixture();
        add_listening(&db, 4, 45.0).unwrap();

        let top = top_tracks(&db, 10).unwrap();
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].track.id, 4);
        assert_eq!(top[0].track.play_count, 0);
        assert!((top[0].secs - 45.0).abs() < 1e-6);
    }

    #[test]
    fn top_artists_sum_the_plays_of_their_tracks() {
        let db = fixture();
        // Tracks 1..3 are Alpha, 4..5 are Beta.
        record_play(&db, 1, NOW).unwrap();
        record_play(&db, 2, NOW).unwrap();
        add_listening(&db, 1, 10.0).unwrap();
        add_listening(&db, 2, 10.0).unwrap();
        for _ in 0..4 {
            record_play(&db, 4, NOW).unwrap();
        }

        let top = top_artists(&db, 10).unwrap();
        assert_eq!(top[0].name, "Beta");
        assert_eq!(top[0].plays, 4);
        assert_eq!(top[1].name, "Alpha");
        assert_eq!(top[1].plays, 2);
        assert!((top[1].secs - 20.0).abs() < 1e-6);
    }

    #[test]
    fn recent_returns_each_track_once_newest_first() {
        let db = fixture();
        record_play(&db, 1, NOW).unwrap();
        record_play(&db, 2, NOW + 10).unwrap();
        record_play(&db, 1, NOW + 20).unwrap();

        let recent = recent(&db, 10).unwrap();
        let ids: Vec<_> = recent.iter().map(|t| t.id).collect();
        assert_eq!(ids, vec![1, 2], "track 1 is newest and appears once");
    }

    #[test]
    fn activity_buckets_plays_by_day() {
        let db = fixture();
        // Today, yesterday, and four days back.
        record_play(&db, 1, NOW).unwrap();
        record_play(&db, 2, NOW).unwrap();
        record_play(&db, 3, NOW - DAY).unwrap();
        record_play(&db, 4, NOW - 4 * DAY).unwrap();

        let week = activity(&db, 7, NOW).unwrap();
        assert_eq!(week.len(), 7);
        assert_eq!(week[6], 2, "today is the last bucket");
        assert_eq!(week[5], 1, "yesterday");
        assert_eq!(week[2], 1, "four days back");
        assert_eq!(week[0], 0);
        assert_eq!(week.iter().sum::<u32>(), 4);
    }

    #[test]
    fn activity_ignores_history_older_than_the_window() {
        let db = fixture();
        record_play(&db, 1, NOW - 30 * DAY).unwrap();
        record_play(&db, 2, NOW).unwrap();

        let week = activity(&db, 7, NOW).unwrap();
        assert_eq!(week.iter().sum::<u32>(), 1, "only the recent play counts");
    }
}
