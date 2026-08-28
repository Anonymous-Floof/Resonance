//! Decode files end to end without a sound device and report what happened.
//!
//! This is the M1 correctness check: it proves the symphonia -> rubato path
//! produces the right number of samples, at the right rate, for real files —
//! including the awkward ones. It needs no audio hardware, so it also works in
//! CI.
//!
//! ```text
//! cargo run -p mp-audio --example decode_probe -- <file-or-folder>...
//! ```

use std::path::{Path, PathBuf};
use std::time::Instant;

use mp_audio::decode::TrackDecoder;
use mp_audio::format::{self, Support};
use mp_audio::resample::Resampler;

/// Rate to convert to, matching a typical Windows shared-mode device.
const TARGET_RATE: u32 = 48_000;
const TARGET_CHANNELS: usize = 2;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: decode_probe <file-or-folder>...");
        std::process::exit(2);
    }

    let mut files = Vec::new();
    for arg in &args {
        collect(Path::new(arg), &mut files);
    }
    files.sort();

    println!("probing {} file(s) -> {TARGET_RATE} Hz\n", files.len());

    let mut ok = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;

    for path in &files {
        match probe(path) {
            Outcome::Played(report) => {
                ok += 1;
                println!(
                    "  OK    {:<52} {:>6} Hz {}ch  {:>7.2}s -> {:>7.2}s  {:>5.0}x",
                    truncate(path),
                    report.source_rate,
                    report.source_channels,
                    report.source_seconds,
                    report.output_seconds,
                    report.speed
                );

                // The whole point of resampling: durations must agree even
                // though the sample counts differ.
                let drift = (report.output_seconds - report.source_seconds).abs();
                if drift > 0.05 {
                    println!("        ^ WARNING: {drift:.3}s of drift after conversion");
                }
            }
            Outcome::Skipped(reason) => {
                skipped += 1;
                println!("  SKIP  {:<52} {reason}", truncate(path));
            }
            Outcome::Failed(reason) => {
                failed += 1;
                println!("  FAIL  {:<52} {reason}", truncate(path));
            }
        }
    }

    println!("\n{ok} decoded, {skipped} unsupported, {failed} failed");

    if failed > 0 {
        std::process::exit(1);
    }
}

struct Report {
    source_rate: u32,
    source_channels: usize,
    source_seconds: f64,
    output_seconds: f64,
    /// Decode speed relative to real time.
    speed: f64,
}

enum Outcome {
    Played(Report),
    Skipped(String),
    Failed(String),
}

fn probe(path: &Path) -> Outcome {
    if let Support::Unsupported { reason } = format::classify(path) {
        return Outcome::Skipped(reason.to_owned());
    }

    let started = Instant::now();

    let mut decoder = match TrackDecoder::open(path) {
        Ok(decoder) => decoder,
        Err(err) => return Outcome::Failed(err.to_string()),
    };

    let source_rate = decoder.sample_rate();
    let source_channels = decoder.channels();

    let mut resampler = match Resampler::new(source_rate, TARGET_RATE, TARGET_CHANNELS) {
        Ok(resampler) => resampler,
        Err(err) => return Outcome::Failed(err.to_string()),
    };

    let mut source_frames = 0u64;
    let mut output_samples = 0u64;

    loop {
        match decoder.next_chunk() {
            Ok(Some(chunk)) => {
                source_frames += chunk.frames as u64;
                resampler.push(&chunk.planes, chunk.frames);
                decoder.recycle(chunk);

                while let Some(block) = resampler.pull() {
                    output_samples += block.len() as u64;
                }
            }
            Ok(None) => {
                if let Some(tail) = resampler.drain() {
                    output_samples += tail.len() as u64;
                }
                break;
            }
            Err(err) => return Outcome::Failed(err.to_string()),
        }
    }

    let source_seconds = source_frames as f64 / f64::from(source_rate);
    let output_seconds = (output_samples / TARGET_CHANNELS as u64) as f64 / f64::from(TARGET_RATE);
    let elapsed = started.elapsed().as_secs_f64().max(1e-9);

    Outcome::Played(Report {
        source_rate,
        source_channels,
        source_seconds,
        output_seconds,
        speed: source_seconds / elapsed,
    })
}

/// Gather audio files, recursing into directories.
fn collect(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_file() {
        if !matches!(format::classify(path), Support::NotAudio) {
            out.push(path.to_path_buf());
        }
        return;
    }

    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };

    for entry in entries.flatten() {
        collect(&entry.path(), out);
    }
}

/// Keep the table readable for deeply nested paths.
fn truncate(path: &Path) -> String {
    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into(),
    );

    if name.chars().count() <= 50 {
        name
    } else {
        let kept: String = name.chars().take(47).collect();
        format!("{kept}...")
    }
}
