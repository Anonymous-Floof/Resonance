//! Builds a synthetic library of N tracks and times what the interface does
//! with it.
//!
//! The plan's M7 requirement is that a 30k-track library stays responsive, and
//! the only honest way to know is to have one. Real collections that size are
//! not lying around, so this fabricates one directly in the database — no
//! files, no tags, no decoding — which is exactly right for measuring the
//! *query* path rather than the scanner.
//!
//! Everything here is measured cold-ish: each timing opens its own statement
//! and reads every row it asks for, the same as the app does when a view
//! changes.
//!
//! ```text
//! cargo run -p mp-core --release --example perf_probe -- [tracks]
//! ```

use std::time::{Duration, Instant};

use mp_core::library::{Filter, Library, Order};

/// Roughly the shape of a real collection: a few hundred artists, a handful of
/// albums each, a dozen tracks an album.
const ARTISTS: usize = 400;
const ALBUMS_PER_ARTIST: usize = 6;
const GENRES: usize = 24;

fn main() -> anyhow::Result<()> {
    let wanted: usize = std::env::args()
        .nth(1)
        .and_then(|arg| arg.parse().ok())
        .unwrap_or(30_000);

    println!("building a synthetic library of {wanted} tracks…");
    let started = Instant::now();
    let library = build(wanted)?;
    println!("  built in {:?}\n", started.elapsed());

    let stats = library.stats()?;
    println!(
        "{} tracks, {} albums, {} artists\n",
        stats.tracks, stats.albums, stats.artists
    );

    // --- the query the Songs view runs on every sort change -----------------
    println!("Songs view, full track list:");
    for order in [
        Order::Title,
        Order::Artist,
        Order::Album,
        Order::DateAdded,
        Order::PlayCount,
    ] {
        let (elapsed, count) = time(|| Ok(library.tracks(&Filter::All, order, false)?.len()))?;
        println!(
            "  {:<12} {:>8.1} ms  ({count} rows)",
            format!("{order:?}"),
            ms(elapsed)
        );
    }

    // --- the grouped views --------------------------------------------------
    println!("\nBrowse views:");
    let (elapsed, count) = time(|| Ok(library.artists()?.len()))?;
    println!(
        "  {:<12} {:>8.1} ms  ({count} rows)",
        "artists",
        ms(elapsed)
    );

    let (elapsed, count) = time(|| Ok(library.albums(None, 1)?.len()))?;
    println!("  {:<12} {:>8.1} ms  ({count} rows)", "albums", ms(elapsed));

    let (elapsed, count) = time(|| Ok(library.genres()?.len()))?;
    println!("  {:<12} {:>8.1} ms  ({count} rows)", "genres", ms(elapsed));

    let (elapsed, count) = time(|| Ok(library.folders()?.len()))?;
    println!(
        "  {:<12} {:>8.1} ms  ({count} rows)",
        "folders",
        ms(elapsed)
    );

    // --- search, which runs on every keystroke ------------------------------
    println!("\nSearch (runs per keystroke):");
    for needle in ["track", "artist 250", "album 1", "genre 7", "zzz"] {
        let (elapsed, count) = time(|| Ok(library.search(needle, Some(500))?.len()))?;
        println!(
            "  {:<12} {:>8.1} ms  ({count} hits)",
            format!("{needle:?}"),
            ms(elapsed)
        );
    }

    // --- what the UI actually does on one state change ----------------------
    //
    // `LibraryState::refresh` re-runs every query behind the screen, not just
    // the visible one, and it is marked stale by a change of sort, of focus,
    // *and* by every keystroke in the search box. So this — not any single
    // query above — is what a keystroke costs.
    println!("\nOne full refresh, as the UI performs it:");

    let (elapsed, _) = time(|| {
        let tracks = library.tracks(&Filter::All, Order::Title, false)?.len();
        let artists = library.artists()?.len();
        let albums = library.albums(None, 1)?.len();
        let genres = library.genres()?.len();
        let folders = library.folders()?.len();
        Ok(tracks + artists + albums + genres + folders)
    })?;
    println!(
        "  {:<22} {:>8.1} ms",
        "first paint (all of it)",
        ms(elapsed)
    );

    // A keystroke now re-reads the tracks and nothing else, and the search is
    // capped rather than materialising the whole library.
    let (elapsed, hits) = time(|| {
        Ok(library
            .tracks(&Filter::Search("track".into()), Order::Title, false)?
            .len())
    })?;
    println!(
        "  {:<22} {:>8.1} ms  ({hits} hits materialised)",
        "one keystroke",
        ms(elapsed)
    );

    // And a change of sort, which is the other thing that used to re-read
    // everything.
    let (elapsed, _) = time(|| Ok(library.tracks(&Filter::All, Order::PlayCount, true)?.len()))?;
    println!("  {:<22} {:>8.1} ms", "change of sort", ms(elapsed));

    // --- what the full list actually costs to hold --------------------------
    let tracks = library.tracks(&Filter::All, Order::Title, false)?;
    println!("\nMemory of the materialised list:");
    println!(
        "  {:>8} bytes  per Track struct",
        size_of::<mp_core::library::Track>()
    );
    println!(
        "  {:>8.1} MB     estimated for {} rows",
        megabytes(&tracks),
        tracks.len()
    );

    println!("\nBudget: a view change should stay under ~100 ms to feel instant.");

    Ok(())
}

/// Time one operation, returning how long it took and whatever it counted.
fn time<T>(mut operation: impl FnMut() -> anyhow::Result<T>) -> anyhow::Result<(Duration, T)> {
    let started = Instant::now();
    let value = operation()?;
    Ok((started.elapsed(), value))
}

fn ms(elapsed: Duration) -> f64 {
    elapsed.as_secs_f64() * 1000.0
}

/// Rough heap cost of the whole list: the structs plus the strings they own.
fn megabytes(tracks: &[mp_core::library::Track]) -> f64 {
    let structs = std::mem::size_of_val(tracks);

    let strings: usize = tracks
        .iter()
        .map(|track| {
            track.title.capacity()
                + track.artist.capacity()
                + track.album.capacity()
                + track.art_id.as_ref().map_or(0, String::capacity)
                + track.path.as_os_str().len()
        })
        .sum();

    (structs + strings) as f64 / (1024.0 * 1024.0)
}

/// Write `count` tracks straight into a fresh in-memory library.
///
/// Inserted through raw SQL rather than the scanner because the scanner reads
/// files, and there are no files here. The shape of the rows matches what a
/// real scan produces, which is all the query planner cares about.
fn build(count: usize) -> anyhow::Result<Library> {
    let library = Library::in_memory()?;
    let connection = library.connection();

    connection.execute_batch("BEGIN")?;

    for index in 0..ARTISTS {
        connection.execute(
            "INSERT INTO artists (name, sort_name) VALUES (?1, ?2)",
            rusqlite::params![format!("Artist {index}"), format!("artist {index:05}")],
        )?;
    }

    for index in 0..GENRES {
        connection.execute(
            "INSERT INTO genres (name, sort_name) VALUES (?1, ?2)",
            rusqlite::params![format!("Genre {index}"), format!("genre {index:03}")],
        )?;
    }

    let album_count = ARTISTS * ALBUMS_PER_ARTIST;
    for index in 0..album_count {
        let artist = (index % ARTISTS) + 1;
        connection.execute(
            "INSERT INTO albums (title, sort_title, artist_id, year) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                format!("Album {index}"),
                format!("album {index:05}"),
                artist as i64,
                1970 + (index % 55) as i64
            ],
        )?;
    }

    for index in 0..count {
        let album = (index % album_count) + 1;
        let artist = (album - 1) % ARTISTS + 1;

        connection.execute(
            "INSERT INTO tracks (
                 path, folder, file_name, title, sort_title, artist_id, album_id,
                 track_no, disc_no, year, duration_ms, added_at, last_seen_at,
                 play_count, mtime, size, tagged
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1, ?9, ?10, ?11, ?11, ?12, 0, 0, 1)",
            rusqlite::params![
                format!("D:\\Music\\Artist {artist}\\Album {album}\\{index:06} Track.mp3"),
                format!("D:\\Music\\Artist {artist}\\Album {album}"),
                format!("{index:06} Track.mp3"),
                format!("Track {index}"),
                format!("track {index:06}"),
                artist as i64,
                album as i64,
                (index % 14 + 1) as i64,
                (1970 + index % 55) as i64,
                (1_700_000_000i64 + index as i64),
                (index % 400) as i64,
                (120_000 + (index % 240) * 1000) as i64,
            ],
        )?;

        connection.execute(
            "INSERT INTO track_genres (track_id, genre_id) VALUES (?1, ?2)",
            rusqlite::params![(index + 1) as i64, (index % GENRES + 1) as i64],
        )?;

        // The scanner maintains this by hand rather than by trigger, so the
        // probe has to as well. Without it every search returns nothing and
        // the timing below measures an empty index — which is fast, and a lie.
        connection.execute(
            "INSERT INTO tracks_fts(rowid, title, artist, album, genre)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                (index + 1) as i64,
                format!("Track {index}"),
                format!("Artist {artist}"),
                format!("Album {album}"),
                format!("Genre {}", index % GENRES),
            ],
        )?;
    }

    connection.execute_batch("COMMIT")?;

    // The planner makes very different choices with and without statistics,
    // and a real library has them. Measuring without would flatter the result.
    connection.execute_batch("ANALYZE")?;

    Ok(library)
}
