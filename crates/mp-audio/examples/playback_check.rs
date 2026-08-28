//! Drive the real engine against real files and assert it behaves.
//!
//! This exercises the parts unit tests cannot reach: the cpal callback, the
//! ring buffer, the flush handshake, and the worker's track-advance logic. It
//! opens the actual output device, so audio really is produced — turn the
//! volume down if that is unwelcome.
//!
//! ```text
//! cargo run -p mp-audio --example playback_check -- <folder>
//! ```
//!
//! Exits non-zero if any check fails.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use mp_audio::engine::{AudioEngine, Command, Event};
use mp_core::config::Playback;

/// Long enough for the ring to fill and the position to move visibly.
const SETTLE: Duration = Duration::from_millis(1200);

fn main() {
    let Some(root) = std::env::args().nth(1) else {
        eprintln!("usage: playback_check <folder>");
        std::process::exit(2);
    };

    // Optional second argument: run as a soak test for N seconds instead of
    // the scripted check. Useful for confirming the counters stay at zero over
    // a long listening session, not just a few seconds.
    let soak_secs: Option<u64> = std::env::args().nth(2).and_then(|a| a.parse().ok());

    let scan = mp_audio::scan::scan(&[PathBuf::from(&root)]);
    if scan.tracks.len() < 2 {
        eprintln!("need at least 2 playable tracks in {root}");
        std::process::exit(2);
    }

    // A handful is plenty and keeps the run short.
    let tracks: Vec<PathBuf> = scan.tracks.into_iter().take(4).collect();
    println!("using {} tracks from {root}\n", tracks.len());

    let mut checks = Checks::default();

    // Keep the volume low: this is a test, not a performance. Set
    // `RESONANCE_SILENT=1` to run it at zero output, so the whole engine is
    // still exercised on a machine somebody is using for something else.
    let silent = std::env::var_os("RESONANCE_SILENT").is_some();
    if silent {
        println!(
            "running silently (RESONANCE_SILENT is set)
"
        );
    }

    let settings = Playback {
        volume: if silent { 0.0 } else { 0.15 },
        muted: silent,
        ..Playback::default()
    };

    let engine = match AudioEngine::new(&settings) {
        Ok(engine) => engine,
        Err(err) => {
            eprintln!("could not start the audio engine: {err}");
            eprintln!("(this check needs a working output device)");
            std::process::exit(2);
        }
    };

    if let Some(secs) = soak_secs {
        soak(&engine, tracks, secs);
        return;
    }

    // -- play ---------------------------------------------------------------

    engine.play_now(tracks.clone(), 0);
    let events = wait(&engine, SETTLE);

    checks.assert("engine reports playing", engine.status().is_playing());
    checks.assert(
        "a TrackStarted event arrived",
        events
            .iter()
            .any(|e| matches!(e, Event::TrackStarted { .. })),
    );

    let first = engine.position_secs();
    checks.assert("position advances from zero", first > 0.0);
    println!("      position after {SETTLE:?}: {first:.2}s");

    // -- position keeps moving ---------------------------------------------

    wait(&engine, Duration::from_millis(800));
    let second = engine.position_secs();
    checks.assert("position keeps advancing", second > first);
    println!("      position now: {second:.2}s");

    // Playback should track wall-clock time closely. A large drift means the
    // resampler ratio or the frame accounting is wrong.
    let expected = first + 0.8;
    let drift = (second - expected).abs();
    checks.assert(
        &format!("playback runs at real time (drift {drift:.3}s)"),
        drift < 0.25,
    );

    // -- pause and resume ---------------------------------------------------

    engine.send(Command::Pause);
    wait(&engine, Duration::from_millis(400));
    let paused_at = engine.position_secs();
    wait(&engine, Duration::from_millis(400));

    checks.assert(
        "position holds while paused",
        (engine.position_secs() - paused_at).abs() < 0.05,
    );

    engine.send(Command::Play);
    wait(&engine, Duration::from_millis(600));
    checks.assert("resumes after pause", engine.position_secs() > paused_at);

    // -- seek ---------------------------------------------------------------

    if let Some(total) = engine.duration_secs() {
        engine.seek_fraction(0.5);
        wait(&engine, Duration::from_millis(700));

        let after = engine.position_secs();
        let target = total * 0.5;
        println!("      seek to {target:.1}s landed at {after:.1}s");

        checks.assert(
            "seek lands near the requested point",
            (after - target).abs() < 2.0,
        );
        checks.assert("playback continues after seeking", {
            let before = engine.position_secs();
            wait(&engine, Duration::from_millis(500));
            engine.position_secs() > before
        });
    } else {
        println!("      (track has no known duration; skipping the seek check)");
    }

    // -- skip ---------------------------------------------------------------

    engine.next();
    let started: Vec<PathBuf> = wait(&engine, SETTLE)
        .into_iter()
        .filter_map(|e| match e {
            Event::TrackStarted { path, .. } => Some(path),
            _ => None,
        })
        .collect();

    checks.assert("skipping starts another track", !started.is_empty());
    if let Some(path) = started.last() {
        println!("      now playing: {}", path.display());
    }
    checks.assert("still playing after a skip", engine.status().is_playing());

    // -- the headline check -------------------------------------------------

    let xruns = engine.xruns();
    println!("\n      underruns: {xruns}");
    checks.assert("no buffer underruns during the whole run", xruns == 0);

    let dropped = engine.dropped();
    println!("      dropped samples: {dropped}");
    checks.assert("no samples dropped on the way to the ring", dropped == 0);

    // -- report -------------------------------------------------------------

    println!("\n{} passed, {} failed", checks.passed, checks.failed);
    if checks.failed > 0 {
        std::process::exit(1);
    }
}

/// Sleep while draining events, returning everything seen.
///
/// The events must be collected here rather than polled for afterwards: the
/// channel is drained by this loop, so a later `poll_events` would find it
/// empty and any assertion against it would pass vacuously.
fn wait(engine: &AudioEngine, duration: Duration) -> Vec<Event> {
    let deadline = Instant::now() + duration;
    let mut seen = Vec::new();

    while Instant::now() < deadline {
        seen.extend(engine.poll_events());
        std::thread::sleep(Duration::from_millis(20));
    }

    seen
}

#[derive(Default)]
struct Checks {
    passed: usize,
    failed: usize,
}

impl Checks {
    fn assert(&mut self, what: &str, ok: bool) {
        if ok {
            self.passed += 1;
            println!("  ok    {what}");
        } else {
            self.failed += 1;
            println!("  FAIL  {what}");
        }
    }
}

/// Play continuously for `secs`, reporting the health counters as it goes.
fn soak(engine: &AudioEngine, tracks: Vec<PathBuf>, secs: u64) {
    println!(
        "soaking for {secs}s
"
    );
    engine.play_now(tracks, 0);

    let started = Instant::now();
    let mut last_report = Instant::now();

    while started.elapsed() < Duration::from_secs(secs) {
        for event in engine.poll_events() {
            if let Event::TrackStarted { path, .. } = event {
                println!("  -> {}", path.display());
            }
        }

        if last_report.elapsed() >= Duration::from_secs(10) {
            println!(
                "  {:>4}s  pos {:>7.1}s  underruns {}  dropped {}",
                started.elapsed().as_secs(),
                engine.position_secs(),
                engine.xruns(),
                engine.dropped()
            );
            last_report = Instant::now();
        }

        std::thread::sleep(Duration::from_millis(50));
    }

    let xruns = engine.xruns();
    let dropped = engine.dropped();
    println!(
        "
final: underruns {xruns}, dropped {dropped}"
    );

    if xruns == 0 && dropped == 0 {
        println!("clean");
    } else {
        println!("PROBLEM: the audio path is losing samples");
        std::process::exit(1);
    }
}
