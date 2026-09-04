//! Ask MusicBrainz and the Cover Art Archive for one album, and report exactly
//! what happened.
//!
//! The unit tests answer from a scripted fake, which proves the matching logic
//! and proves nothing at all about whether the field names are right. This
//! makes real requests to both services.
//!
//! ```bash
//! cargo run -p mp-net --example artwork_probe -- "Radiohead" "Kid A"
//! ```

use std::sync::Arc;

use mp_net::Activity;
use mp_net::artwork::{Client, Query};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let (artist, album) = match args.as_slice() {
        [artist, album, ..] => (artist.clone(), album.clone()),
        _ => {
            eprintln!("usage: artwork_probe <artist> <album>");
            std::process::exit(2);
        }
    };

    let query = Query::new(artist, album);

    // A scratch cache that removes itself, so the probe never leaves a
    // directory behind and never answers from a previous run.
    let dir = tempfile::tempdir().expect("a scratch directory");
    let activity = Arc::new(Activity::in_memory());
    let client = Client::new(dir.path(), Arc::clone(&activity));

    println!("GET {}", query.search_url());

    match client.fetch(&query) {
        Some(cover) => {
            println!("  release:   {}", cover.release_id);
            println!("  bytes:     {}", cover.bytes.len());
            println!(
                "  served by: {}",
                cover.served_by.as_deref().unwrap_or("(not reported)")
            );
            // Enough to tell a real image from an error page, without needing
            // an image decoder in the probe.
            println!("  looks like: {}", sniff(&cover.bytes));
        }
        None => println!("  no cover"),
    }

    println!();
    for entry in activity.recent() {
        println!(
            "log: {:<16} {:<28} {:<10} {:>7} bytes  {}",
            entry.source,
            entry.host,
            entry.outcome.as_str(),
            entry.bytes,
            entry.detail.as_deref().unwrap_or("")
        );
    }
}

/// The format, from its magic bytes.
fn sniff(bytes: &[u8]) -> &'static str {
    match bytes {
        [0xFF, 0xD8, 0xFF, ..] => "JPEG",
        [0x89, b'P', b'N', b'G', ..] => "PNG",
        [b'R', b'I', b'F', b'F', ..] => "WebP (RIFF)",
        [b'G', b'I', b'F', ..] => "GIF",
        _ => "not an image this recognises",
    }
}
