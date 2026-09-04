//! Ask LRCLIB for one track and report exactly what happened.
//!
//! The unit tests answer from a scripted fake, which proves the logic and
//! proves nothing at all about whether the field names are right. This makes
//! one real request.
//!
//! ```bash
//! cargo run -p mp-net --example lyrics_probe -- "Radiohead" "Creep" "Pablo Honey" 239
//! ```
//!
//! Pass `--any-release` to allow the relaxed retry, which is the quickest way
//! to see whether a track that misses is missing altogether or merely tagged
//! with an album and duration nothing recognises.

use std::sync::Arc;
use std::time::Duration;

use mp_net::Activity;
use mp_net::lyrics::{Client, Match, Query};

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();

    let matching = if let Some(flag) = args.iter().position(|a| a == "--any-release") {
        args.remove(flag);
        Match::AnyRelease
    } else {
        Match::Exact
    };

    let (artist, title) = match args.as_slice() {
        [artist, title, ..] => (artist.clone(), title.clone()),
        _ => {
            eprintln!(
                "usage: lyrics_probe [--any-release] <artist> <title> [album] [duration-seconds]"
            );
            std::process::exit(2);
        }
    };

    let mut query = Query::new(artist, title);
    if let Some(album) = args.get(2) {
        query = query.with_album(album.clone());
    }
    if let Some(seconds) = args.get(3).and_then(|s| s.parse().ok()) {
        query = query.with_duration(Duration::from_secs(seconds));
    }

    // A scratch cache that removes itself, so running the probe never leaves a
    // directory behind and never answers from a previous run's cache.
    let dir = tempfile::tempdir().expect("a scratch directory");
    let activity = Arc::new(Activity::in_memory());
    let client = Client::new(dir.path(), Arc::clone(&activity));

    println!("GET {}", query.url());

    match client.fetch(&query, matching) {
        Some(found) => {
            println!("  synced:       {}", found.is_synced());
            println!("  instrumental: {}", found.instrumental);

            match found.best() {
                // The shape of the answer, not the answer itself. Whether the
                // words arrived and parsed is what this is checking, and that
                // is fully answered by the count and the first timestamp —
                // printing somebody else's lyrics to a terminal is not the
                // probe's business.
                Some(text) => {
                    println!("  lines:        {}", text.lines().count());
                    println!(
                        "  starts at:    {}",
                        text.lines()
                            .find_map(|line| line.split(']').next())
                            .map(|stamp| stamp.trim_start_matches('['))
                            .unwrap_or("no timestamp")
                    );
                }
                None => println!("  no words (instrumental)"),
            }
        }
        None => println!("  no answer"),
    }

    println!();
    for entry in activity.recent() {
        println!(
            "log: {} {} {} bytes {}",
            entry.outcome.as_str(),
            entry.host,
            entry.bytes,
            entry.detail.unwrap_or_default()
        );
    }
}
