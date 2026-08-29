//! Report how much digital silence sits at each end of a track.
//!
//! Answers the question "which of my files does trim silence actually do
//! anything to", which is otherwise guesswork: most tracks have none, and
//! testing the setting on one of those looks identical to the setting being
//! broken.
//!
//! ```text
//! cargo run --example silence_probe -- <file-or-folder> [limit]
//! ```

use std::path::{Path, PathBuf};

use mp_audio::decode::TrackDecoder;

/// Matches the threshold the player trims at.
const SILENCE: f32 = 1e-4;

/// Report only tracks with at least this much silence at one end.
const INTERESTING_MS: f64 = 150.0;

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(root) = args.next().map(PathBuf::from) else {
        eprintln!("usage: silence_probe <file-or-folder> [limit]");
        std::process::exit(2);
    };
    let limit: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(60);

    let files = collect(&root, limit);
    if files.is_empty() {
        println!("no audio files found under {}", root.display());
        return;
    }

    println!("scanning {} files\n", files.len());
    println!("{:>9}  {:>9}  track", "leading", "trailing");
    println!("{}", "-".repeat(72));

    let mut interesting = 0;

    for path in &files {
        match measure(path) {
            Ok(Some((lead, trail))) => {
                if lead * 1000.0 >= INTERESTING_MS || trail * 1000.0 >= INTERESTING_MS {
                    interesting += 1;
                    println!(
                        "{:>8.2}s  {:>8.2}s  {}",
                        lead,
                        trail,
                        path.file_name().unwrap_or_default().to_string_lossy()
                    );
                }
            }
            Ok(None) => {}
            Err(err) => eprintln!("  ! {}: {err}", path.display()),
        }
    }

    println!();
    if interesting == 0 {
        println!(
            "nothing with more than {INTERESTING_MS:.0} ms at either end — \
             trim silence would be inaudible on this sample"
        );
    } else {
        println!(
            "{interesting} of {} tracks have silence worth trimming",
            files.len()
        );
    }
}

/// Seconds of digital silence at the start and end of the file.
fn measure(path: &Path) -> Result<Option<(f64, f64)>, String> {
    let mut decoder = TrackDecoder::open(path).map_err(|e| e.to_string())?;
    let rate = f64::from(decoder.sample_rate().max(1));

    let mut leading = 0u64;
    let mut trailing = 0u64;
    let mut seen_sound = false;

    loop {
        match decoder.next_chunk() {
            Ok(Some(chunk)) => {
                for frame in 0..chunk.frames {
                    let silent = chunk
                        .planes
                        .iter()
                        .all(|p| p.get(frame).is_none_or(|s| s.abs() < SILENCE));

                    if silent {
                        if seen_sound {
                            trailing += 1;
                        } else {
                            leading += 1;
                        }
                    } else {
                        seen_sound = true;
                        // Silence that turned out to be interior is not
                        // trailing after all.
                        trailing = 0;
                    }
                }
                decoder.recycle(chunk);
            }
            Ok(None) => break,
            Err(err) => return Err(err.to_string()),
        }
    }

    if !seen_sound {
        return Ok(None);
    }

    Ok(Some((leading as f64 / rate, trailing as f64 / rate)))
}

/// Audio files under `root`, up to `limit`.
fn collect(root: &Path, limit: usize) -> Vec<PathBuf> {
    if root.is_file() {
        return vec![root.to_path_buf()];
    }

    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if mp_core::format::classify(&path).is_supported() {
                out.push(path);
                if out.len() >= limit {
                    return out;
                }
            }
        }
    }

    out
}
