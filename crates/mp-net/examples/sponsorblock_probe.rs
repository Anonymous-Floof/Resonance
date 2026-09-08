//! Ask SponsorBlock what to skip in one video, and show what actually went out.
//!
//! The unit tests answer from a scripted fake, which proves the filtering and
//! the merging and proves nothing about whether the endpoint still answers the
//! way this expects. This makes the real request.
//!
//! ```bash
//! cargo run -p mp-net --example sponsorblock_probe -- dQw4w9WgXcQ
//! cargo run -p mp-net --example sponsorblock_probe -- "https://youtu.be/dQw4w9WgXcQ"
//! ```
//!
//! It prints the URL first, which is the point: the video identifier is not in
//! it. What goes out is four characters of a hash, and the answer covers every
//! video sharing them.

use std::sync::Arc;

use mp_net::sponsorblock::{Client, Query};
use mp_net::{Activity, youtube};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let Some(given) = args.first() else {
        eprintln!("usage: sponsorblock_probe <video id or link>");
        std::process::exit(2);
    };

    // A link is accepted for convenience, but only the identifier is used.
    let video_id = youtube::Query::new(given.clone())
        .video_id()
        .unwrap_or_else(|| given.clone());

    let query = Query::new(video_id.clone());

    println!("video:  {video_id}");
    println!("prefix: {}", query.hash_prefix());
    println!("GET     {}", query.url());

    assert!(
        !query.url().contains(&video_id),
        "the video identifier reached the url, which is the one thing this must never do"
    );
    println!("        (the identifier is not in that url, which is the whole idea)");
    println!();

    let dir = tempfile::tempdir().expect("a scratch directory");
    let activity = Arc::new(Activity::in_memory());
    let client = Client::new(dir.path(), Arc::clone(&activity));

    match client.fetch(&query) {
        Some(segments) => {
            println!("  {} segment(s) to skip:", segments.len());
            for segment in &segments {
                println!(
                    "    {:>8.1}s - {:>8.1}s   ({:.1}s)",
                    segment.start,
                    segment.end,
                    segment.length()
                );
            }
        }
        None => println!("  nothing to skip"),
    }

    println!();
    for entry in activity.recent() {
        println!(
            "log: {:<14} {:<20} {:<10} {:>7} bytes  {}",
            entry.source,
            entry.host,
            entry.outcome.as_str(),
            entry.bytes,
            entry.detail.as_deref().unwrap_or("")
        );
    }
}
