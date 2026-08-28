//! The plan's gapless check, run without a sound device.
//!
//! One continuous sine is written out as two WAV files, split mid-waveform.
//! Played back-to-back through the real decode and resample path, the join has
//! to be indistinguishable from the middle of a single file — if the two halves
//! reconstruct the original within a sample or two, there is no gap and no
//! click, because either would show up as a discontinuity right at the seam.
//!
//! Splitting mid-cycle is deliberate: a split at a zero crossing would hide a
//! dropped or duplicated block, which is exactly the failure being looked for.

use std::f64::consts::PI;
use std::path::{Path, PathBuf};

use mp_audio::decode::TrackDecoder;
use mp_audio::resample::Resampler;

const RATE: u32 = 44_100;
const CHANNELS: usize = 2;
const FREQ: f64 = 440.0;

/// Where the sine is cut, in frames. Deliberately not a zero crossing.
const SPLIT_AT: usize = 11_000;
const TOTAL_FRAMES: usize = 22_050;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("resonance-gapless-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The reference waveform: one continuous sine.
fn reference() -> Vec<f64> {
    (0..TOTAL_FRAMES)
        .map(|n| (2.0 * PI * FREQ * n as f64 / f64::from(RATE)).sin() * 0.5)
        .collect()
}

/// Write 16-bit PCM frames as a WAV file.
///
/// Hand-rolled rather than pulled from a crate: the header is forty-four bytes,
/// and a test fixture generator is not worth a dependency.
fn write_wav(path: &Path, samples: &[f64]) {
    let data_bytes = (samples.len() * CHANNELS * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_bytes as usize);

    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&(CHANNELS as u16).to_le_bytes());
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * CHANNELS as u32 * 2).to_le_bytes()); // byte rate
    out.extend_from_slice(&((CHANNELS * 2) as u16).to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_bytes.to_le_bytes());

    for value in samples {
        let scaled = (value.clamp(-1.0, 1.0) * f64::from(i16::MAX)).round() as i16;
        for _ in 0..CHANNELS {
            out.extend_from_slice(&scaled.to_le_bytes());
        }
    }

    std::fs::write(path, out).unwrap();
}

/// Decode a file all the way through, returning interleaved frames of one
/// channel at the device rate.
fn decode_through_resampler(path: &Path, device_rate: u32) -> Vec<f32> {
    let mut decoder = TrackDecoder::open(path).expect("the fixture should open");
    let mut resampler = Resampler::new(decoder.sample_rate(), device_rate, CHANNELS)
        .expect("the resampler should build");

    let mut out = Vec::new();

    loop {
        // Drain whatever the resampler already holds.
        while let Some(block) = resampler.pull() {
            out.extend(block.chunks(CHANNELS).map(|frame| frame[0]));
        }

        match decoder.next_chunk() {
            Ok(Some(chunk)) => {
                resampler.push(&chunk.planes, chunk.frames);
                decoder.recycle(chunk);
            }
            Ok(None) => break,
            Err(err) => panic!("decoding {} failed: {err}", path.display()),
        }
    }

    while let Some(block) = resampler.pull() {
        out.extend(block.chunks(CHANNELS).map(|frame| frame[0]));
    }
    if let Some(tail) = resampler.drain() {
        out.extend(tail.chunks(CHANNELS).map(|frame| frame[0]));
    }

    out
}

/// The largest jump between consecutive samples.
fn largest_step(samples: &[f32]) -> f32 {
    samples
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).abs())
        .fold(0.0_f32, f32::max)
}

#[test]
fn two_halves_of_a_split_track_rejoin_without_a_discontinuity() {
    let dir = scratch("split");
    let reference = reference();

    let first = dir.join("half-a.wav");
    let second = dir.join("half-b.wav");
    write_wav(&first, &reference[..SPLIT_AT]);
    write_wav(&second, &reference[SPLIT_AT..]);

    // Played at the file's own rate, so no resampling is in the way.
    let a = decode_through_resampler(&first, RATE);
    let b = decode_through_resampler(&second, RATE);

    assert!(
        !a.is_empty() && !b.is_empty(),
        "the fixtures decoded to nothing"
    );

    // Joined the way gapless joins them: end of one, straight into the next.
    let joined: Vec<f32> = a.iter().chain(b.iter()).copied().collect();

    // The reference sine's own steepest step, for comparison.
    let smooth: Vec<f32> = reference.iter().map(|v| *v as f32).collect();
    let natural = largest_step(&smooth);

    // Look only at the neighbourhood of the seam.
    let seam = a.len();
    let window = &joined[seam.saturating_sub(64)..(seam + 64).min(joined.len())];
    let at_seam = largest_step(window);

    assert!(
        at_seam < natural * 1.5,
        "the join stepped by {at_seam:.5}, against a natural {natural:.5} for this \
         waveform — that is a click"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The reconstruction has to be the *same* audio, not merely smooth: dropping
/// or duplicating a block could still look continuous.
#[test]
fn the_rejoined_halves_reproduce_the_original_waveform() {
    let dir = scratch("reconstruct");
    let reference = reference();

    let first = dir.join("half-a.wav");
    let second = dir.join("half-b.wav");
    write_wav(&first, &reference[..SPLIT_AT]);
    write_wav(&second, &reference[SPLIT_AT..]);

    let mut joined = decode_through_resampler(&first, RATE);
    joined.extend(decode_through_resampler(&second, RATE));

    assert_eq!(
        joined.len(),
        reference.len(),
        "the two halves should reconstruct exactly the original length"
    );

    // 16-bit quantisation is the only difference that should exist.
    let tolerance = 2.0 / f32::from(i16::MAX);
    let worst = joined
        .iter()
        .zip(reference.iter())
        .map(|(got, want)| (got - *want as f32).abs())
        .fold(0.0_f32, f32::max);

    assert!(
        worst <= tolerance,
        "the rejoined audio differs from the original by up to {worst}, \
         beyond the {tolerance} that quantisation explains"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The seam has to survive resampling too — this collection plays at 96 kHz
/// from 44.1 kHz sources, so the boundary is never on a whole-sample tick.
#[test]
fn the_seam_survives_sample_rate_conversion() {
    let dir = scratch("resampled");
    let reference = reference();

    let first = dir.join("half-a.wav");
    let second = dir.join("half-b.wav");
    write_wav(&first, &reference[..SPLIT_AT]);
    write_wav(&second, &reference[SPLIT_AT..]);

    let device_rate = 96_000;
    let a = decode_through_resampler(&first, device_rate);
    let b = decode_through_resampler(&second, device_rate);

    let joined: Vec<f32> = a.iter().chain(b.iter()).copied().collect();

    // At 96 kHz the same 440 Hz sine moves less per sample, so its natural step
    // is smaller and a click would stand out more.
    let natural = 2.0 * std::f32::consts::PI * FREQ as f32 / device_rate as f32 * 0.5;

    let seam = a.len();
    let window = &joined[seam.saturating_sub(128)..(seam + 128).min(joined.len())];
    let at_seam = largest_step(window);

    assert!(
        at_seam < natural * 3.0,
        "after resampling, the join stepped by {at_seam:.5} against a natural \
         {natural:.5}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
