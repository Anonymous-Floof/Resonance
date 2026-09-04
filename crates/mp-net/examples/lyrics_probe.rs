//! Ask LRCLIB for one track and report exactly what happened.
//!
//! The unit tests answer from a scripted fake, which proves the logic and
//! proves nothing at all about whether the field names are right. This makes
//! one real request.
//!
//! ```bash
//! cargo run -p mp-net --example lyrics_probe -- "Radiohead" "Creep" "Pablo Honey" 239
//! ```

use std::sync::Arc;
use std::time::Duration;

use mp_net::Activity;
use mp_net::lyrics::{Client, Query};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let (artist, title) = match args.as_slice() {
        [artist, title, ..] => (artist.clone(), title.clone()),
        _ => {
            eprintln!("usage: lyrics_probe <artist> <title> [album] [duration-seconds]");
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

    match client.fetch(&query) {
        Some(found) => {
            println!("  synced:       {}", found.is_synced());
            println!("  instrumental: {}", found.instrumental);

            match found.best() {
                // One line only. The point is to prove the shape came back,
                // not to print somebody else's lyrics to a terminal.
                Some(text) => {
                    let first = text.lines().next().unwrap_or_default();
                    println!("  lines:        {}", text.lines().count());
                    println!("  first line:   {first}");
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
