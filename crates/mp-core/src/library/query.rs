//! Reading the index back out.
//!
//! Every function here returns owned display types rather than borrowed rows,
//! because the caller is a UI that will hold the result across frames while the
//! scanner keeps writing. Queries are shaped so the aggregate work (counts,
//! total durations, a representative cover) happens in SQLite once per refresh
//! rather than in the UI once per frame.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use rusqlite::{Connection, Row, ToSql, params, params_from_iter};

use crate::library::model::{
    Album, Artist, Filter, Folder, Genre, Order, Stats, Track, TrackId, UNKNOWN_ALBUM,
    UNKNOWN_ARTIST,
};

/// Columns every track query selects, in the order [`track_from_row`] expects.
pub(crate) const TRACK_COLUMNS: &str = "
    t.id, t.path, t.title,
    COALESCE(ar.name, '') AS artist_name,
    COALESCE(al.title, '') AS album_title,
    t.album_id, t.artist_id, t.track_no, t.disc_no, t.year, t.duration_ms,
    COALESCE(t.art_id, al.art_id) AS cover,
    t.tagged, t.play_count
";

pub(crate) const TRACK_JOINS: &str = "
    FROM tracks t
    LEFT JOIN artists ar ON ar.id = t.artist_id
    LEFT JOIN albums  al ON al.id = t.album_id
";

pub(crate) fn track_from_row(row: &Row<'_>) -> rusqlite::Result<Track> {
    let artist: String = row.get(3)?;
    let album: String = row.get(4)?;

    Ok(Track {
        id: row.get(0)?,
        path: PathBuf::from(row.get::<_, String>(1)?),
        title: row.get(2)?,
        artist: if artist.is_empty() {
            UNKNOWN_ARTIST.to_owned()
        } else {
            artist
        },
        album: if album.is_empty() {
            UNKNOWN_ALBUM.to_owned()
        } else {
            album
        },
        album_id: row.get(5)?,
        artist_id: row.get(6)?,
        track_no: row.get::<_, Option<i64>>(7)?.map(|n| n as u32),
        disc_no: row.get::<_, Option<i64>>(8)?.map(|n| n as u32),
        year: row.get::<_, Option<i64>>(9)?.map(|n| n as i32),
        duration: row
            .get::<_, Option<i64>>(10)?
            .map(|ms| Duration::from_millis(ms.max(0) as u64)),
        art_id: row.get(11)?,
        tagged: row.get::<_, i64>(12)? != 0,
        play_count: row.get::<_, i64>(13)?.max(0) as u32,
    })
}

/// Fetch the tracks matching `filter`, in the requested order.
pub fn tracks(
    connection: &Connection,
    filter: &Filter,
    order: Order,
    descending: bool,
) -> Result<Vec<Track>> {
    if let Filter::Search(text) = filter {
        return search(connection, text, Some(SEARCH_LIMIT));
    }

    let (predicate, bindings) = predicate_for(filter);
    let sql = format!(
        "SELECT {TRACK_COLUMNS} {TRACK_JOINS} {predicate} ORDER BY {}",
        order.sql(descending)
    );

    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(params_from_iter(bindings.iter()), track_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The `WHERE` clause and its bound values for a filter.
fn predicate_for(filter: &Filter) -> (String, Vec<Box<dyn ToSql>>) {
    match filter {
        Filter::All | Filter::Search(_) => (String::new(), Vec::new()),
        Filter::Artist(id) => (
            // Both the track's own artist and the album's credited artist count,
            // so opening an artist shows the compilations they appear on.
            "WHERE t.artist_id = ?1 OR al.artist_id = ?1".to_owned(),
            vec![Box::new(*id)],
        ),
        Filter::Album(id) => ("WHERE t.album_id = ?1".to_owned(), vec![Box::new(*id)]),
        Filter::Genre(id) => (
            "WHERE t.id IN (SELECT track_id FROM track_genres WHERE genre_id = ?1)".to_owned(),
            vec![Box::new(*id)],
        ),
        Filter::Folder(path) => (
            "WHERE t.folder = ?1".to_owned(),
            vec![Box::new(path.to_string_lossy().into_owned())],
        ),
    }
}

/// The most search results the interface will ever ask for.
///
/// A one-letter query against a large library matches nearly everything, and
/// without a cap the app materialised all of it — thirty thousand rows built
/// and thrown away on *every keystroke*. Nobody scrolls to the ten-thousandth
/// result of a search they are still typing; they add another letter. The
/// caller is told when the cap was hit so the interface can say so rather than
/// quietly pretending the library is smaller than it is.
pub const SEARCH_LIMIT: usize = 1_000;

/// Full-text search across title, artist, album and genre.
///
/// Results come back in relevance order rather than the list's usual sort:
/// when someone types a query, the best match belongs at the top.
pub fn search(connection: &Connection, text: &str, limit: Option<usize>) -> Result<Vec<Track>> {
    let Some(expression) = fts_expression(text) else {
        return Ok(Vec::new());
    };

    let sql = format!(
        "SELECT {TRACK_COLUMNS}
         FROM tracks_fts f
         JOIN tracks t ON t.id = f.rowid
         LEFT JOIN artists ar ON ar.id = t.artist_id
         LEFT JOIN albums  al ON al.id = t.album_id
         WHERE tracks_fts MATCH ?1
         ORDER BY f.rank, t.sort_title
         LIMIT ?2"
    );

    let limit = limit.unwrap_or(usize::MAX).min(50_000) as i64;
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(params![expression, limit], track_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Turn what the user typed into an FTS5 expression.
///
/// Every token is quoted and given a prefix wildcard, so typing `rad` finds
/// Vellichor while a stray quote or `NEAR` cannot change the query's meaning.
/// Returns `None` when nothing searchable is left.
pub fn fts_expression(text: &str) -> Option<String> {
    let tokens: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(|token| format!("\"{token}\"*"))
        .collect();

    (!tokens.is_empty()).then(|| tokens.join(" AND "))
}

/// Every artist that has at least one track.
pub fn artists(connection: &Connection) -> Result<Vec<Artist>> {
    let mut statement = connection.prepare(
        "SELECT ar.id, ar.name,
                COUNT(t.id),
                COUNT(DISTINCT t.album_id),
                (SELECT x.art_id FROM tracks x
                  WHERE x.artist_id = ar.id AND x.art_id IS NOT NULL LIMIT 1)
         FROM artists ar
         JOIN tracks t ON t.artist_id = ar.id
         GROUP BY ar.id
         ORDER BY ar.sort_name, ar.name",
    )?;

    let rows = statement.query_map([], |row| {
        Ok(Artist {
            id: row.get(0)?,
            name: row.get(1)?,
            track_count: row.get::<_, i64>(2)?.max(0) as u32,
            album_count: row.get::<_, i64>(3)?.max(0) as u32,
            art_id: row.get(4)?,
        })
    })?;

    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Albums with at least `min_tracks` tracks, optionally narrowed to one artist.
///
/// `min_tracks` exists because a downloaded collection is full of tags like
/// `album = "Music"`, which produce dozens of one-track "albums" that bury the
/// real ones. Filtering here rather than hiding them in the UI keeps the count
/// the view reports honest.
pub fn albums(connection: &Connection, artist: Option<i64>, min_tracks: u32) -> Result<Vec<Album>> {
    let mut sql = String::from(
        "SELECT al.id, al.title, COALESCE(ar.name, ''), al.artist_id, al.year,
                COUNT(t.id), COALESCE(SUM(t.duration_ms), 0),
                COALESCE(al.art_id,
                    (SELECT x.art_id FROM tracks x
                      WHERE x.album_id = al.id AND x.art_id IS NOT NULL LIMIT 1))
         FROM albums al
         LEFT JOIN artists ar ON ar.id = al.artist_id
         JOIN tracks t ON t.album_id = al.id",
    );

    let mut bindings: Vec<Box<dyn ToSql>> = Vec::new();
    if let Some(id) = artist {
        sql.push_str(" WHERE al.artist_id = ?1 OR t.artist_id = ?1");
        bindings.push(Box::new(id));
    }
    sql.push_str(" GROUP BY al.id");
    if min_tracks > 1 {
        sql.push_str(&format!(" HAVING COUNT(t.id) >= {min_tracks}"));
    }
    sql.push_str(" ORDER BY al.year IS NULL, al.year, al.sort_title");

    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(params_from_iter(bindings.iter()), |row| {
        let artist: String = row.get(2)?;
        Ok(Album {
            id: row.get(0)?,
            title: row.get(1)?,
            artist: if artist.is_empty() {
                UNKNOWN_ARTIST.to_owned()
            } else {
                artist
            },
            artist_id: row.get(3)?,
            year: row.get::<_, Option<i64>>(4)?.map(|y| y as i32),
            track_count: row.get::<_, i64>(5)?.max(0) as u32,
            total_duration: Duration::from_millis(row.get::<_, i64>(6)?.max(0) as u64),
            art_id: row.get(7)?,
        })
    })?;

    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn genres(connection: &Connection) -> Result<Vec<Genre>> {
    let mut statement = connection.prepare(
        "SELECT g.id, g.name, COUNT(tg.track_id),
                (SELECT t.art_id FROM track_genres x
                   JOIN tracks t ON t.id = x.track_id
                  WHERE x.genre_id = g.id AND t.art_id IS NOT NULL LIMIT 1)
         FROM genres g
         JOIN track_genres tg ON tg.genre_id = g.id
         GROUP BY g.id
         ORDER BY COUNT(tg.track_id) DESC, g.sort_name",
    )?;

    let rows = statement.query_map([], |row| {
        Ok(Genre {
            id: row.get(0)?,
            name: row.get(1)?,
            track_count: row.get::<_, i64>(2)?.max(0) as u32,
            art_id: row.get(3)?,
        })
    })?;

    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Directories holding tracks, deepest name first for display.
pub fn folders(connection: &Connection) -> Result<Vec<Folder>> {
    let mut statement = connection.prepare(
        "SELECT folder, COUNT(*), COALESCE(SUM(duration_ms), 0)
         FROM tracks
         GROUP BY folder
         ORDER BY folder",
    )?;

    let rows = statement.query_map([], |row| {
        let path = PathBuf::from(row.get::<_, String>(0)?);
        let name = path.file_name().map_or_else(
            || path.to_string_lossy().into_owned(),
            |n| n.to_string_lossy().into_owned(),
        );
        Ok(Folder {
            path,
            name,
            track_count: row.get::<_, i64>(1)?.max(0) as u32,
            total_duration: Duration::from_millis(row.get::<_, i64>(2)?.max(0) as u64),
        })
    })?;

    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Headline counts for the library views.
pub fn stats(connection: &Connection) -> Result<Stats> {
    let scalar = |sql: &str| -> Result<i64> {
        Ok(connection.query_row(sql, [], |row| row.get::<_, i64>(0))?)
    };

    Ok(Stats {
        tracks: scalar("SELECT COUNT(*) FROM tracks")?.max(0) as u32,
        artists: scalar("SELECT COUNT(*) FROM artists")?.max(0) as u32,
        albums: scalar("SELECT COUNT(*) FROM albums")?.max(0) as u32,
        genres: scalar("SELECT COUNT(*) FROM genres")?.max(0) as u32,
        folders: scalar("SELECT COUNT(DISTINCT folder) FROM tracks")?.max(0) as u32,
        unplayable: scalar("SELECT COUNT(*) FROM unplayable")?.max(0) as u32,
        untagged: scalar("SELECT COUNT(*) FROM tracks WHERE tagged = 0")?.max(0) as u32,
        total_duration: Duration::from_millis(
            scalar("SELECT COALESCE(SUM(duration_ms), 0) FROM tracks")?.max(0) as u64,
        ),
    })
}

/// Files found in the watched folders that this build cannot decode.
pub fn unplayable(connection: &Connection) -> Result<Vec<(PathBuf, String)>> {
    let mut statement = connection.prepare("SELECT path, reason FROM unplayable ORDER BY path")?;
    let rows = statement.query_map([], |row| {
        Ok((
            PathBuf::from(row.get::<_, String>(0)?),
            row.get::<_, String>(1)?,
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Directories that could not be read during the last scan.
pub fn unreadable(connection: &Connection) -> Result<Vec<PathBuf>> {
    let mut statement = connection.prepare("SELECT path FROM unreadable ORDER BY path")?;
    let rows = statement.query_map([], |row| Ok(PathBuf::from(row.get::<_, String>(0)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// One track by id.
pub fn track(connection: &Connection, id: TrackId) -> Result<Option<Track>> {
    let sql = format!("SELECT {TRACK_COLUMNS} {TRACK_JOINS} WHERE t.id = ?1");
    let mut statement = connection.prepare(&sql)?;
    let mut rows = statement.query_map(params![id], track_from_row)?;
    Ok(rows.next().transpose()?)
}

/// Note that a track was played, for recency and play-count ordering.
pub fn record_play(connection: &Connection, id: TrackId, at_unix: i64) -> Result<()> {
    connection.execute(
        "UPDATE tracks SET play_count = play_count + 1, last_played_at = ?2 WHERE id = ?1",
        params![id, at_unix],
    )?;
    Ok(())
}

/// Genre names per track, for the tracks given.
///
/// Fetched in one query rather than per row: the detail panel needs it for a
/// whole album at once.
pub fn genres_for(
    connection: &Connection,
    ids: &[TrackId],
) -> Result<HashMap<TrackId, Vec<String>>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }

    let placeholders = std::iter::repeat_n("?", ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT tg.track_id, g.name
         FROM track_genres tg JOIN genres g ON g.id = tg.genre_id
         WHERE tg.track_id IN ({placeholders})
         ORDER BY g.sort_name"
    );

    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(params_from_iter(ids.iter()), |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;

    let mut out: HashMap<TrackId, Vec<String>> = HashMap::new();
    for row in rows {
        let (id, name) = row?;
        out.entry(id).or_default().push(name);
    }
    Ok(out)
}

/// Paths for a whole filtered view, in order — what the queue needs when the
/// user presses play on a row.
pub fn paths(
    connection: &Connection,
    filter: &Filter,
    order: Order,
    descending: bool,
) -> Result<Vec<PathBuf>> {
    Ok(tracks(connection, filter, order, descending)?
        .into_iter()
        .map(|track| track.path)
        .collect())
}

/// Whether a path is in the index at all.
pub fn contains_path(connection: &Connection, path: &Path) -> Result<bool> {
    let count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM tracks WHERE path = ?1",
        params![path.to_string_lossy()],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A user's search text must never be able to change the query's shape.
    #[test]
    fn search_text_is_reduced_to_quoted_prefix_tokens() {
        assert_eq!(
            fts_expression("winter"),
            Some("\"winter\"* ".trim().to_owned())
        );
        assert_eq!(
            fts_expression("silver junction"),
            Some("\"silver\"* AND \"junction\"*".to_owned())
        );
    }

    #[test]
    fn search_operators_are_neutralised() {
        // `"` would end a string, `*` and `NEAR` are FTS operators. None of them
        // survive tokenising, so the query stays a plain prefix search.
        let expression = fts_expression("a\" OR b NEAR/2 c").unwrap();
        assert!(!expression.contains("OR "), "{expression}");
        assert!(expression.contains("\"NEAR\"*"), "{expression}");
    }

    #[test]
    fn empty_and_punctuation_only_searches_match_nothing() {
        assert_eq!(fts_expression(""), None);
        assert_eq!(fts_expression("   "), None);
        assert_eq!(fts_expression("!!!"), None);
    }
}
