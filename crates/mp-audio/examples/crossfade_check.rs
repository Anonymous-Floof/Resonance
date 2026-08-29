//! Check that a crossfade actually starts, without needing anyone to listen.
//!
//! A fade is judged by ear, but *whether one began at all* is a fact the engine
//! can report. This drives the real worker against real files and asks whether
//! a fade was triggered at the boundary between them.
//!
//! Runs at zero volume by default: the output device is opened, so the whole
//! path is exercised, but nothing is heard. Pass `--audible` to listen.
//!
//! ```text
//! cargo run -p mp-audio --example crossfade_check -- <two-or-more-files> [seconds]
//! cargo run -p mp-audio --example crossfade_check -- <folder> [seconds]
//! ```
//!
//! Given files it plays straight through, which is what the trigger is meant
//! to handle. Given a folder it seeks close to the end of the first track,
//! because waiting out a five minute song to watch one boundary is not a test
//! anybody runs twice.
//!
//! Exits non-zero if no fade started.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use mp_audio::engine::{AudioEngine, Event};
use mp_core::config::Playback;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let audible = args.iter().any(|a| a == "--audible");
    let positional: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();

    if positional.is_empty() {
        eprintln!("usage: crossfade_check <files-or-folder> [seconds] [--audible]");
        std::process::exit(2);
    }

    let seconds: f32 = positional
        .last()
        .and_then(|a| a.parse().ok())
        .filter(|s: &f32| *s > 0.0)
        .unwrap_or(4.0);

    let files: Vec<PathBuf> = positional
        .iter()
        .map(|a| PathBuf::from(a.as_str()))
        .filter(|p| p.is_file())
        .collect();

    let (tracks, seek) = if files.len() >= 2 {
        (files, false)
    } else {
        let root = PathBuf::from(positional[0].as_str());
        let scan = mp_audio::scan::scan(std::slice::from_ref(&root));
        if scan.tracks.len() < 2 {
            eprintln!("need at least 2 playable tracks in {}", root.display());
            std::process::exit(2);
        }
        (scan.tracks.into_iter().take(3).collect(), true)
    };

    println!("crossfade {seconds}s over {} tracks", tracks.len());
    for track in &tracks {
        println!(
            "  {}",
            track.file_name().unwrap_or_default().to_string_lossy()
        );
    }
    println!();

    let settings = Playback {
        volume: if audible { 0.2 } else { 0.0 },
        muted: !audible,
        crossfade_seconds: seconds,
        gapless: true,
        ..Playback::default()
    };

    let engine = match AudioEngine::new(&settings) {
        Ok(engine) => engine,
        Err(err) => {
            eprintln!("could not start the audio engine: {err}");
            std::process::exit(2);
        }
    };

    engine.play_now(tracks, 0);
    drain(&engine, Duration::from_millis(1200));

    let duration = engine.duration_secs().unwrap_or(0.0);
    println!("first track reports {duration:.1}s");
    if duration <= 0.0 {
        println!("  ! no duration, so no fade can be scheduled against it");
    }

    if seek {
        let target = ((duration - f64::from(seconds) - 1.0) / duration).clamp(0.0, 0.99);
        println!("seeking to {:.1}% and waiting", target * 100.0);
        engine.seek_fraction(target as f32);
    } else {
        println!("playing straight through and waiting");
    }
    println!();

    // Long enough to actually reach the boundary. A fixed timeout shorter
    // than the track reports "no fade" without ever having got there, which
    // looks exactly like the feature being broken.
    let budget = if seek { 30.0 } else { duration.max(1.0) + 20.0 };
    println!("waiting up to {budget:.0}s");
    println!();
    let deadline = Instant::now() + Duration::from_secs_f64(budget);
    let mut started = Vec::new();

    while Instant::now() < deadline {
        for event in engine.poll_events() {
            if let Event::TrackStarted { path, .. } = event {
                started.push(
                    path.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }

        if engine.fades() > 0 && started.len() >= 2 {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let fades = engine.fades();
    println!("fades started : {fades}");
    println!("tracks started: {started:?}");
    println!("xruns         : {}", engine.xruns());

    if fades == 0 {
        println!();
        println!("FAIL: a track boundary passed without a fade");
        std::process::exit(1);
    }

    println!();
    println!("OK: a crossfade was started");
}

/// Drain events for `how_long`, discarding them.
fn drain(engine: &AudioEngine, how_long: Duration) {
    let deadline = Instant::now() + how_long;
    while Instant::now() < deadline {
        let _ = engine.poll_events();
        std::thread::sleep(Duration::from_millis(50));
    }
}
