//! Filling in what the scan could not find on disk.
//!
//! Scanning takes cover art from a track's own tags or from a `folder.jpg`
//! beside it. When a release has neither, the album simply has no cover, and
//! nothing offline can change that — the picture is not in the audio.
//!
//! This is the seam an enrichment pass writes through: [`albums_without_art`]
//! says which albums are still missing one, and [`attach_album_art`] records
//! the answer once something has been found. Both are ordinary index
//! operations with nothing network-shaped about them, which is deliberate —
//! the fetching lives in `mp-net` and this crate stays unaware of it.
//!
//! ## The art itself is stored the same way as any other
//!
//! A fetched cover goes through [`ArtCache::store`](super::art::ArtCache::store)
//! exactly as an embedded one does, so it is content-addressed, pre-resized to
//! the sizes the interface draws, and has its accent palette extracted. Twelve
//! tracks from one album share one set of files whether the picture came from
//! a tag or from a server.
//!
//! **Nothing here writes to the user's files.** The cover lands in the app's
//! own cache and the album row points at it; the audio files are not opened,
//! and no tag is added to them.

use anyhow::{Context, Result};
use rusqlite::Connection;

use super::model::AlbumId;

/// An album with no cover anywhere, and enough tagging to look one up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NeedsArt {
    pub id: AlbumId,
    pub title: String,
    pub artist: String,
}

/// Albums that have no cover, and could plausibly be identified.
///
/// Both a title and an artist are required, because an album named by only one
/// of them cannot be told apart from every other release sharing that name.
/// Filtering here rather than at the fetcher keeps the pass from spending a
/// request to be told what the index already knew.
///
/// An album counts as covered if *any* of its tracks carries art, matching how
/// the browse views resolve a cover — otherwise an album showing a picture
/// would be queued for fetching forever.
pub fn albums_without_art(connection: &Connection, limit: usize) -> Result<Vec<NeedsArt>> {
    let mut statement = connection
        .prepare(
            "SELECT al.id, al.title, COALESCE(ar.name, '')
               FROM albums al
               LEFT JOIN artists ar ON ar.id = al.artist_id
              WHERE al.art_id IS NULL
                AND NOT EXISTS (
                        SELECT 1 FROM tracks t
                         WHERE t.album_id = al.id AND t.art_id IS NOT NULL
                    )
                AND TRIM(al.title) <> ''
                AND TRIM(COALESCE(ar.name, '')) <> ''
              ORDER BY al.sort_title
              LIMIT ?1",
        )
        .context("preparing the missing-art query")?;

    let rows = statement
        .query_map([limit as i64], |row| {
            Ok(NeedsArt {
                id: row.get(0)?,
                title: row.get(1)?,
                artist: row.get(2)?,
            })
        })
        .context("listing albums without art")?;

    let mut albums = Vec::new();
    for album in rows {
        albums.push(album.context("reading an album without art")?);
    }

    Ok(albums)
}

/// Point an album, and its tracks, at a cover that has been found.
///
/// Tracks that already have their own art keep it. A track carrying an
/// embedded picture is showing the right one for that track — a single with
/// its own sleeve inside a compilation, say — and an album-level cover is a
/// worse answer for it than the one it already had.
///
/// Returns how many track rows were updated, so a caller can tell a real
/// attachment from a no-op.
pub fn attach_album_art(connection: &Connection, album: AlbumId, art_id: &str) -> Result<usize> {
    connection
        .execute(
            "UPDATE albums SET art_id = ?1 WHERE id = ?2",
            rusqlite::params![art_id, album],
        )
        .context("attaching art to an album")?;

    let tracks = connection
        .execute(
            "UPDATE tracks SET art_id = ?1 WHERE album_id = ?2 AND art_id IS NULL",
            rusqlite::params![art_id, album],
        )
        .context("attaching art to an album's tracks")?;

    Ok(tracks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::db;

    fn fixture() -> Connection {
        db::open_in_memory().expect("in-memory index")
    }

    /// Builds one album with `tracks` tracks. `art` is the album-level cover.
    fn album(connection: &Connection, artist: &str, title: &str, art: Option<&str>) -> AlbumId {
        connection
            .execute(
                "INSERT INTO artists(name, sort_name) VALUES (?1, ?1)
                 ON CONFLICT(name) DO NOTHING",
                [artist],
            )
            .expect("artist");

        let artist_id: i64 = connection
            .query_row("SELECT id FROM artists WHERE name = ?1", [artist], |row| {
                row.get(0)
            })
            .expect("artist id");

        connection
            .execute(
                "INSERT INTO albums(title, sort_title, artist_id, art_id)
                 VALUES (?1, ?1, ?2, ?3)",
                rusqlite::params![title, artist_id, art],
            )
            .expect("album");

        connection.last_insert_rowid()
    }

    fn track(connection: &Connection, album_id: AlbumId, path: &str, art: Option<&str>) {
        connection
            .execute(
                "INSERT INTO tracks(path, folder, file_name, mtime, size,
                                    title, sort_title, album_id, art_id,
                                    added_at, last_seen_at)
                 VALUES (?1, 'C:/music', ?1, 0, 0, ?1, ?1, ?2, ?3, 0, 0)",
                rusqlite::params![path, album_id, art],
            )
            .expect("track");
    }

    fn titles(connection: &Connection) -> Vec<String> {
        albums_without_art(connection, 50)
            .expect("query")
            .into_iter()
            .map(|album| album.title)
            .collect()
    }

    #[test]
    fn an_album_with_no_art_anywhere_is_listed() {
        let connection = fixture();
        let id = album(&connection, "Radiohead", "Kid A", None);
        track(&connection, id, "C:/kid-a/01.mp3", None);

        let found = albums_without_art(&connection, 50).expect("query");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].title, "Kid A");
        assert_eq!(found[0].artist, "Radiohead");
        assert_eq!(found[0].id, id);
    }

    #[test]
    fn an_album_that_already_has_a_cover_is_left_alone() {
        let connection = fixture();
        let id = album(&connection, "Radiohead", "Amnesiac", Some("cover-hash"));
        track(&connection, id, "C:/amnesiac/01.mp3", None);

        assert!(titles(&connection).is_empty());
    }

    /// The browse views show a cover if any track has one, so an album that
    /// looks covered must not be queued for fetching forever.
    #[test]
    fn art_on_a_single_track_counts_as_covered() {
        let connection = fixture();
        let id = album(&connection, "Radiohead", "In Rainbows", None);
        track(&connection, id, "C:/rainbows/01.mp3", Some("cover-hash"));
        track(&connection, id, "C:/rainbows/02.mp3", None);

        assert!(titles(&connection).is_empty());
    }

    /// Neither half can identify a release on its own.
    #[test]
    fn an_album_too_vague_to_look_up_is_not_listed() {
        let connection = fixture();

        let untitled = album(&connection, "Radiohead", "", None);
        track(&connection, untitled, "C:/a.mp3", None);

        connection
            .execute(
                "INSERT INTO albums(title, sort_title, artist_id) VALUES ('Orphan', 'Orphan', NULL)",
                [],
            )
            .expect("album with no artist");

        assert!(titles(&connection).is_empty());
    }

    #[test]
    fn the_limit_is_respected() {
        let connection = fixture();
        for n in 0..10 {
            let id = album(&connection, "Someone", &format!("Album {n}"), None);
            track(&connection, id, &format!("C:/{n}.mp3"), None);
        }

        assert_eq!(albums_without_art(&connection, 4).expect("query").len(), 4);
    }

    #[test]
    fn attaching_art_covers_the_album_and_its_tracks() {
        let connection = fixture();
        let id = album(&connection, "Radiohead", "Kid A", None);
        track(&connection, id, "C:/kid-a/01.mp3", None);
        track(&connection, id, "C:/kid-a/02.mp3", None);

        let updated = attach_album_art(&connection, id, "fetched-hash").expect("attach");
        assert_eq!(updated, 2);

        assert!(
            titles(&connection).is_empty(),
            "it is no longer missing art"
        );

        let album_art: Option<String> = connection
            .query_row("SELECT art_id FROM albums WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .expect("album art");
        assert_eq!(album_art.as_deref(), Some("fetched-hash"));
    }

    /// A track with its own sleeve keeps it. An album-level cover is a worse
    /// answer for that track than the one it already had.
    #[test]
    fn a_track_with_its_own_art_keeps_it() {
        let connection = fixture();
        let id = album(&connection, "Various", "A Compilation", None);
        track(&connection, id, "C:/comp/01.mp3", Some("its-own-sleeve"));
        track(&connection, id, "C:/comp/02.mp3", None);

        let updated = attach_album_art(&connection, id, "album-cover").expect("attach");
        assert_eq!(updated, 1, "only the track without art should change");

        let kept: String = connection
            .query_row(
                "SELECT art_id FROM tracks WHERE path = 'C:/comp/01.mp3'",
                [],
                |row| row.get(0),
            )
            .expect("art");
        assert_eq!(kept, "its-own-sleeve");
    }

    #[test]
    fn attaching_to_an_album_that_does_not_exist_changes_nothing() {
        let connection = fixture();
        assert_eq!(
            attach_album_art(&connection, 9_999, "hash").expect("attach"),
            0
        );
    }
}
