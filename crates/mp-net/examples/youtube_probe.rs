//! Resolve one YouTube link, and report exactly what happened.
//!
//! The unit tests answer from a scripted fake, which proves the parsing and
//! the matching and proves nothing at all about whether `yt-dlp` still reports
//! the fields this expects. That is the thing most likely to break here — the
//! service changes, `yt-dlp` follows it, and the shape of the answer moves —
//! so this makes the real calls.
//!
//! ```bash
//! cargo run -p mp-net --example youtube_probe -- "https://youtu.be/jNQXAC9IVRw"
//! cargo run -p mp-net --example youtube_probe -- --fetch "https://youtu.be/jNQXAC9IVRw"
//! cargo run -p mp-net --example youtube_probe -- --list "https://www.youtube.com/playlist?list=..."
//! ```
//!
//! Without `--fetch` nothing is downloaded but the metadata, which is the
//! quickest way to tell a link this build cannot use from one it can.
//!
//! `--list` reads what is in a playlist and fetches none of it. A link naming
//! a playlist and no video is listed without being asked.

use std::sync::Arc;

use mp_net::Activity;
use mp_net::tool::YtDlp;
use mp_net::youtube::{Client, Query};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let fetch = args.iter().any(|arg| arg == "--fetch");
    let list = args.iter().any(|arg| arg == "--list");

    let Some(link) = args.iter().find(|arg| !arg.starts_with("--")) else {
        eprintln!("usage: youtube_probe [--fetch | --list] <link>");
        std::process::exit(2);
    };

    let query = Query::new(link.clone());

    let listing = query.playlist_link().is_some() && (list || query.video_id().is_none());

    if query.names_account_list() {
        eprintln!("that is one of the signed-in user's own lists, which needs an account: {link}");
        std::process::exit(1);
    }

    if !listing && query.video_id().is_none() {
        eprintln!("that is not a link this recognises: {link}");
        eprintln!(
            "a link is never handed to yt-dlp unless it names a video or a playlist, so this stops here."
        );
        std::process::exit(1);
    }

    let Some(tool) = YtDlp::locate(None) else {
        eprintln!("yt-dlp is not on PATH. It is not bundled and never downloaded;");
        eprintln!("install it and try again.");
        std::process::exit(1);
    };

    println!("yt-dlp:   {}", tool.program().display());
    println!(
        "version:  {}",
        tool.version().as_deref().unwrap_or("(it would not say)")
    );
    if listing {
        println!("playlist: {}", query.playlist_id().unwrap_or_default());
        println!("asks for: {}", query.playlist_link().unwrap_or_default());
    } else {
        println!("video:    {}", query.video_id().unwrap_or_default());
    }
    println!();

    // A scratch cache that removes itself, so the probe never leaves anything
    // behind and never answers from a previous run.
    let dir = tempfile::tempdir().expect("a scratch directory");
    let activity = Arc::new(Activity::in_memory());
    let client = Client::new(
        Box::new(tool),
        dir.path().join("cache"),
        dir.path().join("audio"),
        Arc::clone(&activity),
    );

    if listing {
        match client.list(&query) {
            Ok(found) => {
                println!("  title:     {}", found.title);
                println!("  videos:    {}", found.entries.len());
                if let Some(at) = query.video_id().and_then(|id| found.position_of(&id)) {
                    println!("  starts at: {}", at + 1);
                }
                println!();
                for (number, entry) in found.entries.iter().enumerate().take(10) {
                    println!(
                        "  {:>3}. {} {:<40} {}",
                        number + 1,
                        entry.video_id,
                        entry.title.chars().take(40).collect::<String>(),
                        entry.artist
                    );
                }
                if found.entries.len() > 10 {
                    println!("  ... and {} more", found.entries.len() - 10);
                }
            }
            Err(trouble) => {
                println!("  nothing came back: {}", trouble.detail());
                println!("  means:             {}", trouble.message());
            }
        }
    } else {
        probe_video(&client, &query, fetch);
    }

    println!();
    for entry in activity.recent() {
        println!(
            "log: {:<18} {:<34} {:<10} {:>9} bytes  {}  {}",
            entry.source,
            entry.host,
            entry.outcome.as_str(),
            entry.bytes,
            entry.subject,
            entry.detail.as_deref().unwrap_or("")
        );
    }
}

/// Resolve one video, and fetch it if asked.
fn probe_video(client: &Client, query: &Query, fetch: bool) {
    match client.resolve(query) {
        Ok(resolved) => {
            println!("  title:     {}", resolved.title);
            println!("  artist:    {}", resolved.artist);
            println!("  album:     {}", resolved.album.as_deref().unwrap_or("-"));
            println!(
                "  duration:  {}",
                resolved.duration.map_or_else(
                    || "(not reported)".to_owned(),
                    |d| format!("{}s", d.as_secs())
                )
            );
            println!(
                "  thumbnail: {}",
                resolved.thumbnail.as_deref().unwrap_or("-")
            );

            if fetch {
                println!();
                match client.fetch_audio(&resolved) {
                    Ok(audio) => {
                        println!("  fetched:   {}", audio.path.display());
                        println!("  bytes:     {}", audio.bytes);
                        println!("  looks like: {}", sniff(&audio.path));
                    }
                    Err(trouble) => {
                        println!("  no audio:  {}", trouble.detail());
                        println!("  means:     {}", trouble.message());
                    }
                }
            }
        }
        Err(trouble) => {
            println!("  nothing came back: {}", trouble.detail());
            println!("  means:             {}", trouble.message());
        }
    }
}

/// The container, from its first bytes.
///
/// Enough to tell a real audio file from an error page, without needing a
/// decoder in the probe. An `ftyp` box at offset four is MP4 and its relatives,
/// which is what the AAC stream arrives in.
fn sniff(path: &std::path::Path) -> String {
    let Ok(bytes) = std::fs::read(path) else {
        return "unreadable".to_owned();
    };

    match bytes.get(..12) {
        Some([_, _, _, _, b'f', b't', b'y', b'p', a, b, c, d]) => {
            let brand: String = [*a, *b, *c, *d].iter().map(|c| *c as char).collect();
            format!("MP4 family, brand {}", brand.trim())
        }
        Some([b'O', b'g', b'g', b'S', ..]) => "Ogg - this build cannot decode Opus".to_owned(),
        Some([0x1A, 0x45, 0xDF, 0xA3, ..]) => {
            "Matroska or WebM - this build cannot decode Opus".to_owned()
        }
        Some([0xFF, 0xF1 | 0xF9, ..]) => "bare ADTS AAC".to_owned(),
        _ => "not something this recognises".to_owned(),
    }
}
