//! Scan a real folder and report what the library made of it — no window, no
//! sound device.
//!
//! This is the check that matters for M2: unit tests prove the index behaves,
//! but only a real collection shows whether the *metadata* is any good. It
//! prints what the user would actually see in each view, so bad artist splits
//! and missing covers are obvious.
//!
//! ```text
//! cargo run --release -p mp-core --example library_probe -- "D:\Music"
//! ```

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use mp_core::library::{Filter, Library, Order, Progress, ScanOptions};

fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let Some(root) = args.next().map(PathBuf::from) else {
        eprintln!("usage: library_probe <folder> [more folders...]");
        std::process::exit(2);
    };

    let mut roots = vec![root];
    roots.extend(args.map(PathBuf::from));

    // A scratch database, so a probe run never disturbs the real library.
    let scratch = std::env::temp_dir().join(format!("resonance-probe-{}", std::process::id()));
    std::fs::create_dir_all(&scratch)?;
    let mut library = Library::open_at(scratch.join("probe.db"), scratch.join("art"))?;

    let options = ScanOptions {
        roots: roots.clone(),
        ..ScanOptions::default()
    };

    for root in &roots {
        println!("scanning {}", root.display());
    }

    let progress = Progress::new();
    let summary = library.scan_blocking(&options, &progress)?;

    rule("scan");
    println!("  {}", summary.describe());
    println!("  added        {}", summary.added);
    println!("  unchanged    {}", summary.unchanged);
    println!("  unplayable   {}", summary.unplayable);
    println!("  unreadable   {}", summary.unreadable);
    println!("  too short    {}", summary.too_short);
    println!("  failed       {}", summary.failed);
    println!("  artists rec. {}", summary.artists_recovered);
    println!("  elapsed      {:.2}s", summary.elapsed.as_secs_f32());

    let stats = library.stats()?;
    rule("library");
    println!("  tracks   {}", stats.tracks);
    println!("  artists  {}", stats.artists);
    println!("  albums   {}", stats.albums);
    println!("  genres   {}", stats.genres);
    println!("  folders  {}", stats.folders);
    println!("  total    {}", hms(stats.total_duration));
    println!(
        "  tagged   {} of {} ({} came from the filename)",
        stats.tracks - stats.untagged,
        stats.tracks,
        stats.untagged
    );

    let with_art = library
        .tracks(&Filter::All, Order::Title, false)?
        .iter()
        .filter(|track| track.art_id.is_some())
        .count();
    println!("  artwork  {with_art} of {} tracks", stats.tracks);

    rule("artists (top 15 by track count)");
    let mut artists = library.artists()?;
    artists.sort_by_key(|a| std::cmp::Reverse(a.track_count));
    for artist in artists.iter().take(15) {
        println!(
            "  {:>4}  {:<44} {} album(s)",
            artist.track_count,
            truncate(&artist.name, 44),
            artist.album_count
        );
    }

    let real_albums = library.albums(None, 2)?.len();
    println!(
        "  albums with more than one track: {real_albums} of {}",
        stats.albums
    );

    rule("albums (top 15)");
    let mut albums = library.albums(None, 1)?;
    albums.sort_by_key(|a| std::cmp::Reverse(a.track_count));
    for album in albums.iter().take(15) {
        println!(
            "  {:>4}  {:<34} {:<24} {}",
            album.track_count,
            truncate(&album.title, 34),
            truncate(&album.artist, 24),
            album
                .year
                .map_or_else(|| "----".to_owned(), |y| y.to_string())
        );
    }

    rule("genres");
    for genre in library.genres()?.iter().take(20) {
        println!("  {:>4}  {}", genre.track_count, genre.name);
    }

    rule("folders");
    for folder in library.folders()? {
        println!(
            "  {:>4}  {:<40} {}",
            folder.track_count,
            truncate(&folder.name, 40),
            hms(folder.total_duration)
        );
    }

    rule("sample tracks");
    let tracks = library.tracks(&Filter::All, Order::Artist, false)?;
    for track in tracks.iter().step_by((tracks.len() / 15).max(1)).take(15) {
        println!(
            "  {:<38} {:<24} {:>7}  {}{}",
            truncate(&track.title, 38),
            truncate(&track.artist, 24),
            track
                .duration
                .map_or_else(|| "  --:--".to_owned(), |d| format!("{:>7}", hms(d))),
            if track.tagged { "tag" } else { "name" },
            if track.art_id.is_some() { " art" } else { "" }
        );
    }

    if stats.unplayable > 0 {
        rule("unplayable");
        for (path, reason) in library.unplayable()? {
            println!(
                "  {} — {reason}",
                path.file_name().unwrap_or_default().to_string_lossy()
            );
        }
    }

    rule("search");
    for term in ["love", "night", "the"] {
        let hits = library.search(term, Some(5))?;
        println!("  {term:<8} {} hit(s)", hits.len());
        for hit in hits.iter().take(3) {
            println!("           {} — {}", truncate(&hit.title, 40), hit.artist);
        }
    }

    // The claim the incremental design is built on, measured rather than
    // asserted: a second scan with nothing changed must not reopen any file.
    rule("rescan (nothing changed)");
    let progress = Progress::new();
    let again = library.scan_blocking(&options, &progress)?;
    println!("  {}", again.describe());
    println!("  files reopened  {}", again.added + again.updated);
    println!("  elapsed         {:.3}s", again.elapsed.as_secs_f32());
    println!(
        "  speed-up        {:.0}x",
        summary.elapsed.as_secs_f32() / again.elapsed.as_secs_f32().max(0.0001)
    );

    println!("\nscratch database left at {}", scratch.display());
    Ok(())
}

fn rule(title: &str) {
    println!(
        "\n=== {title} {}",
        "=".repeat(60_usize.saturating_sub(title.len()))
    );
}

fn hms(duration: Duration) -> String {
    let total = duration.as_secs();
    let (hours, minutes, seconds) = (total / 3600, (total % 3600) / 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    text.chars()
        .take(width.saturating_sub(1))
        .collect::<String>()
        + "…"
}
