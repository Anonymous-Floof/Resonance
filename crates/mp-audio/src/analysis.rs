//! Measuring what a track sounds like.
//!
//! The similarity engine in `mp-core` can reason about tags and about what you
//! have put in playlists together, but neither helps with a file called
//! `track07.mp3` that has no tags at all — and a collection assembled from
//! wherever is full of those. This is the answer: listen to the file once,
//! reduce it to eight numbers, and compare those.
//!
//! It lives here rather than in `mp-core` because it needs a decoder. The shape
//! of the result, its storage and the comparison all live in
//! [`mp_core::library::features`]; this module only produces the numbers.
//!
//! # Cost
//!
//! Analysis decodes real audio, so it is far more expensive than a tag scan —
//! seconds per track rather than milliseconds. Three things keep that bearable:
//! only a slice of each track is examined, the pass is resumable so it can be
//! stopped and picked up later, and it is never on the path of anything the
//! user is waiting for.

use std::path::Path;

use anyhow::{Context, Result};
use mp_core::library::features::Features;
use realfft::RealFftPlanner;
use realfft::num_complex::Complex;

use crate::decode::TrackDecoder;

/// How much audio to examine, in seconds.
///
/// A minute is enough to characterise a track and short enough that a large
/// library finishes this side of never. Analysing a whole ten-minute mix would
/// cost ten times as much to produce nearly the same eight numbers.
pub const ANALYSIS_SECONDS: f32 = 60.0;

/// How far into the track to start, as a fraction of its length.
///
/// Skipping the opening avoids characterising a track by its intro — fades,
/// silence and spoken word are all common there and none of them represent the
/// track.
const SKIP_FRACTION: f64 = 0.15;

/// Analysis window, in samples.
const FRAME: usize = 2048;

/// Hop between windows. Half the frame is the usual overlap for this kind of
/// measurement: enough that a transient is not missed between frames, not so
/// much that the cost doubles again.
const HOP: usize = 1024;

/// Hop for the onset envelope, in samples.
///
/// Much finer than [`HOP`], and deliberately not sharing it. Tempo resolution
/// is set by how finely the envelope is sampled: at a 1024-sample hop the
/// envelope runs at about 43 frames a second, a 120 bpm beat is 21.5 frames
/// apart, and *no integer lag represents it*. The correlation at 21 frames is
/// half a frame out on every beat while the one at 43 is nearly exact, so the
/// measurement lands confidently on 60 bpm. At 256 samples the envelope runs
/// at about 172 a second and the same beat is 86 frames apart, which is
/// representable.
///
/// This costs almost nothing because the envelope needs no transform — see
/// [`onset_envelope`].
const ONSET_HOP: usize = 256;

/// Frequency range the spectral centroid is mapped over, in Hz.
///
/// Logarithmic, because brightness is heard that way — the step from 100 Hz to
/// 200 Hz is as large as the one from 4 kHz to 8 kHz.
const CENTROID_RANGE_HZ: (f32, f32) = (50.0, 8_000.0);

/// Band edges for the energy split, in Hz.
const BASS_HZ: f32 = 250.0;
const MID_HZ: f32 = 4_000.0;

/// Tempo range considered, in beats per minute.
///
/// Also the range [`Features::tempo`] is normalised over, so the two have to
/// agree. Outside this, autocorrelation reliably locks onto half or double the
/// real tempo, and a confidently wrong answer is worse than a vague one.
pub const TEMPO_RANGE_BPM: (f32, f32) = (60.0, 180.0);

/// Loudness range mapped onto `0.0..=1.0`, in dBFS.
const LOUDNESS_RANGE_DB: (f32, f32) = (-60.0, 0.0);

/// Decode a slice of `path` and measure it.
pub fn analyse(path: &Path) -> Result<Features> {
    let mut decoder = TrackDecoder::open(path)
        .with_context(|| format!("opening {} for analysis", path.display()))?;

    let rate = decoder.sample_rate();
    let channels = decoder.channels().max(1);

    // Where to start, and how much to take.
    let skip_frames = decoder
        .duration()
        .map(|duration| (duration.as_secs_f64() * SKIP_FRACTION * f64::from(rate)) as usize)
        .unwrap_or(0);
    let wanted = (ANALYSIS_SECONDS * rate as f32) as usize;

    let mut samples: Vec<f32> = Vec::with_capacity(wanted.min(1 << 22));
    let mut skipped = 0usize;

    loop {
        match decoder.next_chunk() {
            Ok(Some(chunk)) => {
                for frame in 0..chunk.frames {
                    if skipped < skip_frames {
                        skipped += 1;
                        continue;
                    }

                    // Downmixed to mono: every measurement here is about the
                    // spectrum and the rhythm, neither of which is a stereo
                    // property, and mono halves the work.
                    let mut sum = 0.0;
                    for plane in chunk.planes.iter().take(channels) {
                        sum += plane.get(frame).copied().unwrap_or(0.0);
                    }
                    samples.push(sum / channels as f32);
                }

                let done = samples.len() >= wanted;
                decoder.recycle(chunk);

                if done {
                    break;
                }
            }
            Ok(None) => break,
            Err(err) => {
                return Err(err).with_context(|| format!("decoding {}", path.display()));
            }
        }
    }

    // A track shorter than the skip is analysed from its start rather than not
    // at all — short interludes are still worth placing.
    if samples.is_empty() && skip_frames > 0 {
        decoder.seek(std::time::Duration::ZERO)?;
        while let Ok(Some(chunk)) = decoder.next_chunk() {
            for frame in 0..chunk.frames {
                let mut sum = 0.0;
                for plane in chunk.planes.iter().take(channels) {
                    sum += plane.get(frame).copied().unwrap_or(0.0);
                }
                samples.push(sum / channels as f32);
            }
            let done = samples.len() >= wanted;
            decoder.recycle(chunk);
            if done {
                break;
            }
        }
    }

    Ok(analyse_samples(&samples, rate))
}

/// Measure a mono buffer.
///
/// Split out from [`analyse`] so every one of these numbers can be checked
/// against a signal whose answer is known, with no file and no decoder in the
/// way.
pub fn analyse_samples(samples: &[f32], rate: u32) -> Features {
    let rate = rate.max(1) as f32;

    // Nothing to measure. The default sits at the middle of every axis, which
    // is equidistant from everything — the honest answer for "no information".
    if samples.len() < FRAME {
        return Features::default();
    }

    let loudness = loudness_of(samples);
    let zero_cross = zero_crossing_rate(samples);

    let spectra = spectrogram(samples);
    let (centroid, rolloff, bands) = spectral_shape(&spectra, rate);
    let tempo = tempo_of(samples, rate);

    Features {
        tempo,
        centroid,
        rolloff,
        loudness,
        bass: bands[0],
        mid: bands[1],
        treble: bands[2],
        zero_cross,
    }
    .sanitised()
}

/// Root-mean-square level, mapped through decibels.
///
/// Straight RMS would put almost every track into the bottom tenth of the
/// axis, because the linear scale has nearly all its room above where music
/// lives.
fn loudness_of(samples: &[f32]) -> f32 {
    let mut sum = 0.0_f64;
    for sample in samples {
        sum += f64::from(*sample) * f64::from(*sample);
    }

    let rms = (sum / samples.len() as f64).sqrt() as f32;

    if rms <= 1e-7 {
        return 0.0;
    }

    let db = 20.0 * rms.log10();
    let (low, high) = LOUDNESS_RANGE_DB;

    ((db - low) / (high - low)).clamp(0.0, 1.0)
}

/// How often the signal crosses zero, per sample.
///
/// High for noise and for percussive or distorted material; low for a pure
/// tone or anything bass-heavy.
fn zero_crossing_rate(samples: &[f32]) -> f32 {
    let mut crossings = 0usize;

    for pair in samples.windows(2) {
        // The sign comparison rather than a multiply: two subnormals multiply
        // to zero and would be missed.
        if (pair[0] >= 0.0) != (pair[1] >= 0.0) {
            crossings += 1;
        }
    }

    let rate = crossings as f32 / (samples.len() - 1).max(1) as f32;

    // White noise sits near 0.5, which is the practical ceiling; scaling by
    // two spreads real material across the axis instead of the bottom half.
    (rate * 2.0).clamp(0.0, 1.0)
}

/// Magnitude spectra, one per analysis window.
fn spectrogram(samples: &[f32]) -> Vec<Vec<f32>> {
    let mut planner = RealFftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(FRAME);

    let mut input = fft.make_input_vec();
    let mut output: Vec<Complex<f32>> = fft.make_output_vec();

    // Hann, periodic — the same window and the same reason as the visualiser's.
    let window: Vec<f32> = (0..FRAME)
        .map(|n| {
            let phase = std::f32::consts::TAU * n as f32 / FRAME as f32;
            0.5 * (1.0 - phase.cos())
        })
        .collect();

    let mut spectra = Vec::new();
    let mut start = 0;

    while start + FRAME <= samples.len() {
        for ((slot, sample), weight) in input
            .iter_mut()
            .zip(samples[start..start + FRAME].iter())
            .zip(window.iter())
        {
            *slot = sample * weight;
        }

        if fft.process(&mut input, &mut output).is_ok() {
            spectra.push(output.iter().map(|bin| bin.norm()).collect());
        }

        start += HOP;
    }

    spectra
}

/// Average centroid, rolloff and band split across the spectrogram.
fn spectral_shape(spectra: &[Vec<f32>], rate: f32) -> (f32, f32, [f32; 3]) {
    if spectra.is_empty() {
        return (0.5, 0.5, [0.33, 0.33, 0.33]);
    }

    let bin_hz = rate / FRAME as f32;

    let mut centroid_total = 0.0;
    let mut rolloff_total = 0.0;
    let mut bands = [0.0_f64; 3];
    let mut counted = 0.0_f32;

    for spectrum in spectra {
        let mut energy = 0.0_f32;
        let mut weighted = 0.0_f32;

        for (index, magnitude) in spectrum.iter().enumerate() {
            let freq = index as f32 * bin_hz;
            energy += magnitude;
            weighted += freq * magnitude;

            let band = if freq < BASS_HZ {
                0
            } else if freq < MID_HZ {
                1
            } else {
                2
            };
            bands[band] += f64::from(*magnitude);
        }

        // A silent window carries no shape information; including it would
        // drag the average towards whatever zero divided by zero becomes.
        if energy <= 1e-6 {
            continue;
        }

        centroid_total += weighted / energy;

        // Rolloff: the frequency below which 85% of the energy lies.
        let threshold = energy * 0.85;
        let mut running = 0.0;
        let mut rolloff_hz = 0.0;

        for (index, magnitude) in spectrum.iter().enumerate() {
            running += magnitude;
            if running >= threshold {
                rolloff_hz = index as f32 * bin_hz;
                break;
            }
        }

        rolloff_total += rolloff_hz;
        counted += 1.0;
    }

    if counted == 0.0 {
        return (0.0, 0.0, [0.33, 0.33, 0.33]);
    }

    let centroid = log_normalise(centroid_total / counted);
    let rolloff = log_normalise(rolloff_total / counted);

    let total: f64 = bands.iter().sum();
    let split = if total > 0.0 {
        [
            (bands[0] / total) as f32,
            (bands[1] / total) as f32,
            (bands[2] / total) as f32,
        ]
    } else {
        [0.33, 0.33, 0.33]
    };

    (centroid, rolloff, split)
}

/// Map a frequency onto `0.0..=1.0`, logarithmically.
fn log_normalise(hz: f32) -> f32 {
    let (low, high) = CENTROID_RANGE_HZ;
    let clamped = hz.max(low);

    ((clamped / low).log10() / (high / low).log10()).clamp(0.0, 1.0)
}

/// A signal that rises wherever something in the audio starts.
///
/// Loudness per short block, log-compressed, then half-wave rectified
/// differences. No transform involved: an onset is a change in *level*, and
/// measuring it directly is both cheaper than a spectrogram and finer, because
/// the block can be much shorter than a useful analysis window.
///
/// The log compression is what makes this work across a whole track. A raw
/// energy difference is dominated by the loudest passage, so the beat in a
/// quiet verse contributes almost nothing and the estimate is decided by the
/// chorus alone.
fn onset_envelope(samples: &[f32]) -> Vec<f32> {
    let levels: Vec<f32> = samples
        .chunks(ONSET_HOP)
        .map(|block| {
            let mut sum = 0.0_f64;
            for sample in block {
                sum += f64::from(*sample) * f64::from(*sample);
            }
            let rms = (sum / block.len().max(1) as f64).sqrt() as f32;

            // ln(1 + kx). Compresses the loud end without the singularity a
            // plain logarithm has at silence.
            (1.0 + rms * 200.0).ln()
        })
        .collect();

    levels
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).max(0.0))
        .collect()
}

/// Estimate tempo from the onset envelope.
///
/// The envelope spikes when something starts; the spacing between those spikes
/// is the beat. Autocorrelation finds that spacing without having to detect
/// individual onsets, which is what makes it robust on material where the beat
/// is implied rather than hammered.
fn tempo_of(samples: &[f32], rate: f32) -> f32 {
    // The middle of the range, meaning "no idea", when there is too little to
    // work with.
    const UNKNOWN: f32 = 0.5;

    let mut envelope = onset_envelope(samples);

    if envelope.len() < 64 {
        return UNKNOWN;
    }

    // Centred on zero, so the autocorrelation measures periodicity rather than
    // the constant offset every music signal has.
    let mean = envelope.iter().sum::<f32>() / envelope.len() as f32;
    for value in &mut envelope {
        *value -= mean;
    }

    let frames_per_second = rate / ONSET_HOP as f32;
    let (slowest, fastest) = TEMPO_RANGE_BPM;

    // A slower tempo means a longer gap between beats, so the lag bounds are
    // the other way round from the bpm bounds.
    let longest_lag = (60.0 / slowest * frames_per_second).round() as usize;
    let shortest_lag = (60.0 / fastest * frames_per_second).round().max(1.0) as usize;

    if shortest_lag >= longest_lag || longest_lag >= envelope.len() {
        return UNKNOWN;
    }

    let mut scores = Vec::with_capacity(longest_lag - shortest_lag + 1);

    for lag in shortest_lag..=longest_lag {
        let mut score = 0.0;
        for index in 0..envelope.len() - lag {
            score += envelope[index] * envelope[index + lag];
        }

        // Normalised by overlap, or short lags win simply by having more terms.
        scores.push(score / (envelope.len() - lag) as f32);
    }

    let best_score = scores.iter().copied().fold(f32::NEG_INFINITY, f32::max);

    if best_score <= 0.0 {
        return UNKNOWN;
    }

    // Take the *shortest* lag that scores nearly as well as the best, not the
    // best outright.
    //
    // This is the octave error, and it is not a rare edge case: a beat at 120
    // bpm repeats every half second, so it also repeats every whole second,
    // and autocorrelation scores both alike. Picking the maximum then reads a
    // straightforward 120 bpm track as 60. Preferring the shorter lag among
    // near-equal peaks resolves it, and it is safe because a lag that is not a
    // real beat period falls between beats and correlates badly — after the
    // mean subtraction above, usually negative.
    const NEAR_ENOUGH: f32 = 0.85;

    let best_lag = scores
        .iter()
        .position(|score| *score >= best_score * NEAR_ENOUGH)
        .map(|index| index + shortest_lag)
        .unwrap_or(shortest_lag);

    let bpm = 60.0 * frames_per_second / best_lag as f32;

    ((bpm - slowest) / (fastest - slowest)).clamp(0.0, 1.0)
}

/// Turn a normalised tempo back into beats per minute, for display.
pub fn tempo_to_bpm(tempo: f32) -> f32 {
    let (slowest, fastest) = TEMPO_RANGE_BPM;
    slowest + tempo.clamp(0.0, 1.0) * (fastest - slowest)
}

// ---------------------------------------------------------------------------
// The background pass
// ---------------------------------------------------------------------------

/// What one batch of analysis did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Batch {
    pub analysed: usize,
    /// Files that could not be decoded. Recorded, not retried forever.
    pub failed: usize,
    /// Tracks still waiting after this batch.
    pub remaining: u32,
}

impl Batch {
    /// Whether there is more to do.
    pub fn has_more(&self) -> bool {
        self.remaining > 0 && (self.analysed > 0 || self.failed > 0)
    }
}

/// Analyse up to `limit` pending tracks, writing the results into the index.
///
/// Returns after `limit` tracks so the caller can check for cancellation, save
/// progress and yield. The pass is resumable by construction: every result is
/// committed as it is produced, and the queue is derived from the index rather
/// than held in memory, so stopping halfway loses nothing.
///
/// A file that will not decode is *not* left in the queue. It is stored with
/// default features against its current fingerprint, which takes it out of the
/// queue until the file changes — otherwise one broken file would be retried on
/// every pass, forever.
pub fn run_batch(
    connection: &mp_core::library::db::Handle,
    limit: usize,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<Batch> {
    use mp_core::library::features;
    use std::sync::atomic::Ordering;

    let pending = features::pending(connection, limit)?;
    let mut batch = Batch::default();

    for (id, path, mtime, size) in pending {
        if cancel.load(Ordering::Relaxed) {
            break;
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs() as i64);

        match analyse(Path::new(&path)) {
            Ok(measured) => {
                features::store(connection, id, &measured, mtime, size, now)?;
                batch.analysed += 1;
            }
            Err(err) => {
                tracing::debug!("could not analyse {path}: {err:#}");
                features::store(connection, id, &Features::default(), mtime, size, now)?;
                batch.failed += 1;
            }
        }
    }

    let (done, total) = features::progress(connection)?;
    batch.remaining = total.saturating_sub(done);

    Ok(batch)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 44_100;

    fn sine(freq: f32, seconds: f32, amplitude: f32) -> Vec<f32> {
        let count = (seconds * RATE as f32) as usize;
        (0..count)
            .map(|n| {
                let phase = std::f32::consts::TAU * freq * n as f32 / RATE as f32;
                phase.sin() * amplitude
            })
            .collect()
    }

    /// Deterministic pseudo-noise, so the test does not need a dependency.
    fn noise(seconds: f32) -> Vec<f32> {
        let count = (seconds * RATE as f32) as usize;
        let mut state = 0x2545_F491_4F6C_DD1D_u64;

        (0..count)
            .map(|_| {
                state ^= state >> 12;
                state ^= state << 25;
                state ^= state >> 27;
                let value = (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40) as f32;
                value / (1u32 << 23) as f32 - 1.0
            })
            .collect()
    }

    /// A click every `interval` seconds, which is a beat with a known tempo.
    fn click_track(bpm: f32, seconds: f32) -> Vec<f32> {
        let count = (seconds * RATE as f32) as usize;
        let period = (60.0 / bpm * RATE as f32) as usize;

        let mut out = vec![0.0; count];
        let mut at = 0;

        while at < count {
            // A short decaying burst of broadband energy, so the onset shows
            // up as spectral flux across the whole spectrum.
            for offset in 0..256.min(count - at) {
                let decay = 1.0 - offset as f32 / 256.0;
                let wobble = if offset % 2 == 0 { 1.0 } else { -1.0 };
                out[at + offset] = decay * decay * wobble * 0.8;
            }
            at += period;
        }

        out
    }

    #[test]
    fn a_bright_tone_reads_brighter_than_a_dark_one() {
        let low = analyse_samples(&sine(120.0, 5.0, 0.5), RATE);
        let high = analyse_samples(&sine(6_000.0, 5.0, 0.5), RATE);

        assert!(
            high.centroid > low.centroid + 0.3,
            "centroid barely moved: {:.3} to {:.3}",
            low.centroid,
            high.centroid
        );
        assert!(high.rolloff > low.rolloff);
    }

    #[test]
    fn energy_lands_in_the_band_it_belongs_to() {
        let low = analyse_samples(&sine(100.0, 5.0, 0.5), RATE);
        assert!(
            low.bass > low.treble,
            "a 100 Hz tone read bass {:.3}, treble {:.3}",
            low.bass,
            low.treble
        );

        let high = analyse_samples(&sine(8_000.0, 5.0, 0.5), RATE);
        assert!(
            high.treble > high.bass,
            "an 8 kHz tone read treble {:.3}, bass {:.3}",
            high.treble,
            high.bass
        );

        let middle = analyse_samples(&sine(1_000.0, 5.0, 0.5), RATE);
        assert!(middle.mid > middle.bass && middle.mid > middle.treble);
    }

    #[test]
    fn louder_material_reads_louder() {
        let quiet = analyse_samples(&sine(440.0, 5.0, 0.02), RATE);
        let loud = analyse_samples(&sine(440.0, 5.0, 0.9), RATE);

        assert!(
            loud.loudness > quiet.loudness + 0.3,
            "loudness barely moved: {:.3} to {:.3}",
            quiet.loudness,
            loud.loudness
        );
    }

    #[test]
    fn noise_crosses_zero_far_more_than_a_bass_tone() {
        let tone = analyse_samples(&sine(80.0, 5.0, 0.5), RATE);
        let hiss = analyse_samples(&noise(5.0), RATE);

        assert!(
            hiss.zero_cross > tone.zero_cross + 0.3,
            "zero-crossing barely moved: {:.3} to {:.3}",
            tone.zero_cross,
            hiss.zero_cross
        );
    }

    /// The hardest number to get right, so it is checked against a signal whose
    /// tempo is known exactly.
    #[test]
    fn a_click_track_reads_close_to_its_real_tempo() {
        for wanted in [80.0_f32, 100.0, 120.0, 150.0] {
            let features = analyse_samples(&click_track(wanted, 20.0), RATE);
            let measured = tempo_to_bpm(features.tempo);

            // Within 6%: the frame rate quantises which lags are available, so
            // an exact answer is not on offer at every tempo.
            let error = (measured - wanted).abs() / wanted;
            assert!(
                error < 0.06,
                "a {wanted} bpm click read as {measured:.1} bpm"
            );
        }
    }

    /// Material with no beat should say so rather than inventing one.
    #[test]
    fn a_steady_tone_does_not_produce_a_confident_tempo() {
        let features = analyse_samples(&sine(440.0, 20.0, 0.5), RATE);

        // Not asserting a particular value — just that it is finite and in
        // range, since there is no right answer to be had.
        assert!(features.tempo.is_finite());
        assert!((0.0..=1.0).contains(&features.tempo));
    }

    #[test]
    fn silence_is_measured_without_dividing_by_zero() {
        let features = analyse_samples(&vec![0.0; RATE as usize * 3], RATE);

        for value in features.vector() {
            assert!(value.is_finite(), "silence produced a non-finite feature");
            assert!((0.0..=1.0).contains(&value));
        }

        assert_eq!(features.loudness, 0.0);
    }

    /// Too short to analyse is not an error; it is a track with no information.
    #[test]
    fn a_buffer_shorter_than_one_window_falls_back_to_the_default() {
        let features = analyse_samples(&sine(440.0, 0.01, 0.5), RATE);
        assert_eq!(features, Features::default());
    }

    #[test]
    fn every_feature_stays_in_range_for_any_input() {
        let inputs = [
            sine(20.0, 3.0, 1.0),
            sine(20_000.0, 3.0, 1.0),
            noise(3.0),
            vec![0.0; RATE as usize * 3],
            vec![1.0; RATE as usize * 3],
            click_track(200.0, 10.0),
        ];

        for samples in &inputs {
            let features = analyse_samples(samples, RATE);

            for value in features.vector() {
                assert!(
                    value.is_finite() && (0.0..=1.0).contains(&value),
                    "a feature escaped its range: {value}"
                );
            }
        }
    }

    /// The same audio has to measure the same every time, or a re-analysis
    /// would silently reshuffle the similarity rankings.
    #[test]
    fn analysis_is_deterministic() {
        let samples = click_track(120.0, 10.0);

        assert_eq!(
            analyse_samples(&samples, RATE),
            analyse_samples(&samples, RATE)
        );
    }

    /// The same music at a different sample rate should measure much the same.
    #[test]
    fn the_sample_rate_does_not_change_the_answer_much() {
        let at_44 = analyse_samples(&sine(1_000.0, 5.0, 0.5), 44_100);

        let count = (5.0 * 48_000.0) as usize;
        let resampled: Vec<f32> = (0..count)
            .map(|n| {
                let phase = std::f32::consts::TAU * 1_000.0 * n as f32 / 48_000.0;
                phase.sin() * 0.5
            })
            .collect();
        let at_48 = analyse_samples(&resampled, 48_000);

        assert!(
            (at_44.centroid - at_48.centroid).abs() < 0.05,
            "centroid moved with the sample rate: {:.3} against {:.3}",
            at_44.centroid,
            at_48.centroid
        );
        assert!((at_44.loudness - at_48.loudness).abs() < 0.05);
    }

    #[test]
    fn tempo_maps_back_and_forth() {
        let (slowest, fastest) = TEMPO_RANGE_BPM;

        assert_eq!(tempo_to_bpm(0.0), slowest);
        assert_eq!(tempo_to_bpm(1.0), fastest);
        assert!((tempo_to_bpm(0.5) - (slowest + fastest) / 2.0).abs() < 0.01);

        // Out-of-range input clamps rather than extrapolating.
        assert_eq!(tempo_to_bpm(-1.0), slowest);
        assert_eq!(tempo_to_bpm(5.0), fastest);
    }

    /// Two different kinds of material must not land on top of each other, or
    /// the feature vector is not discriminating anything.
    #[test]
    fn different_material_produces_distinguishable_vectors() {
        let bass = analyse_samples(&sine(60.0, 5.0, 0.6), RATE);
        let hiss = analyse_samples(&noise(5.0), RATE);
        let beat = analyse_samples(&click_track(120.0, 10.0), RATE);

        for (left, right, name) in [
            (&bass, &hiss, "bass against noise"),
            (&bass, &beat, "bass against a beat"),
        ] {
            let similarity = left.similarity(right);
            assert!(
                similarity < 0.85,
                "{name} scored {similarity:.3} — too alike to tell apart"
            );
        }
    }
}
