//! Drive the visualiser feed through the real engine, without a window.
//!
//! The unit tests analyse a buffer that was handed straight to the analyzer.
//! This checks the part they cannot: that audio actually survives the trip from
//! a file, through the decoder, the resampler, the ring, the DSP chain, the tap
//! and out to the analyzer — at the device's own sample rate, with the real
//! callback driving it.
//!
//! It plays generated tones of known frequency and asks where the spectrum
//! thinks they are. A wrong answer here means something in that chain is
//! resampling, downmixing or windowing incorrectly, none of which is visible in
//! a screenshot.
//!
//! ```text
//! cargo run -p mp-audio --example viz_check
//! ```
//!
//! Set `RESONANCE_SILENT=1` to run it at zero output — the tap sits before the
//! volume control, so the check still works with nothing audible.
//!
//! Exits non-zero if any check fails.

use std::path::Path;
use std::time::{Duration, Instant};

use mp_audio::engine::{AudioEngine, Command};
use mp_audio::viz::{Analyzer, analyzer};
use mp_core::config::{Playback, Visualizer as VizSettings};

/// Tones to check, in Hz. Spread across the display so a mapping that is
/// stretched or offset shows up rather than cancelling out.
const TONES: [f32; 4] = [110.0, 440.0, 1_760.0, 7_040.0];

const FILE_RATE: u32 = 44_100;
const SECONDS: u32 = 4;

fn main() {
    let dir = std::env::temp_dir().join(format!("resonance-viz-check-{}", std::process::id()));
    if let Err(err) = std::fs::create_dir_all(&dir) {
        eprintln!("could not create a scratch directory: {err}");
        std::process::exit(2);
    }

    let silent = std::env::var_os("RESONANCE_SILENT").is_some();
    if silent {
        println!("running silently (RESONANCE_SILENT is set)\n");
    }

    let settings = Playback {
        volume: if silent { 0.0 } else { 0.15 },
        muted: silent,
        // One tone per track, so a boundary never lands inside a measurement.
        gapless: false,
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

    let rate = engine.shared().device_rate();
    let channels = engine.shared().device_channels();
    println!("device: {rate} Hz, {channels} ch");
    if rate != FILE_RATE {
        println!("(files are {FILE_RATE} Hz, so the resampler is in the path — good)\n");
    } else {
        println!();
    }

    let mut monitor = None;
    let mut analyzer = Analyzer::new(rate.max(1));
    let viz = VizSettings {
        bar_count: 64,
        // No smoothing: each reading should reflect the audio in front of it.
        smoothing: 0.0,
        sensitivity: 1.0,
        ..VizSettings::default()
    };

    let mut failures = 0;
    let mut checked = 0;

    for tone in TONES {
        let path = dir.join(format!("tone-{tone:.0}.wav"));
        write_tone(&path, tone);

        engine.send(Command::PlayNow {
            tracks: vec![path.clone()],
            start: 0,
        });

        // Let the ring fill and the callback get going before measuring.
        std::thread::sleep(Duration::from_millis(900));

        if monitor.is_none() {
            monitor = engine.take_visualizer();
        }
        let Some(feed) = monitor.as_mut() else {
            eprintln!("FAIL  the engine never published a visualiser feed");
            std::process::exit(1);
        };

        // Several readings, taken like a UI would: poll, analyse, wait a frame.
        let mut peak_bar = 0usize;
        let mut peak_level = 0.0_f32;
        let mut sample_count = 0u64;

        let deadline = Instant::now() + Duration::from_millis(600);
        while Instant::now() < deadline {
            let before = feed.received();
            let frame = analyzer.analyze(feed, &viz, 1.0 / 60.0);
            sample_count += feed.received() - before;

            if let Some((index, level)) = frame
                .bars
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
                && *level > peak_level
            {
                peak_level = *level;
                peak_bar = index;
            }

            std::thread::sleep(Duration::from_millis(16));
        }

        let want = analyzer::bar_for_hz(tone, viz.bar_count, rate);
        let drift = peak_bar.abs_diff(want);
        // One bar either side: a 64-bar log display is about a sixth of an
        // octave per bar, and the tone sits wherever it sits inside one.
        let ok = drift <= 1 && peak_level > 0.5;
        checked += 1;

        println!(
            "{}  {tone:>7.0} Hz -> bar {peak_bar:>2} (expected {want:>2}), level {peak_level:.3}, {sample_count} samples",
            if ok { "PASS " } else { "FAIL " }
        );

        if !ok {
            failures += 1;
        }
    }

    // Silence has to read as silence, or the display twitches forever between
    // tracks.
    engine.send(Command::Stop);
    std::thread::sleep(Duration::from_millis(600));

    if let Some(feed) = monitor.as_mut() {
        // Several frames, so the starvation logic has time to settle the
        // display the way it would in front of a user.
        let mut frame = analyzer.analyze(feed, &viz, 1.0 / 60.0).clone();
        for _ in 0..40 {
            frame = analyzer.analyze(feed, &viz, 1.0 / 60.0).clone();
        }
        let loudest = frame.bars.iter().copied().fold(0.0_f32, f32::max);
        let quiet = !frame.active && loudest < 0.05;
        checked += 1;

        println!(
            "{}  after stop: active {}, loudest bar {loudest:.4}",
            if quiet { "PASS " } else { "FAIL " },
            frame.active
        );
        if !quiet {
            failures += 1;
        }
    }

    println!(
        "\nunderruns {}, dropped {}",
        engine.xruns(),
        engine.dropped()
    );
    let _ = std::fs::remove_dir_all(&dir);

    println!("\n{}/{} checks passed", checked - failures, checked);
    if failures > 0 {
        std::process::exit(1);
    }
}

/// Write a stereo 16-bit sine as a WAV file.
fn write_tone(path: &Path, freq: f32) {
    const CHANNELS: usize = 2;
    let frames = (FILE_RATE * SECONDS) as usize;
    let data_bytes = (frames * CHANNELS * 2) as u32;

    let mut out = Vec::with_capacity(44 + data_bytes as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&(CHANNELS as u16).to_le_bytes());
    out.extend_from_slice(&FILE_RATE.to_le_bytes());
    out.extend_from_slice(&(FILE_RATE * CHANNELS as u32 * 2).to_le_bytes());
    out.extend_from_slice(&((CHANNELS * 2) as u16).to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_bytes.to_le_bytes());

    for n in 0..frames {
        let phase = std::f64::consts::TAU * f64::from(freq) * n as f64 / f64::from(FILE_RATE);
        // Short of full scale so nothing in the path has to limit.
        let value = (phase.sin() * 0.7 * f64::from(i16::MAX)).round() as i16;
        for _ in 0..CHANNELS {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }

    if let Err(err) = std::fs::write(path, out) {
        eprintln!("could not write {}: {err}", path.display());
        std::process::exit(2);
    }
}
