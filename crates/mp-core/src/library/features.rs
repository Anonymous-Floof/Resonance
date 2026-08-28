//! What a track *sounds* like, reduced to eight numbers.
//!
//! Tags say who made a track and what it was filed under. They say nothing
//! about whether it is fast or slow, bright or dark, dense or sparse — and in a
//! collection where half the files are named by whoever uploaded them, tags
//! often say nothing at all. These features are the offline substitute: cheap
//! to compute once, cheap to compare, and derived from the audio itself so they
//! work on a completely untagged library.
//!
//! Computing them needs a decoder, which lives in `mp-audio`. This module owns
//! the shape, the storage and the comparison — everything that the similarity
//! engine needs and that can be tested without decoding anything.
//!
//! # Why a fixed struct rather than a blob
//!
//! The plan called for a blob. Columns won because the similarity query wants
//! to filter on a couple of these in SQL before it ranks, and a blob would mean
//! deserialising every track in the library to compare two of them.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

use super::model::TrackId;

/// Bumped when a feature changes meaning, so stored values are recomputed
/// rather than silently compared against ones from a different definition.
pub const ANALYSIS_VERSION: u32 = 1;

/// How many numbers make up the comparison vector.
pub const DIMENSIONS: usize = 8;

/// The measured character of one track.
///
/// Every field is normalised to `0.0..=1.0` so no single one dominates the
/// distance simply by being measured in larger units. The normalisation ranges
/// are chosen for recorded music rather than for the theoretical limits: a
/// tempo axis spanning 0–600 bpm would put every real track inside a tenth of
/// its range and make the axis useless.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Features {
    /// Estimated tempo, normalised over 60–180 bpm.
    pub tempo: f32,
    /// Spectral centroid — the "brightness" of the sound.
    pub centroid: f32,
    /// Spectral rolloff — where most of the energy sits below.
    pub rolloff: f32,
    /// Overall loudness.
    pub loudness: f32,
    /// Energy split, each a share of the total.
    pub bass: f32,
    pub mid: f32,
    pub treble: f32,
    /// Zero-crossing rate — high for noisy or percussive material.
    pub zero_cross: f32,
}

impl Features {
    /// The vector form, for distance calculations.
    pub fn vector(&self) -> [f32; DIMENSIONS] {
        [
            self.tempo,
            self.centroid,
            self.rolloff,
            self.loudness,
            self.bass,
            self.mid,
            self.treble,
            self.zero_cross,
        ]
    }

    /// How alike two tracks sound, `0.0` (unalike) to `1.0` (identical).
    ///
    /// Euclidean rather than cosine. Cosine distance ignores magnitude, which
    /// is exactly wrong here: two tracks with the same *shape* of spectrum at
    /// wildly different loudness and tempo are not similar listening, and
    /// cosine would call them identical.
    pub fn similarity(&self, other: &Self) -> f32 {
        let mine = self.vector();
        let theirs = other.vector();

        let mut sum = 0.0;
        for (a, b) in mine.iter().zip(theirs.iter()) {
            let delta = a - b;
            sum += delta * delta;
        }

        // The largest possible distance is the diagonal of the unit cube.
        let distance = sum.sqrt() / (DIMENSIONS as f32).sqrt();

        (1.0 - distance).clamp(0.0, 1.0)
    }

    /// Whether two tracks are close in tempo specifically, for a reason chip.
    pub fn similar_tempo(&self, other: &Self) -> bool {
        // 0.08 of the 60–180 range is about ten bpm.
        (self.tempo - other.tempo).abs() < 0.08
    }

    /// Clamp every axis into range.
    ///
    /// Called on the way in from the analyser and on the way out of the
    /// database, so a value from a buggy or older analysis cannot drag a
    /// distance calculation somewhere impossible.
    pub fn sanitised(mut self) -> Self {
        for value in [
            &mut self.tempo,
            &mut self.centroid,
            &mut self.rolloff,
            &mut self.loudness,
            &mut self.bass,
            &mut self.mid,
            &mut self.treble,
            &mut self.zero_cross,
        ] {
            *value = if value.is_finite() {
                value.clamp(0.0, 1.0)
            } else {
                0.0
            };
        }

        self
    }
}

impl Default for Features {
    /// The middle of every axis: no information, equidistant from everything.
    fn default() -> Self {
        Self {
            tempo: 0.5,
            centroid: 0.5,
            rolloff: 0.5,
            loudness: 0.5,
            bass: 0.5,
            mid: 0.5,
            treble: 0.5,
            zero_cross: 0.5,
        }
    }
}

/// Store one track's analysis.
///
/// `mtime` and `size` are the same fingerprint the scanner uses, so a file that
/// was re-encoded gets re-analysed rather than compared using numbers that
/// describe a different recording.
pub fn store(
    connection: &Connection,
    track: TrackId,
    features: &Features,
    mtime: i64,
    size: i64,
    now: i64,
) -> Result<()> {
    let features = features.sanitised();

    connection.execute(
        "INSERT INTO audio_features (
             track_id, mtime, size, version,
             tempo, centroid, rolloff, loudness, bass, mid, treble, zero_cross,
             analysed_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
         ON CONFLICT(track_id) DO UPDATE SET
             mtime = excluded.mtime,
             size = excluded.size,
             version = excluded.version,
             tempo = excluded.tempo,
             centroid = excluded.centroid,
             rolloff = excluded.rolloff,
             loudness = excluded.loudness,
             bass = excluded.bass,
             mid = excluded.mid,
             treble = excluded.treble,
             zero_cross = excluded.zero_cross,
             analysed_at = excluded.analysed_at",
        params![
            track,
            mtime,
            size,
            ANALYSIS_VERSION,
            features.tempo,
            features.centroid,
            features.rolloff,
            features.loudness,
            features.bass,
            features.mid,
            features.treble,
            features.zero_cross,
            now,
        ],
    )?;

    Ok(())
}

/// Read one track's analysis, if it is present and current.
pub fn get(connection: &Connection, track: TrackId) -> Result<Option<Features>> {
    let row = connection
        .query_row(
            "SELECT tempo, centroid, rolloff, loudness, bass, mid, treble, zero_cross
               FROM audio_features
              WHERE track_id = ?1 AND version = ?2",
            params![track, ANALYSIS_VERSION],
            |row| {
                Ok(Features {
                    tempo: row.get(0)?,
                    centroid: row.get(1)?,
                    rolloff: row.get(2)?,
                    loudness: row.get(3)?,
                    bass: row.get(4)?,
                    mid: row.get(5)?,
                    treble: row.get(6)?,
                    zero_cross: row.get(7)?,
                })
            },
        )
        .optional()?;

    Ok(row.map(Features::sanitised))
}

/// Read the analysis for several tracks at once.
pub fn get_many(
    connection: &Connection,
    ids: &[TrackId],
) -> Result<std::collections::HashMap<TrackId, Features>> {
    use std::collections::HashMap;

    if ids.is_empty() {
        return Ok(HashMap::new());
    }

    let placeholders = std::iter::repeat_n("?", ids.len())
        .collect::<Vec<_>>()
        .join(",");

    let sql = format!(
        "SELECT track_id, tempo, centroid, rolloff, loudness, bass, mid, treble, zero_cross
           FROM audio_features
          WHERE version = {ANALYSIS_VERSION} AND track_id IN ({placeholders})"
    );

    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(rusqlite::params_from_iter(ids.iter()), |row| {
        Ok((
            row.get::<_, TrackId>(0)?,
            Features {
                tempo: row.get(1)?,
                centroid: row.get(2)?,
                rolloff: row.get(3)?,
                loudness: row.get(4)?,
                bass: row.get(5)?,
                mid: row.get(6)?,
                treble: row.get(7)?,
                zero_cross: row.get(8)?,
            },
        ))
    })?;

    let mut out = HashMap::new();
    for row in rows {
        let (id, features) = row?;
        out.insert(id, features.sanitised());
    }

    Ok(out)
}

/// How much of the library has been analysed.
pub fn progress(connection: &Connection) -> Result<(u32, u32)> {
    let done: i64 = connection.query_row(
        "SELECT count(*) FROM audio_features f
           JOIN tracks t ON t.id = f.track_id
          WHERE f.version = ?1 AND f.mtime = t.mtime AND f.size = t.size",
        params![ANALYSIS_VERSION],
        |row| row.get(0),
    )?;

    let total: i64 = connection.query_row("SELECT count(*) FROM tracks", [], |row| row.get(0))?;

    Ok((done.max(0) as u32, total.max(0) as u32))
}

/// Tracks still needing analysis, oldest additions first.
///
/// Returns anything with no analysis, an analysis from a previous version, or
/// one taken before the file changed. Ordering by `added_at` means a freshly
/// imported album is analysed as a block rather than scattered through the
/// queue, so the feature is useful on it sooner.
pub fn pending(connection: &Connection, limit: usize) -> Result<Vec<(TrackId, String, i64, i64)>> {
    let mut statement = connection.prepare(
        "SELECT t.id, t.path, t.mtime, t.size
           FROM tracks t
           LEFT JOIN audio_features f ON f.track_id = t.id
          WHERE f.track_id IS NULL
             OR f.version != ?1
             OR f.mtime != t.mtime
             OR f.size != t.size
          ORDER BY t.added_at, t.id
          LIMIT ?2",
    )?;

    let rows = statement
        .query_map(params![ANALYSIS_VERSION, limit as i64], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;

    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::db;

    fn fixture() -> Connection {
        let db = db::open_in_memory().unwrap();

        for id in 1..=3 {
            db.execute(
                "INSERT INTO tracks (
                     id, path, folder, file_name, mtime, size, title, sort_title,
                     added_at, last_seen_at
                 ) VALUES (?1, ?2, '/m', 'x.mp3', 100, 200, 'T', 't', ?1, 0)",
                params![id, format!("/m/{id}.mp3")],
            )
            .unwrap();
        }

        db
    }

    fn loud_fast() -> Features {
        Features {
            tempo: 0.9,
            centroid: 0.8,
            rolloff: 0.85,
            loudness: 0.9,
            bass: 0.6,
            mid: 0.7,
            treble: 0.8,
            zero_cross: 0.7,
        }
    }

    fn quiet_slow() -> Features {
        Features {
            tempo: 0.1,
            centroid: 0.2,
            rolloff: 0.15,
            loudness: 0.1,
            bass: 0.3,
            mid: 0.2,
            treble: 0.1,
            zero_cross: 0.2,
        }
    }

    #[test]
    fn a_track_is_identical_to_itself() {
        assert_eq!(loud_fast().similarity(&loud_fast()), 1.0);
    }

    #[test]
    fn opposites_score_far_apart() {
        let close = loud_fast().similarity(&loud_fast());
        let far = loud_fast().similarity(&quiet_slow());

        assert!(far < 0.5, "opposite tracks scored {far:.3}");
        assert!(close > far);
    }

    /// Similarity has to stay inside its stated range for any input, because
    /// it is blended with weighted terms that assume it.
    #[test]
    fn similarity_stays_within_range() {
        let extremes = [
            Features {
                tempo: 0.0,
                centroid: 0.0,
                rolloff: 0.0,
                loudness: 0.0,
                bass: 0.0,
                mid: 0.0,
                treble: 0.0,
                zero_cross: 0.0,
            },
            Features {
                tempo: 1.0,
                centroid: 1.0,
                rolloff: 1.0,
                loudness: 1.0,
                bass: 1.0,
                mid: 1.0,
                treble: 1.0,
                zero_cross: 1.0,
            },
            Features::default(),
        ];

        for a in &extremes {
            for b in &extremes {
                let score = a.similarity(b);
                assert!(
                    (0.0..=1.0).contains(&score),
                    "similarity escaped its range: {score}"
                );
            }
        }
    }

    /// Corners of the unit cube are the furthest apart anything can be, and
    /// must score zero rather than a negative number.
    #[test]
    fn the_furthest_possible_pair_scores_zero() {
        let low = Features {
            tempo: 0.0,
            centroid: 0.0,
            rolloff: 0.0,
            loudness: 0.0,
            bass: 0.0,
            mid: 0.0,
            treble: 0.0,
            zero_cross: 0.0,
        };
        let high = Features {
            tempo: 1.0,
            centroid: 1.0,
            rolloff: 1.0,
            loudness: 1.0,
            bass: 1.0,
            mid: 1.0,
            treble: 1.0,
            zero_cross: 1.0,
        };

        assert_eq!(low.similarity(&high), 0.0);
    }

    /// A NaN from a buggy analysis must not poison every comparison.
    #[test]
    fn non_finite_values_are_scrubbed() {
        let broken = Features {
            tempo: f32::NAN,
            centroid: f32::INFINITY,
            rolloff: -5.0,
            loudness: 12.0,
            ..Features::default()
        }
        .sanitised();

        for value in broken.vector() {
            assert!(value.is_finite(), "a non-finite value survived");
            assert!((0.0..=1.0).contains(&value), "{value} is out of range");
        }

        assert!(broken.similarity(&Features::default()).is_finite());
    }

    #[test]
    fn features_survive_a_round_trip_through_the_database() {
        let db = fixture();
        let original = loud_fast();

        store(&db, 1, &original, 100, 200, 0).unwrap();
        let read = get(&db, 1).unwrap().unwrap();

        // Stored as f32 columns, so this is exact.
        assert_eq!(read, original);
    }

    #[test]
    fn storing_twice_replaces_rather_than_duplicating() {
        let db = fixture();

        store(&db, 1, &loud_fast(), 100, 200, 0).unwrap();
        store(&db, 1, &quiet_slow(), 100, 200, 1).unwrap();

        assert_eq!(get(&db, 1).unwrap().unwrap(), quiet_slow());

        let rows: i64 = db
            .query_row("SELECT count(*) FROM audio_features", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[test]
    fn everything_starts_out_pending() {
        let db = fixture();

        assert_eq!(pending(&db, 100).unwrap().len(), 3);
        assert_eq!(progress(&db).unwrap(), (0, 3));
    }

    #[test]
    fn analysed_tracks_leave_the_queue() {
        let db = fixture();
        store(&db, 1, &loud_fast(), 100, 200, 0).unwrap();

        let waiting: Vec<TrackId> = pending(&db, 100).unwrap().iter().map(|row| row.0).collect();
        assert_eq!(waiting, vec![2, 3]);
        assert_eq!(progress(&db).unwrap(), (1, 3));
    }

    /// A re-encoded file has to be analysed again — the old numbers describe a
    /// different recording.
    #[test]
    fn a_changed_file_returns_to_the_queue() {
        let db = fixture();
        store(&db, 1, &loud_fast(), 100, 200, 0).unwrap();
        assert_eq!(pending(&db, 100).unwrap().len(), 2);

        db.execute("UPDATE tracks SET mtime = 999 WHERE id = 1", [])
            .unwrap();

        let waiting: Vec<TrackId> = pending(&db, 100).unwrap().iter().map(|row| row.0).collect();
        assert!(waiting.contains(&1), "the changed file was not re-queued");
        assert_eq!(progress(&db).unwrap(), (0, 3));
    }

    /// A changed analyser invalidates its own past results.
    #[test]
    fn an_older_analysis_version_is_ignored() {
        let db = fixture();
        store(&db, 1, &loud_fast(), 100, 200, 0).unwrap();

        db.execute(
            "UPDATE audio_features SET version = 0 WHERE track_id = 1",
            [],
        )
        .unwrap();

        assert!(get(&db, 1).unwrap().is_none());
        assert_eq!(pending(&db, 100).unwrap().len(), 3);
    }

    #[test]
    fn several_tracks_can_be_read_at_once() {
        let db = fixture();
        store(&db, 1, &loud_fast(), 100, 200, 0).unwrap();
        store(&db, 3, &quiet_slow(), 100, 200, 0).unwrap();

        let many = get_many(&db, &[1, 2, 3]).unwrap();

        assert_eq!(many.len(), 2, "an unanalysed track produced a row");
        assert_eq!(many[&1], loud_fast());
        assert_eq!(many[&3], quiet_slow());
        assert!(!many.contains_key(&2));

        assert!(get_many(&db, &[]).unwrap().is_empty());
    }

    #[test]
    fn the_queue_respects_its_limit() {
        let db = fixture();
        assert_eq!(pending(&db, 2).unwrap().len(), 2);
    }
}
