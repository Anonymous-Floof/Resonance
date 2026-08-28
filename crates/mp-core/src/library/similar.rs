//! Finding more of what you already like, without the internet.
//!
//! This is the engine behind "add similar" in the playlist builder. It is
//! deliberately offline: it reasons from the tags in your files, from what you
//! have already put together in your own playlists, and from what the tracks
//! actually sound like. No lookups, no accounts, nothing leaves the machine.
//!
//! # The blend
//!
//! Four signals, because no one of them survives a real collection:
//!
//! * **Shared genres** work beautifully on a well-tagged library and not at all
//!   on one where half the files say `Music`.
//! * **Co-occurrence** — artists you have already placed in the same playlist —
//!   is the strongest signal there is, and is empty until you have made a few.
//! * **Era proximity** is weak but nearly always available.
//! * **Sound**, from [`Features`], is the only one that works on a completely
//!   untagged file, and is only available once the analysis pass has run.
//!
//! Any of them can be missing. The weights are renormalised over whichever
//! signals were actually available for a given pair, so a library with no
//! genres still ranks sensibly on the rest rather than scoring everything zero.
//!
//! # Explainability
//!
//! Every suggestion carries the [`Reason`]s it earned. A recommendation you
//! cannot interrogate is one you cannot correct, and "because it shares a genre
//! with the seed" is the difference between a tool and a slot machine.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use rusqlite::{Connection, params, params_from_iter};

use super::features::{self, Features};
use super::model::{ArtistId, Track, TrackId};
use super::playlist::PlaylistId;
use super::query;

/// How many candidates are scored. Beyond this the ranking does not change
/// meaningfully, and the cost does.
const CANDIDATE_LIMIT: usize = 2_000;

/// Most tracks any single artist may contribute to a suggestion list.
///
/// Without this the answer to "more like this" is reliably "the rest of this
/// album", which is true, useless, and not what was asked.
pub const DEFAULT_PER_ARTIST: usize = 2;

/// What the suggestions are being drawn towards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seed {
    Track(TrackId),
    Artist(ArtistId),
    Playlist(PlaylistId),
}

/// Why a track was suggested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    SharedGenre(String),
    SameArtist,
    /// This artist appears alongside the seed's in one of your playlists.
    PlayedTogether(String),
    SameEra(i32),
    SameFolder,
    SimilarSound,
    SimilarTempo,
}

impl Reason {
    /// The short label shown on the suggestion row.
    pub fn chip(&self) -> String {
        match self {
            Self::SharedGenre(genre) => format!("shared genre: {genre}"),
            Self::SameArtist => "same artist".to_owned(),
            Self::PlayedTogether(playlist) => format!("together in {playlist}"),
            Self::SameEra(decade) => format!("{decade}s"),
            Self::SameFolder => "same folder".to_owned(),
            Self::SimilarSound => "similar sound".to_owned(),
            Self::SimilarTempo => "similar tempo".to_owned(),
        }
    }
}

/// One ranked suggestion.
#[derive(Debug, Clone)]
pub struct Suggestion {
    pub track: Track,
    /// How much of the *available* evidence this track matched, `0.0..=1.0`.
    ///
    /// Not renormalised over whichever signals happened to exist — see
    /// [`score`] for why that turned out to be a mistake.
    pub score: f32,
    /// How much evidence there was to go on at all, `0.0..=1.0`.
    ///
    /// Low means the library has little to reason from: no genres, no
    /// playlists yet, no audio analysis. The suggestions are still ordered
    /// sensibly, but they are guesses, and the interface should say so rather
    /// than presenting them with the same confidence as a well-tagged result.
    pub confidence: f32,
    pub reasons: Vec<Reason>,
}

impl Suggestion {
    /// Whether this rests on so little evidence that it should be hedged.
    pub fn is_weak(&self) -> bool {
        self.confidence < 0.25
    }
}

/// What the seed looks like, gathered once.
#[derive(Debug, Default)]
struct Profile {
    tracks: HashSet<TrackId>,
    artists: HashSet<ArtistId>,
    genres: HashSet<String>,
    folders: HashSet<String>,
    /// How many tracks each of the seed's folders holds.
    ///
    /// A folder is only evidence when it is small. Five tracks in a folder is
    /// an album; several hundred is a catch-all downloads folder, and treating
    /// the two alike made every track in a flat collection a perfect match for
    /// every other.
    folder_sizes: HashMap<String, u32>,
    decades: HashSet<i32>,
    /// The average of the seed's analysed tracks, when any were analysed.
    sound: Option<Features>,
    /// Artist ids that share a playlist with the seed's artists, and the
    /// playlist that connects them.
    together: HashMap<ArtistId, String>,
}

/// Rank tracks by how well they suit `seed`.
///
/// `limit` caps the result; `per_artist` caps how many any one artist may
/// contribute. Tracks already in the seed are never suggested back.
pub fn suggest(
    connection: &Connection,
    seed: Seed,
    limit: usize,
    per_artist: usize,
) -> Result<Vec<Suggestion>> {
    let profile = build_profile(connection, seed)?;

    if profile.tracks.is_empty() {
        return Ok(Vec::new());
    }

    let candidates = gather_candidates(connection, &profile)?;
    if candidates.is_empty() {
        return Ok(Vec::new());
    }

    let ids: Vec<TrackId> = candidates.iter().map(|track| track.id).collect();
    let genres = query::genres_for(connection, &ids)?;
    let sounds = features::get_many(connection, &ids)?;

    let mut scored: Vec<Suggestion> = candidates
        .into_iter()
        .filter(|track| !profile.tracks.contains(&track.id))
        .map(|track| {
            let (score, confidence, reasons) = score(&profile, &track, &genres, &sounds);
            Suggestion {
                track,
                score,
                confidence,
                reasons,
            }
        })
        // A suggestion with nothing to say for itself is not a suggestion, it
        // is a random track. On a real collection this mattered: hundreds of
        // candidates arrived through a shared 264-track folder, scored a
        // rounding error above zero, and would have been listed with an empty
        // row of reason chips. Requiring an explanation is what keeps the
        // feature honest — an empty result is a fair answer to "what else is
        // like this" when the answer is genuinely "nothing we can tell".
        .filter(|suggestion| suggestion.score > 0.0 && !suggestion.reasons.is_empty())
        .collect();

    // Highest first. Ties break on play count before title: on a collection
    // with little metadata a great many candidates score alike, and falling
    // back to alphabetical order means "more like this" hands back the front
    // of the library. What the user actually listens to is a far better guess
    // than what happens to start with the letter A.
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.track.play_count.cmp(&a.track.play_count))
            .then_with(|| a.track.title.cmp(&b.track.title))
            .then_with(|| a.track.id.cmp(&b.track.id))
    });

    Ok(apply_diversity(scored, limit, per_artist))
}

/// Keep the best suggestions without letting one artist take over.
fn apply_diversity(scored: Vec<Suggestion>, limit: usize, per_artist: usize) -> Vec<Suggestion> {
    let per_artist = per_artist.max(1);
    let mut used: HashMap<ArtistId, usize> = HashMap::new();
    let mut out = Vec::with_capacity(limit.min(scored.len()));

    for suggestion in scored {
        if out.len() >= limit {
            break;
        }

        // Tracks with no artist at all are not "the same artist" as each
        // other; capping them together would drop most of an untagged library.
        if let Some(artist) = suggestion.track.artist_id {
            let count = used.entry(artist).or_insert(0);
            if *count >= per_artist {
                continue;
            }
            *count += 1;
        }

        out.push(suggestion);
    }

    out
}

// ---------------------------------------------------------------------------
// The profile
// ---------------------------------------------------------------------------

fn build_profile(connection: &Connection, seed: Seed) -> Result<Profile> {
    let seed_tracks = seed_tracks(connection, seed)?;

    let mut profile = Profile::default();

    for track in &seed_tracks {
        profile.tracks.insert(track.id);

        if let Some(artist) = track.artist_id {
            profile.artists.insert(artist);
        }
        if let Some(year) = track.year {
            profile.decades.insert(decade(year));
        }
        if let Some(folder) = track.path.parent() {
            profile
                .folders
                .insert(folder.to_string_lossy().into_owned());
        }
    }

    let ids: Vec<TrackId> = seed_tracks.iter().map(|track| track.id).collect();

    for names in query::genres_for(connection, &ids)?.into_values() {
        for name in names {
            profile.genres.insert(name.to_lowercase());
        }
    }

    // The seed's sound is the average of whichever of its tracks have been
    // analysed. Averaging a whole album is the right thing for an album seed
    // and harmless for a single-track one.
    let sounds = features::get_many(connection, &ids)?;
    profile.sound = average(sounds.values());

    profile.together = co_occurring_artists(connection, &profile.artists)?;
    profile.folder_sizes = folder_sizes(connection, &profile.folders)?;

    Ok(profile)
}

/// How many tracks live in each of `folders`.
fn folder_sizes(
    connection: &Connection,
    folders: &HashSet<String>,
) -> Result<HashMap<String, u32>> {
    if folders.is_empty() {
        return Ok(HashMap::new());
    }

    let placeholders = std::iter::repeat_n("?", folders.len())
        .collect::<Vec<_>>()
        .join(",");

    let sql = format!(
        "SELECT folder, count(*) FROM tracks WHERE folder IN ({placeholders}) GROUP BY folder"
    );

    let values: Vec<&String> = folders.iter().collect();

    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(params_from_iter(values.iter()), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?.max(0) as u32,
        ))
    })?;

    let mut out = HashMap::new();
    for row in rows {
        let (folder, count) = row?;
        out.insert(folder, count);
    }

    Ok(out)
}

fn seed_tracks(connection: &Connection, seed: Seed) -> Result<Vec<Track>> {
    let (predicate, binding): (&str, i64) = match seed {
        Seed::Track(id) => ("WHERE t.id = ?1", id),
        Seed::Artist(id) => ("WHERE t.artist_id = ?1 OR al.artist_id = ?1", id),
        Seed::Playlist(id) => (
            "JOIN playlist_items pi ON pi.track_id = t.id WHERE pi.playlist_id = ?1",
            id,
        ),
    };

    let sql = format!(
        "SELECT {} {} {predicate} LIMIT 500",
        query::TRACK_COLUMNS,
        query::TRACK_JOINS
    );

    let mut statement = connection.prepare(&sql)?;
    let rows = statement
        .query_map(params![binding], query::track_from_row)?
        .collect::<rusqlite::Result<_>>()?;

    Ok(rows)
}

/// Artists that share a playlist with any of `artists`.
///
/// This is the signal that gets better the more you use the app, and it is the
/// only one that reflects your taste rather than someone else's metadata.
fn co_occurring_artists(
    connection: &Connection,
    artists: &HashSet<ArtistId>,
) -> Result<HashMap<ArtistId, String>> {
    if artists.is_empty() {
        return Ok(HashMap::new());
    }

    let placeholders = std::iter::repeat_n("?", artists.len())
        .collect::<Vec<_>>()
        .join(",");

    let ids: Vec<ArtistId> = artists.iter().copied().collect();

    let sql = format!(
        "SELECT DISTINCT other.artist_id, p.name
           FROM playlist_items mine
           JOIN tracks seed  ON seed.id = mine.track_id
           JOIN playlist_items other_item ON other_item.playlist_id = mine.playlist_id
           JOIN tracks other ON other.id = other_item.track_id
           JOIN playlists p  ON p.id = mine.playlist_id
          WHERE seed.artist_id IN ({placeholders})
            AND other.artist_id IS NOT NULL
            AND other.artist_id NOT IN ({placeholders})"
    );

    // The id list is bound twice, once for each `IN`.
    let doubled: Vec<ArtistId> = ids.iter().chain(ids.iter()).copied().collect();

    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(params_from_iter(doubled.iter()), |row| {
        Ok((row.get::<_, ArtistId>(0)?, row.get::<_, String>(1)?))
    })?;

    let mut out = HashMap::new();
    for row in rows {
        let (artist, playlist) = row?;
        out.entry(artist).or_insert(playlist);
    }

    Ok(out)
}

fn average<'a>(values: impl Iterator<Item = &'a Features>) -> Option<Features> {
    let mut total = [0.0_f32; features::DIMENSIONS];
    let mut count = 0_f32;

    for features in values {
        for (slot, value) in total.iter_mut().zip(features.vector().iter()) {
            *slot += value;
        }
        count += 1.0;
    }

    if count == 0.0 {
        return None;
    }

    for value in &mut total {
        *value /= count;
    }

    Some(
        Features {
            tempo: total[0],
            centroid: total[1],
            rolloff: total[2],
            loudness: total[3],
            bass: total[4],
            mid: total[5],
            treble: total[6],
            zero_cross: total[7],
        }
        .sanitised(),
    )
}

fn decade(year: i32) -> i32 {
    year - year.rem_euclid(10)
}

// ---------------------------------------------------------------------------
// Candidates
// ---------------------------------------------------------------------------

/// Tracks worth scoring.
///
/// A union of everything that has *any* connection to the seed, rather than the
/// whole library: scoring 30k tracks to return twenty is work spent on rows
/// that were never going to place.
fn gather_candidates(connection: &Connection, profile: &Profile) -> Result<Vec<Track>> {
    let mut clauses: Vec<String> = Vec::new();
    let mut bindings: Vec<rusqlite::types::Value> = Vec::new();

    if !profile.genres.is_empty() {
        let placeholders = std::iter::repeat_n("?", profile.genres.len())
            .collect::<Vec<_>>()
            .join(",");
        clauses.push(format!(
            "EXISTS (SELECT 1 FROM track_genres tg JOIN genres g ON g.id = tg.genre_id
                      WHERE tg.track_id = t.id AND lower(g.name) IN ({placeholders}))"
        ));
        for genre in &profile.genres {
            bindings.push(rusqlite::types::Value::Text(genre.clone()));
        }
    }

    let mut artist_pool: HashSet<ArtistId> = profile.artists.clone();
    artist_pool.extend(profile.together.keys().copied());

    if !artist_pool.is_empty() {
        let placeholders = std::iter::repeat_n("?", artist_pool.len())
            .collect::<Vec<_>>()
            .join(",");
        clauses.push(format!("t.artist_id IN ({placeholders})"));
        for artist in &artist_pool {
            bindings.push(rusqlite::types::Value::Integer(*artist));
        }
    }

    if !profile.folders.is_empty() {
        let placeholders = std::iter::repeat_n("?", profile.folders.len())
            .collect::<Vec<_>>()
            .join(",");
        clauses.push(format!("t.folder IN ({placeholders})"));
        for folder in &profile.folders {
            bindings.push(rusqlite::types::Value::Text(folder.clone()));
        }
    }

    // Nothing connects to the seed at all — an untagged single in a folder of
    // its own, before any analysis has run. Falling back to the same era keeps
    // the feature from simply returning nothing, and the reasons make it plain
    // that the connection is thin.
    if clauses.is_empty() {
        if profile.decades.is_empty() {
            return Ok(Vec::new());
        }

        let placeholders = std::iter::repeat_n("?", profile.decades.len())
            .collect::<Vec<_>>()
            .join(",");
        clauses.push(format!(
            "(t.year IS NOT NULL AND (t.year - (t.year % 10)) IN ({placeholders}))"
        ));
        for decade in &profile.decades {
            bindings.push(rusqlite::types::Value::Integer(i64::from(*decade)));
        }
    }

    let sql = format!(
        "SELECT {} {} WHERE {} LIMIT {CANDIDATE_LIMIT}",
        query::TRACK_COLUMNS,
        query::TRACK_JOINS,
        clauses.join(" OR "),
    );

    let mut statement = connection.prepare(&sql)?;
    let rows = statement
        .query_map(params_from_iter(bindings.iter()), query::track_from_row)?
        .collect::<rusqlite::Result<_>>()?;

    Ok(rows)
}

// ---------------------------------------------------------------------------
// Scoring
// ---------------------------------------------------------------------------

/// Relative importance of each signal. These sum to one.
const WEIGHT_GENRE: f32 = 0.34;
const WEIGHT_TOGETHER: f32 = 0.26;
const WEIGHT_SOUND: f32 = 0.24;
const WEIGHT_ERA: f32 = 0.09;
const WEIGHT_FOLDER: f32 = 0.07;

const TOTAL_WEIGHT: f32 =
    WEIGHT_GENRE + WEIGHT_TOGETHER + WEIGHT_SOUND + WEIGHT_ERA + WEIGHT_FOLDER;

/// A folder this size or smaller counts fully as evidence.
///
/// Above it the signal fades away, because a large folder is a dumping ground
/// rather than a grouping. Around album length: a folder of a dozen tracks
/// really does say its contents belong together.
const DISCRIMINATING_FOLDER: f32 = 12.0;

/// Same-artist tracks are worth suggesting but should not crowd out the point
/// of the exercise, so this is added on top rather than being a weighted term.
const SAME_ARTIST_BONUS: f32 = 0.10;

/// Rank one candidate. Returns its score, how much evidence existed, and why.
///
/// The score is the fraction of *all* possible evidence that matched — it is
/// deliberately **not** renormalised over whichever signals happened to be
/// available. An earlier version did renormalise, on the reasoning that a
/// library with no genre tags should still rank on the rest. It does rank, but
/// the arithmetic was a trap: run against a real collection with no genres, no
/// playlists and every file in one folder, the only surviving signal was
/// "same folder" — so every track matched all of the available evidence and
/// scored a perfect 1.00. Hundreds of identical scores, sorted alphabetically,
/// presented with total confidence.
///
/// Not renormalising means such a library scores everything near zero, which
/// is the honest answer, while the ordering between candidates is preserved.
/// [`Suggestion::confidence`] reports the thinness separately so the interface
/// can say what is going on instead of implying certainty.
fn score(
    profile: &Profile,
    track: &Track,
    genres: &HashMap<TrackId, Vec<String>>,
    sounds: &HashMap<TrackId, Features>,
) -> (f32, f32, Vec<Reason>) {
    let mut reasons = Vec::new();

    let mut weighted = 0.0;
    // How much evidence existed to be matched, whether or not it matched.
    let mut available = 0.0;

    // -- genres
    if !profile.genres.is_empty() {
        let theirs: HashSet<String> = genres
            .get(&track.id)
            .map(|names| names.iter().map(|name| name.to_lowercase()).collect())
            .unwrap_or_default();

        if !theirs.is_empty() {
            let shared: Vec<&String> = profile.genres.intersection(&theirs).collect();

            // Jaccard rather than a raw count: one shared genre out of one is a
            // stronger signal than one shared out of eight.
            let union = profile.genres.union(&theirs).count().max(1);
            let overlap = shared.len() as f32 / union as f32;

            weighted += WEIGHT_GENRE * overlap;
            available += WEIGHT_GENRE;

            if let Some(name) = shared.first() {
                reasons.push(Reason::SharedGenre((*name).clone()));
            }
        }
    }

    // -- co-occurrence
    if !profile.together.is_empty() {
        available += WEIGHT_TOGETHER;

        if let Some(artist) = track.artist_id
            && let Some(playlist) = profile.together.get(&artist)
        {
            weighted += WEIGHT_TOGETHER;
            reasons.push(Reason::PlayedTogether(playlist.clone()));
        }
    }

    // -- sound
    if let (Some(seed_sound), Some(theirs)) = (profile.sound.as_ref(), sounds.get(&track.id)) {
        let closeness = seed_sound.similarity(theirs);

        weighted += WEIGHT_SOUND * closeness;
        available += WEIGHT_SOUND;

        // Only claimed when it is actually close. A chip on every row would
        // stop being information.
        if closeness > 0.75 {
            reasons.push(Reason::SimilarSound);
        }
        if seed_sound.similar_tempo(theirs) {
            reasons.push(Reason::SimilarTempo);
        }
    }

    // -- era
    if !profile.decades.is_empty() {
        available += WEIGHT_ERA;

        if let Some(year) = track.year {
            let theirs = decade(year);
            if profile.decades.contains(&theirs) {
                weighted += WEIGHT_ERA;
                reasons.push(Reason::SameEra(theirs));
            }
        }
    }

    // -- folder
    if !profile.folders.is_empty()
        && let Some(folder) = track.path.parent()
    {
        let name = folder.to_string_lossy();

        if profile.folders.contains(name.as_ref()) {
            // Faded out by how large the folder is. A shared folder of a
            // dozen tracks is an album; a shared folder of hundreds is
            // only evidence that both files are on the same disk.
            let size = profile
                .folder_sizes
                .get(name.as_ref())
                .copied()
                .unwrap_or(1)
                .max(1) as f32;
            let discrimination = (DISCRIMINATING_FOLDER / size).min(1.0);

            available += WEIGHT_FOLDER * discrimination;
            weighted += WEIGHT_FOLDER * discrimination;

            // Only claimed as a reason when the folder means something.
            if discrimination > 0.4 {
                reasons.push(Reason::SameFolder);
            }
        } else {
            available += WEIGHT_FOLDER;
        }
    }

    if available <= 0.0 {
        return (0.0, 0.0, reasons);
    }

    let mut total = weighted / TOTAL_WEIGHT;

    if let Some(artist) = track.artist_id
        && profile.artists.contains(&artist)
    {
        total += SAME_ARTIST_BONUS;
        reasons.push(Reason::SameArtist);
    }

    let confidence = (available / TOTAL_WEIGHT).clamp(0.0, 1.0);

    (total.clamp(0.0, 1.0), confidence, reasons)
}

// ---------------------------------------------------------------------------
// Auto-radio
// ---------------------------------------------------------------------------

/// How much recent listening auto-radio avoids repeating.
const RADIO_MEMORY: usize = 60;

/// Continue listening from where the queue ran out.
///
/// The same ranking as [`suggest`], with one addition that matters more than it
/// sounds: anything played recently is excluded. Without that, radio settles
/// into a loop of the same dozen tracks within an hour, because the tracks most
/// similar to what just played are the ones that just played.
///
/// `already_queued` is whatever the caller is about to play but has not played
/// yet, so two consecutive top-ups do not both pick the same track.
pub fn radio(
    connection: &Connection,
    seed: Seed,
    already_queued: &[TrackId],
    count: usize,
) -> Result<Vec<Track>> {
    let recent = super::playlist::recently_played(connection, RADIO_MEMORY)?;

    let mut avoid: HashSet<TrackId> = recent.into_iter().collect();
    avoid.extend(already_queued.iter().copied());

    // Asked for generously, because the exclusions below thin the list and a
    // second query would cost more than a longer first one.
    let pool = suggest(
        connection,
        seed,
        (count + avoid.len()).min(500),
        DEFAULT_PER_ARTIST,
    )?;

    let mut out: Vec<Track> = pool
        .into_iter()
        .filter(|suggestion| !avoid.contains(&suggestion.track.id))
        .map(|suggestion| suggestion.track)
        .take(count)
        .collect();

    // Everything similar has been heard recently. Rather than stopping — which
    // reads as the app giving up — fall back to the same ranking without the
    // recency filter. Repeating a track is a smaller failure than silence.
    if out.is_empty() {
        out = suggest(connection, seed, count, DEFAULT_PER_ARTIST)?
            .into_iter()
            .filter(|suggestion| !already_queued.contains(&suggestion.track.id))
            .map(|suggestion| suggestion.track)
            .take(count)
            .collect();
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::db;
    use crate::library::playlist;

    /// Two artists, two genres, a folder each.
    fn fixture() -> Connection {
        let db = db::open_in_memory().unwrap();

        db.execute_batch(
            "INSERT INTO artists (id, name, sort_name) VALUES
                 (1, 'Seed Artist', 'seed artist'),
                 (2, 'Neighbour',   'neighbour'),
                 (3, 'Stranger',    'stranger');
             INSERT INTO genres (id, name, sort_name) VALUES
                 (1, 'Shoegaze', 'shoegaze'), (2, 'Polka', 'polka');",
        )
        .unwrap();

        // 1 seed; 2,3 same genre different artist; 4 same artist; 5 unrelated.
        let rows = [
            (1, 1, "/a", 1993, Some(1)),
            (2, 2, "/b", 1995, Some(1)),
            (3, 2, "/b", 1994, Some(1)),
            (4, 1, "/a", 1993, Some(1)),
            (5, 3, "/c", 2020, Some(2)),
        ];

        for (id, artist, folder, year, genre) in rows {
            db.execute(
                "INSERT INTO tracks (
                     id, path, folder, file_name, mtime, size, title, sort_title,
                     artist_id, year, duration_ms, added_at, last_seen_at
                 ) VALUES (?1, ?2, ?3, 'x.mp3', 1, 2, ?4, ?5, ?6, ?7, 200000, 0, 0)",
                params![
                    id,
                    format!("{folder}/{id}.mp3"),
                    folder,
                    format!("Track {id}"),
                    format!("track {id}"),
                    artist,
                    year,
                ],
            )
            .unwrap();

            if let Some(genre) = genre {
                db.execute(
                    "INSERT INTO track_genres (track_id, genre_id) VALUES (?1, ?2)",
                    params![id, genre],
                )
                .unwrap();
            }
        }

        db
    }

    fn ids(suggestions: &[Suggestion]) -> Vec<TrackId> {
        suggestions.iter().map(|s| s.track.id).collect()
    }

    #[test]
    fn the_seed_is_never_suggested_back() {
        let db = fixture();
        let out = suggest(&db, Seed::Track(1), 10, 10).unwrap();

        assert!(!ids(&out).contains(&1), "the seed suggested itself");
        assert!(!out.is_empty(), "nothing was suggested at all");
    }

    #[test]
    fn tracks_sharing_a_genre_rank_above_unrelated_ones() {
        let db = fixture();
        let out = suggest(&db, Seed::Track(1), 10, 10).unwrap();

        let ranked = ids(&out);
        let stranger = ranked.iter().position(|id| *id == 5);

        // Track 5 shares neither genre, artist, folder nor decade, so it should
        // not be a candidate at all.
        assert!(
            stranger.is_none(),
            "an unrelated track was suggested: {ranked:?}"
        );
    }

    /// Every suggestion has to be explainable.
    #[test]
    fn every_suggestion_carries_a_reason() {
        let db = fixture();

        for suggestion in suggest(&db, Seed::Track(1), 10, 10).unwrap() {
            assert!(
                !suggestion.reasons.is_empty(),
                "track {} was suggested with no reason",
                suggestion.track.id
            );

            for reason in &suggestion.reasons {
                assert!(!reason.chip().is_empty());
            }
        }
    }

    /// "More like this" should not mean "the rest of this album".
    #[test]
    fn one_artist_cannot_dominate_the_list() {
        let db = db::open_in_memory().unwrap();

        db.execute_batch(
            "INSERT INTO artists (id, name, sort_name) VALUES
                 (1, 'Prolific', 'prolific'), (2, 'Other', 'other');
             INSERT INTO genres (id, name, sort_name) VALUES (1, 'Shoegaze', 'shoegaze');",
        )
        .unwrap();

        // Ten by one artist, two by another, all sharing a genre.
        for id in 1..=12 {
            let artist = if id <= 10 { 1 } else { 2 };
            db.execute(
                "INSERT INTO tracks (
                     id, path, folder, file_name, mtime, size, title, sort_title,
                     artist_id, year, added_at, last_seen_at
                 ) VALUES (?1, ?2, '/m', 'x.mp3', 1, 2, ?3, ?3, ?4, 2000, 0, 0)",
                params![id, format!("/m/{id}.mp3"), format!("t{id:02}"), artist],
            )
            .unwrap();
            db.execute(
                "INSERT INTO track_genres (track_id, genre_id) VALUES (?1, 1)",
                params![id],
            )
            .unwrap();
        }

        let out = suggest(&db, Seed::Track(1), 10, DEFAULT_PER_ARTIST).unwrap();

        let from_prolific = out.iter().filter(|s| s.track.artist_id == Some(1)).count();

        assert!(
            from_prolific <= DEFAULT_PER_ARTIST,
            "one artist contributed {from_prolific} of {} suggestions",
            out.len()
        );
    }

    /// Untagged tracks all have `artist_id = NULL`; capping them as one artist
    /// would throw away most of a badly tagged library.
    #[test]
    fn tracks_with_no_artist_are_not_capped_together() {
        let db = db::open_in_memory().unwrap();

        db.execute_batch(
            "INSERT INTO genres (id, name, sort_name) VALUES (1, 'Shoegaze', 'shoegaze');",
        )
        .unwrap();

        for id in 1..=8 {
            db.execute(
                "INSERT INTO tracks (
                     id, path, folder, file_name, mtime, size, title, sort_title,
                     year, added_at, last_seen_at
                 ) VALUES (?1, ?2, '/m', 'x.mp3', 1, 2, ?3, ?3, 2000, 0, 0)",
                params![id, format!("/m/{id}.mp3"), format!("t{id}")],
            )
            .unwrap();
            db.execute(
                "INSERT INTO track_genres (track_id, genre_id) VALUES (?1, 1)",
                params![id],
            )
            .unwrap();
        }

        let out = suggest(&db, Seed::Track(1), 10, 2).unwrap();

        assert!(
            out.len() > 2,
            "untagged tracks were capped as a single artist: {} returned",
            out.len()
        );
    }

    /// The signal that reflects the user's own taste rather than metadata.
    #[test]
    fn artists_placed_in_the_same_playlist_are_drawn_together() {
        let mut db = fixture();

        // Put the seed artist and the stranger in one playlist. That should be
        // enough to pull the stranger's other work into the suggestions, which
        // nothing else in the fixture would do.
        let list = playlist::create(&db, "Mine", 0).unwrap();
        playlist::add_tracks(&mut db, list, &[1, 5], 0).unwrap();

        let out = suggest(&db, Seed::Track(1), 10, 10).unwrap();

        let stranger = out.iter().find(|s| s.track.artist_id == Some(3));
        assert!(
            stranger.is_some(),
            "co-occurrence did not connect the artists: {:?}",
            ids(&out)
        );

        assert!(
            stranger
                .unwrap()
                .reasons
                .iter()
                .any(|reason| matches!(reason, Reason::PlayedTogether(_))),
            "the connection was made but not explained"
        );
    }

    #[test]
    fn an_artist_seed_uses_the_whole_catalogue() {
        let db = fixture();
        let out = suggest(&db, Seed::Artist(1), 10, 10).unwrap();

        // Tracks 1 and 4 are the seed artist's; neither may come back.
        let ranked = ids(&out);
        assert!(!ranked.contains(&1) && !ranked.contains(&4));
        assert!(!ranked.is_empty());
    }

    #[test]
    fn a_playlist_seed_averages_everything_in_it() {
        let mut db = fixture();

        let list = playlist::create(&db, "Seedy", 0).unwrap();
        playlist::add_tracks(&mut db, list, &[1, 4], 0).unwrap();

        let out = suggest(&db, Seed::Playlist(list), 10, 10).unwrap();
        let ranked = ids(&out);

        assert!(!ranked.contains(&1) && !ranked.contains(&4));
        assert!(!ranked.is_empty());
    }

    /// The engine has to work before the analysis pass has run.
    #[test]
    fn suggestions_work_with_no_audio_analysis_at_all() {
        let db = fixture();
        let out = suggest(&db, Seed::Track(1), 10, 10).unwrap();

        assert!(!out.is_empty());
        for suggestion in &out {
            assert!(
                !suggestion
                    .reasons
                    .iter()
                    .any(|r| matches!(r, Reason::SimilarSound)),
                "a sound reason appeared with no analysis stored"
            );
        }
    }

    /// And it has to use the analysis once it exists.
    #[test]
    fn analysed_tracks_that_sound_alike_rank_higher() {
        let db = fixture();

        let bright = Features {
            tempo: 0.9,
            centroid: 0.9,
            rolloff: 0.9,
            loudness: 0.9,
            bass: 0.2,
            mid: 0.5,
            treble: 0.9,
            zero_cross: 0.8,
        };
        let dark = Features {
            tempo: 0.1,
            centroid: 0.1,
            rolloff: 0.1,
            loudness: 0.2,
            bass: 0.9,
            mid: 0.4,
            treble: 0.1,
            zero_cross: 0.1,
        };

        // Seed and track 2 sound alike; track 3 does not.
        features::store(&db, 1, &bright, 1, 2, 0).unwrap();
        features::store(&db, 2, &bright, 1, 2, 0).unwrap();
        features::store(&db, 3, &dark, 1, 2, 0).unwrap();

        let out = suggest(&db, Seed::Track(1), 10, 10).unwrap();

        let position = |id: TrackId| out.iter().position(|s| s.track.id == id);
        let (two, three) = (position(2), position(3));

        assert!(two.is_some() && three.is_some());
        assert!(
            two < three,
            "the similar-sounding track did not rank higher: {:?}",
            ids(&out)
        );

        let alike = out.iter().find(|s| s.track.id == 2).unwrap();
        assert!(
            alike
                .reasons
                .iter()
                .any(|r| matches!(r, Reason::SimilarSound)),
            "the sound match was not explained"
        );
    }

    /// The failure a real collection exposed.
    ///
    /// With no genres, no playlists, no analysis and every file in one folder,
    /// renormalising over "whichever signals exist" left a shared folder as
    /// the only evidence — so every track matched all of it and scored a
    /// perfect 1.00. Hundreds of identical scores, ordered alphabetically,
    /// presented with complete confidence.
    ///
    /// The honest answer for a collection like that is an empty list: nothing
    /// in it is connected to anything else in a way this can see.
    #[test]
    fn a_library_with_almost_no_metadata_admits_it_knows_nothing() {
        let db = db::open_in_memory().unwrap();

        for id in 1..=200 {
            db.execute(
                "INSERT INTO tracks (
                     id, path, folder, file_name, mtime, size, title, sort_title,
                     added_at, last_seen_at
                 ) VALUES (?1, ?2, '/music', 'x.mp3', 1, 2, ?3, ?3, 0, 0)",
                params![id, format!("/music/{id}.mp3"), format!("t{id:03}")],
            )
            .unwrap();
        }

        let out = suggest(&db, Seed::Track(1), 20, 20).unwrap();

        assert!(
            out.is_empty(),
            "a shared 200-track folder produced {} suggestions, the first scoring {:.2}",
            out.len(),
            out[0].score
        );
    }

    /// Same library, but with one thing worth knowing: shared artists. The
    /// suggestions should come back, ordered, and flagged as thin evidence
    /// rather than presented with confidence.
    #[test]
    fn thin_evidence_still_ranks_but_reports_low_confidence() {
        let db = db::open_in_memory().unwrap();

        db.execute_batch("INSERT INTO artists (id, name, sort_name) VALUES (1, 'A', 'a');")
            .unwrap();

        for id in 1..=50 {
            // Half share an artist with the seed; the rest have none.
            let artist = if id % 2 == 1 { Some(1) } else { None };

            db.execute(
                "INSERT INTO tracks (
                     id, path, folder, file_name, mtime, size, title, sort_title,
                     artist_id, added_at, last_seen_at
                 ) VALUES (?1, ?2, '/music', 'x.mp3', 1, 2, ?3, ?3, ?4, 0, 0)",
                params![id, format!("/music/{id}.mp3"), format!("t{id:03}"), artist],
            )
            .unwrap();
        }

        let out = suggest(&db, Seed::Track(1), 10, 10).unwrap();

        assert!(
            !out.is_empty(),
            "a shared artist was not enough to suggest anything"
        );

        for suggestion in &out {
            assert!(
                suggestion.score < 0.5,
                "a track scored {:.2} on a shared artist alone",
                suggestion.score
            );
            assert!(
                suggestion.is_weak(),
                "confidence {:.2} does not reflect how little there was to go on",
                suggestion.confidence
            );
            assert!(
                !suggestion.reasons.is_empty(),
                "track {} was suggested with nothing to say for it",
                suggestion.track.id
            );
        }
    }

    /// The other half of the same fix: a small folder really is evidence.
    #[test]
    fn a_small_folder_still_counts_as_evidence() {
        let db = db::open_in_memory().unwrap();

        // An album's worth in one folder, and a crowd in another.
        for id in 1..=6 {
            db.execute(
                "INSERT INTO tracks (
                     id, path, folder, file_name, mtime, size, title, sort_title,
                     added_at, last_seen_at
                 ) VALUES (?1, ?2, '/album', 'x.mp3', 1, 2, ?3, ?3, 0, 0)",
                params![id, format!("/album/{id}.mp3"), format!("a{id}")],
            )
            .unwrap();
        }

        let out = suggest(&db, Seed::Track(1), 10, 10).unwrap();

        assert!(!out.is_empty());
        assert!(
            out.iter()
                .any(|s| s.reasons.iter().any(|r| matches!(r, Reason::SameFolder))),
            "a six-track folder was not treated as a grouping"
        );
        assert!(
            out[0].score > 0.05,
            "a real album folder scored only {:.3}",
            out[0].score
        );
    }

    /// A well-tagged pair should be confident; a bare one should not.
    #[test]
    fn confidence_reflects_how_much_there_was_to_go_on() {
        let db = fixture();
        let rich = suggest(&db, Seed::Track(1), 5, 5).unwrap();

        assert!(!rich.is_empty());
        assert!(
            rich[0].confidence > 0.3,
            "a tagged library reported confidence {:.2}",
            rich[0].confidence
        );
    }

    #[test]
    fn scores_stay_within_range_and_descend() {
        let db = fixture();
        let out = suggest(&db, Seed::Track(1), 10, 10).unwrap();

        for suggestion in &out {
            assert!(
                (0.0..=1.0).contains(&suggestion.score),
                "score {} is out of range",
                suggestion.score
            );
        }

        for pair in out.windows(2) {
            assert!(
                pair[0].score >= pair[1].score,
                "results are not in descending order"
            );
        }
    }

    #[test]
    fn the_limit_is_respected() {
        let db = fixture();
        assert!(suggest(&db, Seed::Track(1), 2, 10).unwrap().len() <= 2);
        assert!(suggest(&db, Seed::Track(1), 0, 10).unwrap().is_empty());
    }

    #[test]
    fn a_seed_that_does_not_exist_returns_nothing() {
        let db = fixture();
        assert!(suggest(&db, Seed::Track(9_999), 10, 10).unwrap().is_empty());
        assert!(
            suggest(&db, Seed::Playlist(9_999), 10, 10)
                .unwrap()
                .is_empty()
        );
    }

    /// A lone untagged file in its own folder connects to nothing. Returning an
    /// empty list is correct; panicking or returning the whole library is not.
    #[test]
    fn a_track_connected_to_nothing_returns_an_empty_list() {
        let db = db::open_in_memory().unwrap();

        db.execute(
            "INSERT INTO tracks (
                 id, path, folder, file_name, mtime, size, title, sort_title,
                 added_at, last_seen_at
             ) VALUES (1, '/lonely/a.mp3', '/lonely', 'a.mp3', 1, 2, 'A', 'a', 0, 0)",
            [],
        )
        .unwrap();

        // Its own folder is in the profile, so it finds only itself — and the
        // seed is filtered out.
        assert!(suggest(&db, Seed::Track(1), 10, 10).unwrap().is_empty());
    }

    // -- auto-radio --------------------------------------------------------

    #[test]
    fn radio_continues_from_the_current_track() {
        let db = fixture();

        let next = radio(&db, Seed::Track(1), &[], 3).unwrap();

        assert!(!next.is_empty(), "radio produced nothing to play");
        assert!(
            !next.iter().any(|track| track.id == 1),
            "radio suggested the track that is already playing"
        );
    }

    /// The failure this exists to prevent: settling into a loop of the same
    /// handful of tracks.
    #[test]
    fn radio_avoids_what_was_just_played() {
        let db = fixture();

        // Track 2 is a strong match for the seed, so it would ordinarily lead.
        playlist::record_play(&db, 2, 1_000).unwrap();

        let next = radio(&db, Seed::Track(1), &[], 5).unwrap();

        assert!(
            !next.iter().any(|track| track.id == 2),
            "radio replayed a track from the recent history"
        );
    }

    /// Two top-ups in a row must not both choose the same track.
    #[test]
    fn radio_skips_what_is_already_queued() {
        let db = fixture();

        let first = radio(&db, Seed::Track(1), &[], 1).unwrap();
        assert_eq!(first.len(), 1);

        let queued: Vec<TrackId> = first.iter().map(|track| track.id).collect();
        let second = radio(&db, Seed::Track(1), &queued, 1).unwrap();

        assert_eq!(second.len(), 1);
        assert_ne!(
            second[0].id, first[0].id,
            "radio handed back a track that was already queued"
        );
    }

    /// When everything similar has been heard recently, repeating beats
    /// stopping — silence reads as the app having given up.
    #[test]
    fn radio_repeats_rather_than_falling_silent() {
        let db = fixture();

        // Every candidate played a moment ago.
        for id in 1..=5 {
            playlist::record_play(&db, id, 1_000).unwrap();
        }

        let next = radio(&db, Seed::Track(1), &[], 3).unwrap();

        assert!(
            !next.is_empty(),
            "radio stopped instead of falling back to a repeat"
        );
        assert!(!next.iter().any(|track| track.id == 1));
    }

    #[test]
    fn radio_respects_its_count() {
        let db = fixture();
        assert!(radio(&db, Seed::Track(1), &[], 2).unwrap().len() <= 2);
    }

    #[test]
    fn decades_are_computed_the_way_people_say_them() {
        assert_eq!(decade(1993), 1990);
        assert_eq!(decade(1990), 1990);
        assert_eq!(decade(2001), 2000);
        assert_eq!(decade(2020), 2020);
    }

    #[test]
    fn every_reason_has_a_readable_chip() {
        let reasons = [
            Reason::SharedGenre("shoegaze".into()),
            Reason::SameArtist,
            Reason::PlayedTogether("Evening".into()),
            Reason::SameEra(1990),
            Reason::SameFolder,
            Reason::SimilarSound,
            Reason::SimilarTempo,
        ];

        for reason in reasons {
            let chip = reason.chip();
            assert!(!chip.is_empty());
            assert!(chip.len() < 60, "chip is too long for a row: {chip}");
        }
    }
}
