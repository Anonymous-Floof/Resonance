//! Turning a window of samples into something worth drawing.
//!
//! This lives in `mp-audio` rather than in the UI for two reasons. It is pure
//! signal processing — an FFT, a windowing function and some smoothing — with
//! no notion of pixels, so it belongs with the rest of the DSP. And it is the
//! part most likely to be subtly wrong, so it wants tests that assert a 1 kHz
//! sine lands in the 1 kHz bar rather than a screenshot that looks plausible.
//!
//! It runs on the UI thread, not in the callback, so it is free to allocate and
//! to call `log10`.
//!
//! # What the numbers mean
//!
//! Everything in [`Frame`] is normalised to `0.0..=1.0` except [`Frame::wave`],
//! which stays in signal units so the oscilloscope can show clipping. A bar at
//! `1.0` is a full-scale sine at that frequency; `0.0` is at or below the noise
//! floor.

use realfft::RealFftPlanner;
use realfft::num_complex::Complex;

/// Samples per analysis window at ordinary sample rates.
///
/// 2048 at 44.1 kHz is 21.5 Hz per bin and about 46 ms of audio — fine enough
/// to separate the low bars, short enough that the display still feels
/// connected to what is playing. A longer window would resolve bass better and
/// make the whole thing feel laggy.
pub const BASE_FFT_SIZE: usize = 2048;

/// The window length to use at a given device rate.
///
/// What matters is the window's *duration*, not its sample count: frequency
/// resolution is one over the former. Holding the sample count fixed halves
/// the resolution every time the rate doubles, and on a 96 kHz device that put
/// the bottom twenty-odd bars all inside a single 47 Hz bin — so the whole
/// bass end moved as one block, and a 110 Hz tone peaked two bars away from
/// where it belonged. Scaling with the rate holds the window at roughly 45 ms
/// and the resolution at roughly 23 Hz wherever it runs.
pub const fn fft_size_for(sample_rate: u32) -> usize {
    match sample_rate {
        0..=56_000 => BASE_FFT_SIZE,
        56_001..=112_000 => BASE_FFT_SIZE * 2,
        _ => BASE_FFT_SIZE * 4,
    }
}

/// How long the stream may go quiet before the display starts settling.
///
/// Arrivals come in device-buffer-sized bursts, so a UI frame landing between
/// two of them legitimately sees nothing. This has to be comfortably longer
/// than any plausible buffer, and short enough that a pause settles promptly.
const STARVE_GRACE_SECS: f32 = 0.05;

/// Samples the oscilloscope draws.
pub const WAVE_SAMPLES: usize = 1024;

/// How far back the trigger may look for a crossing.
///
/// One window plus one display's worth: enough to find a crossing even for a
/// low-frequency waveform, without reaching so far back that the oscilloscope
/// shows visibly old audio.
const TRIGGER_SEARCH: usize = WAVE_SAMPLES * 2;

/// Lowest frequency given a bar. Below this is mostly rumble and DC offset.
pub const MIN_HZ: f32 = 30.0;

/// Highest frequency given a bar.
pub const MAX_HZ: f32 = 16_000.0;

/// Level treated as silence, in dB relative to full scale.
///
/// -72 dB is roughly 12-bit noise. Going lower mostly buys a display that
/// twitches during quiet passages.
const FLOOR_DB: f32 = -72.0;

/// Band edges for the three-way energy split, in Hz.
const BASS_HZ: (f32, f32) = (20.0, 250.0);
const MID_HZ: (f32, f32) = (250.0, 4_000.0);
const TREBLE_HZ: (f32, f32) = (4_000.0, 16_000.0);

/// Level treated as an empty band, in dB.
///
/// Higher than [`FLOOR_DB`] on purpose. The bars want a deep floor so quiet
/// detail still shows; the band energies drive brightness, and a 72 dB range
/// squeezed into `0.0..=1.0` leaves ordinary music sitting in a narrow slice
/// near the top, where a quiet track and a loud one look the same.
const BAND_FLOOR_DB: f32 = -60.0;

pub const MIN_BARS: usize = 8;
pub const MAX_BARS: usize = 256;

/// One analysed moment, ready to draw.
#[derive(Debug, Clone, Default)]
pub struct Frame {
    /// Smoothed magnitudes, one per bar, low frequency first. `0.0..=1.0`.
    pub bars: Vec<f32>,
    /// Falling peak-hold caps, one per bar. `0.0..=1.0`.
    pub peaks: Vec<f32>,
    /// A trigger-aligned window of the waveform, in signal units.
    pub wave: Vec<f32>,
    /// Overall loudness of the window, `0.0..=1.0`.
    pub rms: f32,
    /// Largest absolute sample in the window, in signal units.
    pub peak: f32,
    /// Smoothed low/mid/high energy, `0.0..=1.0` each.
    pub bass: f32,
    pub mid: f32,
    pub treble: f32,
    /// Beat strength, `0.0..=1.0`, decaying after each onset.
    pub onset: f32,
    /// Whether there is audio to look at. False during silence and when
    /// nothing is playing, so visualisers can show a resting state instead of
    /// a flat line that looks broken.
    pub active: bool,
}

impl Frame {
    /// The bar and its cap at `index`, or zeros when out of range.
    pub fn bar(&self, index: usize) -> (f32, f32) {
        (
            self.bars.get(index).copied().unwrap_or(0.0),
            self.peaks.get(index).copied().unwrap_or(0.0),
        )
    }
}

/// The range of FFT bins that feed one bar.
#[derive(Debug, Clone, Copy)]
struct BarRange {
    lo: usize,
    hi: usize,
    /// The bar's centre frequency, expressed in bins.
    ///
    /// Fractional on purpose: it is where the bar's value is *sampled* when the
    /// bar turns out to be narrower than a bin.
    centre: f32,
    /// Whether this bar is narrower than one bin.
    ///
    /// Not derivable from `lo` and `hi`: those are the *clamped* range, and a
    /// bar half a bin wide that happens to straddle a boundary still spans two
    /// of them. Comparing the widths directly is the question actually being
    /// asked — can the transform resolve this bar at all.
    narrow: bool,
}

pub struct Analyzer {
    sample_rate: f32,
    /// Window length in samples, chosen for the current rate.
    fft_size: usize,

    fft: std::sync::Arc<dyn realfft::RealToComplex<f32>>,
    /// Windowed input. Reused; `process` scribbles on it.
    input: Vec<f32>,
    output: Vec<Complex<f32>>,
    /// Precomputed Hann window.
    window: Vec<f32>,

    /// Raw samples pulled from the monitor, before windowing.
    samples: Vec<f32>,
    /// Per-bin amplitude in dB, before binning.
    bins_db: Vec<f32>,

    /// Which bins feed which bar. Rebuilt when the bar count or rate changes.
    ranges: Vec<BarRange>,
    /// Unsmoothed bar levels this frame, kept for the flux calculation.
    raw: Vec<f32>,
    /// Falling-cap velocity, one per bar.
    cap_velocity: Vec<f32>,

    /// Total power in the bass, mid and treble ranges, from the last
    /// transform. Accumulated there, where the linear amplitudes still exist.
    band_power: [f32; 3],

    /// Rolling baseline for onset detection.
    flux_baseline: f32,

    /// How long the feed has delivered nothing.
    starved: f32,

    frame: Frame,
}

impl Analyzer {
    pub fn new(sample_rate: u32) -> Self {
        let rate = sample_rate.max(1);
        let mut planner = RealFftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(fft_size_for(rate));

        let mut analyzer = Self {
            sample_rate: rate as f32,
            // Zero so `rebuild_fft` cannot mistake this for already sized.
            fft_size: 0,
            fft,
            input: Vec::new(),
            output: Vec::new(),
            window: Vec::new(),
            samples: Vec::new(),
            bins_db: Vec::new(),
            ranges: Vec::new(),
            raw: Vec::new(),
            cap_velocity: Vec::new(),
            band_power: [0.0; 3],
            flux_baseline: 0.0,
            starved: 0.0,
            frame: Frame::default(),
        };

        analyzer.rebuild_fft();
        analyzer.rebuild_ranges(64);
        analyzer
    }

    /// Tell the analyzer the device rate changed, so the mapping follows.
    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        let rate = sample_rate.max(1) as f32;
        if (rate - self.sample_rate).abs() < f32::EPSILON {
            return;
        }

        self.sample_rate = rate;
        self.rebuild_fft();

        let bars = self.ranges.len();
        self.rebuild_ranges(bars);
    }

    /// The window length currently in use.
    pub fn window_len(&self) -> usize {
        self.fft_size
    }

    /// Plan the transform and size every buffer that depends on the window.
    fn rebuild_fft(&mut self) {
        let size = fft_size_for(self.sample_rate as u32);
        if size == self.fft_size {
            return;
        }
        self.fft_size = size;

        let mut planner = RealFftPlanner::<f32>::new();
        self.fft = planner.plan_fft_forward(size);

        self.input = self.fft.make_input_vec();
        self.output = self.fft.make_output_vec();

        // Hann. Periodic rather than symmetric (`/ N`, not `/ (N - 1)`), which
        // is the correct choice for spectral analysis: it makes the window
        // seamless when the signal is treated as periodic, so a bin-centred
        // tone leaks into its neighbours as little as possible.
        self.window = (0..size)
            .map(|n| {
                let phase = std::f32::consts::TAU * n as f32 / size as f32;
                0.5 * (1.0 - phase.cos())
            })
            .collect();

        self.samples = vec![0.0; size];
        self.bins_db = vec![FLOOR_DB; size / 2 + 1];
    }

    pub fn frame(&self) -> &Frame {
        &self.frame
    }

    /// Analyse the newest audio and return the result.
    ///
    /// `dt` is the time since the last call, used to keep the smoothing and the
    /// falling caps running at the same *rate* whether the UI is at 60 fps or
    /// has dropped to 30 in low-power mode. Without it, unfocusing the window
    /// would visibly change how the visualiser behaves.
    pub fn analyze(
        &mut self,
        monitor: &mut super::Monitor,
        settings: &mp_core::config::Visualizer,
        dt: f32,
    ) -> &Frame {
        let bars = settings.bar_count.clamp(MIN_BARS, MAX_BARS);
        if bars != self.ranges.len() {
            self.rebuild_ranges(bars);
        }

        // A stalled UI can hand us an enormous dt; a paused one, zero. Clamping
        // keeps the smoothing from jumping or freezing at the extremes.
        let dt = dt.clamp(1.0 / 240.0, 1.0 / 15.0);

        if monitor.poll() > 0 {
            self.starved = 0.0;
        } else {
            // Nothing arrived. After a grace period long enough to rule out
            // simply having landed between two device buffers, treat the
            // absence as what it is — silence — and let it scroll into the
            // history at real time, so the display settles instead of freezing
            // on whatever happened to be playing when the music stopped.
            self.starved += dt;
            if self.starved > STARVE_GRACE_SECS {
                monitor.starve((dt * self.sample_rate) as usize);
            }
        }

        monitor.latest(&mut self.samples);

        let gain = settings.sensitivity.clamp(0.1, 4.0);

        // Level of the window, before any of the frequency work.
        let mut sum_squares = 0.0_f32;
        let mut peak = 0.0_f32;
        for sample in &self.samples {
            let value = *sample * gain;
            sum_squares += value * value;
            peak = peak.max(value.abs());
        }
        let rms = (sum_squares / self.samples.len() as f32).sqrt();

        // Below this the input is silence, not quiet music: analysing it would
        // just amplify the noise floor into a twitching display.
        let active = peak > 1e-4;

        self.transform(gain);
        self.fill_bars();
        let flux = self.spectral_flux();
        self.smooth(settings, dt, active);
        self.update_caps(settings, dt);
        self.update_bands(active);
        self.update_onset(flux, dt);
        self.fill_wave(gain);

        self.frame.rms = rms.min(1.0);
        self.frame.peak = peak;
        self.frame.active = active;

        &self.frame
    }

    // -- internals ---------------------------------------------------------

    /// Work out which FFT bins feed each bar.
    ///
    /// The mapping is logarithmic because hearing is: an octave from 100 to
    /// 200 Hz deserves as much width as one from 5 to 10 kHz, and a linear
    /// mapping would spend three quarters of the display on the top two
    /// octaves, where music has very little going on.
    fn rebuild_ranges(&mut self, bars: usize) {
        let bars = bars.clamp(MIN_BARS, MAX_BARS);
        let bin_hz = self.sample_rate / self.fft_size as f32;
        // `sample_rate` was built from an integer rate, so the cast is exact.
        let ratio = top_hz(self.sample_rate as u32) / MIN_HZ;

        self.ranges.clear();
        self.ranges.reserve(bars);

        let last_bin = self.fft_size / 2;

        for index in 0..bars {
            let lo_hz = MIN_HZ * ratio.powf(index as f32 / bars as f32);
            let hi_hz = MIN_HZ * ratio.powf((index + 1) as f32 / bars as f32);

            // Bin 0 is DC, which is never interesting and is often a large
            // offset in badly mastered files.
            let lo = ((lo_hz / bin_hz).floor() as usize).max(1).min(last_bin);
            // At least one bin wide: with many bars the low end asks for a
            // fraction of a bin, and an empty range would draw a permanent gap.
            let hi = ((hi_hz / bin_hz).ceil() as usize)
                .max(lo + 1)
                .min(last_bin + 1);

            // Geometric mean, not arithmetic: the bars are spaced
            // logarithmically, so the middle of one is the middle on a log
            // scale too.
            let centre = (lo_hz * hi_hz).sqrt() / bin_hz;
            let narrow = (hi_hz - lo_hz) < bin_hz;

            self.ranges.push(BarRange {
                lo,
                hi,
                centre,
                narrow,
            });
        }

        self.raw = vec![0.0; bars];
        self.cap_velocity = vec![0.0; bars];
        self.frame.bars = vec![0.0; bars];
        self.frame.peaks = vec![0.0; bars];
    }

    /// Window the samples and take the FFT, leaving per-bin dB in `bins_db`.
    fn transform(&mut self, gain: f32) {
        for ((slot, sample), window) in self
            .input
            .iter_mut()
            .zip(self.samples.iter())
            .zip(self.window.iter())
        {
            *slot = *sample * gain * *window;
        }

        if self.fft.process(&mut self.input, &mut self.output).is_err() {
            // Only possible on a length mismatch, which cannot happen here.
            self.bins_db.fill(FLOOR_DB);
            return;
        }

        // Single-sided amplitude. The 2 undoes the negative-frequency half we
        // are not looking at, and dividing by the window's coherent gain (0.5
        // for Hann) undoes the amplitude the window itself removed — so a
        // full-scale sine reads 0 dB rather than about -6.
        let scale = 4.0 / self.fft_size as f32;
        let bin_hz = self.sample_rate / self.fft_size as f32;

        let mut power = [0.0_f32; 3];

        for (index, (slot, bin)) in self.bins_db.iter_mut().zip(self.output.iter()).enumerate() {
            let amplitude = bin.norm() * scale;

            *slot = if amplitude > 1e-9 {
                20.0 * amplitude.log10()
            } else {
                FLOOR_DB
            };

            // Band power is summed here rather than averaged from the decibel
            // values later. Summing energy is what makes a loud passage read
            // as louder than a quiet one; a mean of decibels across a wide
            // band is dominated by its many near-silent bins and barely moves.
            let freq = index as f32 * bin_hz;
            let band = if freq < BASS_HZ.0 {
                None
            } else if freq < BASS_HZ.1 {
                Some(0)
            } else if freq < MID_HZ.1 {
                Some(1)
            } else if freq < TREBLE_HZ.1 {
                Some(2)
            } else {
                None
            };

            if let Some(band) = band {
                power[band] += amplitude * amplitude;
            }
        }

        self.band_power = power;
    }

    /// Collapse the bins into bars, normalised to `0.0..=1.0`.
    ///
    /// Two regimes, because the low end and the high end have opposite
    /// problems. Up top a bar spans many bins and the question is which of
    /// them to believe. Down at the bottom a bar is *narrower* than a single
    /// bin — at 110 Hz a bar is about 11 Hz wide and a bin is 23 — so several
    /// neighbouring bars would otherwise read the exact same bin and come back
    /// identical. That draws the whole bass end as one flat block, and makes
    /// the peak land on whichever of the tied bars happened to be compared
    /// last rather than on the one nearest the note.
    fn fill_bars(&mut self) {
        for (bar, range) in self.raw.iter_mut().zip(self.ranges.iter()) {
            let db = if !range.narrow {
                // Several bins: take the loudest, not the average. A single
                // strong partial inside a wide high-frequency bar is exactly
                // what you want to see; averaging buries it under its quiet
                // neighbours.
                let mut loudest = FLOOR_DB;
                for value in &self.bins_db[range.lo..range.hi] {
                    if *value > loudest {
                        loudest = *value;
                    }
                }
                loudest
            } else {
                // Finer than the transform can resolve: read the spectrum at
                // the bar's own centre frequency instead. It cannot invent
                // resolution that is not there, but it does give each bar a
                // distinct value, so the bass reads as a smooth slope and the
                // peak sits on the right bar.
                interpolate_db(&self.bins_db, range.centre)
            };

            *bar = ((db - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0);
        }
    }

    /// Sum of positive frame-to-frame change, the basis for beat detection.
    ///
    /// Measured against the *smoothed* bars from the previous frame, which is
    /// what makes it a change detector rather than a loudness meter: a steady
    /// loud passage has near-zero flux, a drum hit has a lot.
    fn spectral_flux(&self) -> f32 {
        let mut flux = 0.0;
        for (raw, previous) in self.raw.iter().zip(self.frame.bars.iter()) {
            let delta = raw - previous;
            if delta > 0.0 {
                flux += delta;
            }
        }
        flux / self.raw.len().max(1) as f32
    }

    /// Asymmetric smoothing: quick to rise, slow to fall.
    ///
    /// Symmetric smoothing makes percussion look like a sine wave — the attack
    /// is the part that carries the rhythm, so it is left almost untouched
    /// while the decay is stretched.
    fn smooth(&mut self, settings: &mp_core::config::Visualizer, dt: f32, active: bool) {
        let smoothing = settings.smoothing.clamp(0.0, 0.95);

        // Per-frame coefficients, quoted at 60 fps and then corrected for the
        // frame we actually got.
        let release = rate_adjust((1.0 - smoothing).clamp(0.02, 1.0), dt);
        let attack = rate_adjust(((1.0 - smoothing) * 3.0).clamp(0.10, 1.0), dt);

        for (current, raw) in self.frame.bars.iter_mut().zip(self.raw.iter()) {
            let target = if active { *raw } else { 0.0 };
            let step = if target > *current { attack } else { release };
            *current += (target - *current) * step;

            // Keeps a bar from lingering at 0.001 forever, which shows up as a
            // permanent one-pixel line along the bottom.
            if current.abs() < 1e-4 {
                *current = 0.0;
            }
        }
    }

    /// Peak-hold caps that accelerate as they fall.
    ///
    /// Constant-speed caps drift down at the same lazy rate whether they are
    /// falling one pixel or the whole height. Gravity means a cap sits briefly
    /// at a new peak and then drops away, which reads as a peak marker rather
    /// than as a second, slower set of bars.
    fn update_caps(&mut self, settings: &mp_core::config::Visualizer, dt: f32) {
        const GRAVITY: f32 = 1.9;

        if !settings.show_peak_caps {
            self.frame.peaks.fill(0.0);
            return;
        }

        for ((cap, velocity), bar) in self
            .frame
            .peaks
            .iter_mut()
            .zip(self.cap_velocity.iter_mut())
            .zip(self.frame.bars.iter())
        {
            if *bar >= *cap {
                *cap = *bar;
                *velocity = 0.0;
            } else {
                *velocity += GRAVITY * dt;
                *cap = (*cap - *velocity * dt).max(*bar);
            }
        }
    }

    /// Low/mid/high energy, for the visualisers that react to a band rather
    /// than to individual bars.
    ///
    /// Derived from the summed power in each range, converted to decibels.
    /// That is what makes these track loudness: a quiet passage and a loud one
    /// differ by real decibels here, where the previous mean-of-decibels
    /// measure pinned bass at 1.0 for almost all material and left every
    /// visualiser driven by it looking identical whatever was playing.
    fn update_bands(&mut self, active: bool) {
        let level = |power: f32| -> f32 {
            if !active || power <= 1e-12 {
                return 0.0;
            }

            // Power, so ten times the logarithm rather than twenty.
            let db = 10.0 * power.log10();
            ((db - BAND_FLOOR_DB) / -BAND_FLOOR_DB).clamp(0.0, 1.0)
        };

        self.frame.bass = level(self.band_power[0]);
        self.frame.mid = level(self.band_power[1]);
        self.frame.treble = level(self.band_power[2]);
    }

    /// Beat strength from spectral flux, measured against its own recent
    /// average so it adapts to the material instead of needing a threshold.
    fn update_onset(&mut self, flux: f32, dt: f32) {
        let baseline_step = rate_adjust(0.06, dt);
        self.flux_baseline += (flux - self.flux_baseline) * baseline_step;

        // A beat is flux well above the recent norm. The floor keeps quiet
        // passages from registering every small fluctuation as a hit.
        let excess = flux - self.flux_baseline * 1.6 - 0.004;
        let strength = (excess * 22.0).clamp(0.0, 1.0);

        // Rises instantly, falls over roughly a third of a second.
        let decayed = self.frame.onset - dt * 3.0;
        self.frame.onset = strength.max(decayed).clamp(0.0, 1.0);
    }

    /// Fill the oscilloscope window, aligned to a rising zero crossing.
    ///
    /// Without the alignment the window starts at an arbitrary phase every
    /// frame and a steady tone appears to slide sideways, which looks like a
    /// scrolling bug rather than a waveform.
    fn fill_wave(&mut self, gain: f32) {
        if self.frame.wave.len() != WAVE_SAMPLES {
            self.frame.wave = vec![0.0; WAVE_SAMPLES];
        }

        let search = TRIGGER_SEARCH.min(self.samples.len());
        let source = &self.samples[self.samples.len() - search..];
        let start = trigger_point(source, WAVE_SAMPLES);

        for (slot, sample) in self.frame.wave.iter_mut().zip(source[start..].iter()) {
            *slot = *sample * gain;
        }
    }
}

/// The top of the displayed range at a given device rate.
///
/// Capped below Nyquist rather than at it: the last few percent is where the
/// anti-alias filter of whatever produced the file rolls off, so it is
/// reliably empty and would only ever draw a dead bar.
pub fn top_hz(sample_rate: u32) -> f32 {
    MAX_HZ
        .min(sample_rate as f32 * 0.5 * 0.95)
        .max(MIN_HZ * 2.0)
}

/// Which bar a frequency falls in.
///
/// The inverse of the mapping [`Analyzer`] bins with, exposed so a frequency
/// axis — or a test asking where a tone should have landed — does not have to
/// reimplement it and drift out of step.
pub fn bar_for_hz(freq: f32, bars: usize, sample_rate: u32) -> usize {
    let bars = bars.clamp(MIN_BARS, MAX_BARS);
    let ratio = top_hz(sample_rate) / MIN_HZ;
    let position = (freq.max(MIN_HZ) / MIN_HZ).log10() / ratio.log10();
    ((position * bars as f32) as usize).min(bars - 1)
}

/// Read the spectrum at a fractional bin position, in dB.
///
/// Interpolating in dB rather than in amplitude is deliberate: the display is
/// a dB scale, so a straight line here is a straight line on screen.
fn interpolate_db(bins: &[f32], position: f32) -> f32 {
    if bins.is_empty() {
        return FLOOR_DB;
    }

    let last = bins.len() - 1;
    let lo = (position.max(0.0) as usize).min(last);
    let hi = (lo + 1).min(last);
    let fraction = (position - lo as f32).clamp(0.0, 1.0);

    bins[lo] + (bins[hi] - bins[lo]) * fraction
}

/// Convert a per-frame smoothing coefficient quoted at 60 fps to one that has
/// the same effect over `dt` seconds.
///
/// `k` is the fraction of the remaining distance covered per frame, so the
/// fraction *remaining* is `1 - k` and compounds; correcting for a different
/// frame time is that same compounding over a different number of frames.
fn rate_adjust(k: f32, dt: f32) -> f32 {
    let frames = dt * 60.0;
    1.0 - (1.0 - k.clamp(0.0, 1.0)).powf(frames)
}

/// Find where a `window`-long slice of `source` should start so the waveform
/// sits still between frames.
///
/// Looks for a rising crossing of zero, requiring the signal to have gone
/// meaningfully *negative* first. That hysteresis is what stops a waveform
/// that hovers near zero — which is most quiet music — from triggering on
/// noise and jittering.
fn trigger_point(source: &[f32], window: usize) -> usize {
    if source.len() <= window {
        return 0;
    }

    let limit = source.len() - window;

    let mut peak = 0.0_f32;
    for sample in &source[..limit] {
        peak = peak.max(sample.abs());
    }

    // Nothing to lock onto.
    if peak < 1e-5 {
        return 0;
    }

    let threshold = peak * 0.05;
    let mut armed = false;

    for (index, sample) in source[..limit].iter().enumerate() {
        if *sample < -threshold {
            armed = true;
        } else if armed && *sample >= threshold {
            return index;
        }
    }

    // No crossing found: a very low-frequency or heavily offset signal. The
    // most recent audio is the least wrong answer.
    limit
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::viz;
    use mp_core::config::Visualizer as VizSettings;

    const RATE: u32 = 48_000;

    fn settings() -> VizSettings {
        VizSettings {
            // Smoothing off, so one call reflects one window rather than a
            // fraction of it.
            smoothing: 0.0,
            ..VizSettings::default()
        }
    }

    /// Feed a tone and read back which bar it landed in.
    fn analyse_tone(freq: f32, bars: usize) -> (Analyzer, Frame) {
        let (mut tap, mut monitor) = viz::channel();
        let mut analyzer = Analyzer::new(RATE);

        for n in 0..viz::HISTORY_SAMPLES {
            let phase = std::f32::consts::TAU * freq * n as f32 / RATE as f32;
            tap.push_frame(&[phase.sin()], 1.0);
            if n % 4000 == 0 {
                monitor.poll();
            }
        }
        monitor.poll();

        let mut config = settings();
        config.bar_count = bars;

        // Twice: the first call has nothing to smooth against.
        analyzer.analyze(&mut monitor, &config, 1.0 / 60.0);
        let frame = analyzer.analyze(&mut monitor, &config, 1.0 / 60.0).clone();

        (analyzer, frame)
    }

    /// The bar a frequency should land in, by the same log mapping the
    /// analyzer uses — derived independently here so the test would catch the
    /// mapping being wrong, not just changed.
    fn expected_bar(freq: f32, bars: usize) -> usize {
        let top = MAX_HZ.min(RATE as f32 * 0.5 * 0.95);
        let position = (freq / MIN_HZ).log10() / (top / MIN_HZ).log10();
        ((position * bars as f32) as usize).min(bars - 1)
    }

    #[test]
    fn a_tone_lands_in_the_bar_for_its_frequency() {
        for freq in [100.0, 440.0, 1_000.0, 4_000.0] {
            let (_, frame) = analyse_tone(freq, 64);

            let loudest = frame
                .bars
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
                .map(|(index, _)| index)
                .unwrap();

            let want = expected_bar(freq, 64);
            assert!(
                loudest.abs_diff(want) <= 1,
                "{freq} Hz peaked at bar {loudest}, expected about {want}"
            );
        }
    }

    /// A full-scale sine should read at or near the top of the scale. If the
    /// window's coherent gain were not corrected for, this would sit around
    /// 0.92 instead — a 6 dB error that is invisible by eye.
    #[test]
    fn a_full_scale_tone_reaches_the_top_of_the_scale() {
        let (_, frame) = analyse_tone(1_000.0, 64);
        let loudest = frame.bars.iter().copied().fold(0.0_f32, f32::max);

        assert!(
            loudest > 0.97,
            "a full-scale sine only reached {loudest:.3} of the display range"
        );
    }

    #[test]
    fn silence_reads_as_silence_and_is_marked_inactive() {
        let (_tap, mut monitor) = viz::channel();
        let mut analyzer = Analyzer::new(RATE);
        monitor.poll();

        let frame = analyzer.analyze(&mut monitor, &settings(), 1.0 / 60.0);

        assert!(!frame.active, "silence should not be marked active");
        assert!(frame.bars.iter().all(|bar| *bar == 0.0));
        assert_eq!(frame.peak, 0.0);
    }

    /// Changing the bar count must resize every parallel array, or the next
    /// analysis indexes past the end of one of them.
    #[test]
    fn changing_the_bar_count_resizes_everything() {
        let (_tap, mut monitor) = viz::channel();
        let mut analyzer = Analyzer::new(RATE);
        monitor.poll();

        for bars in [16, 128, 32, 256, 8] {
            let mut config = settings();
            config.bar_count = bars;
            let frame = analyzer.analyze(&mut monitor, &config, 1.0 / 60.0);

            assert_eq!(frame.bars.len(), bars);
            assert_eq!(frame.peaks.len(), bars);
        }
    }

    #[test]
    fn the_bar_count_is_clamped_to_something_drawable() {
        let (_tap, mut monitor) = viz::channel();
        let mut analyzer = Analyzer::new(RATE);
        monitor.poll();

        let mut config = settings();

        config.bar_count = 0;
        assert_eq!(
            analyzer
                .analyze(&mut monitor, &config, 1.0 / 60.0)
                .bars
                .len(),
            MIN_BARS
        );

        config.bar_count = 100_000;
        assert_eq!(
            analyzer
                .analyze(&mut monitor, &config, 1.0 / 60.0)
                .bars
                .len(),
            MAX_BARS
        );
    }

    /// Bass in the bass band and treble in the treble band, not smeared.
    #[test]
    fn band_energies_follow_the_content() {
        let (_, low) = analyse_tone(80.0, 64);
        assert!(
            low.bass > low.treble,
            "an 80 Hz tone read bass {:.3} against treble {:.3}",
            low.bass,
            low.treble
        );

        let (_, high) = analyse_tone(8_000.0, 64);
        assert!(
            high.treble > high.bass,
            "an 8 kHz tone read treble {:.3} against bass {:.3}",
            high.treble,
            high.bass
        );
    }

    /// The whole point of the trigger: a steady tone must not slide sideways.
    #[test]
    fn the_oscilloscope_trigger_holds_a_steady_tone_still() {
        let (mut tap, mut monitor) = viz::channel();
        let mut analyzer = Analyzer::new(RATE);
        let config = settings();

        let mut phase = 0.0_f32;
        let step = std::f32::consts::TAU * 440.0 / RATE as f32;

        let push =
            |tap: &mut viz::Tap, monitor: &mut viz::Monitor, count: usize, phase: &mut f32| {
                for _ in 0..count {
                    tap.push_frame(&[phase.sin()], 1.0);
                    *phase += step;
                }
                monitor.poll();
            };

        push(&mut tap, &mut monitor, viz::HISTORY_SAMPLES, &mut phase);
        let first = analyzer
            .analyze(&mut monitor, &config, 1.0 / 60.0)
            .wave
            .clone();

        // Advance by a deliberately non-periodic amount, so an untriggered
        // window would come back at a completely different phase.
        push(&mut tap, &mut monitor, 813, &mut phase);
        let second = analyzer
            .analyze(&mut monitor, &config, 1.0 / 60.0)
            .wave
            .clone();

        // Both windows start just after a rising zero crossing, so they should
        // line up closely.
        let worst = first
            .iter()
            .zip(second.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0_f32, f32::max);

        assert!(
            worst < 0.1,
            "the triggered window shifted by up to {worst:.3} between frames"
        );
    }

    #[test]
    fn the_trigger_finds_a_rising_crossing() {
        // Two full cycles, starting at the positive peak so index 0 is wrong.
        let source: Vec<f32> = (0..3072)
            .map(|n| (std::f32::consts::TAU * n as f32 / 512.0).cos())
            .collect();

        let start = trigger_point(&source, WAVE_SAMPLES);

        assert!(
            source[start] >= 0.0,
            "the trigger landed on a negative sample"
        );
        assert!(
            source[start.saturating_sub(1)] < source[start],
            "the trigger landed on a falling edge"
        );
    }

    #[test]
    fn the_trigger_gives_up_gracefully_on_silence() {
        let source = vec![0.0; 3072];
        assert_eq!(trigger_point(&source, WAVE_SAMPLES), 0);
    }

    /// Smoothing has to describe the same behaviour over time regardless of
    /// frame rate, or low-power mode would change how the display moves.
    #[test]
    fn smoothing_is_corrected_for_frame_rate() {
        // Sixty frames at 60 fps, and thirty at 30 fps, cover one second each.
        let mut fast = 0.0_f32;
        for _ in 0..60 {
            fast += (1.0 - fast) * rate_adjust(0.2, 1.0 / 60.0);
        }

        let mut slow = 0.0_f32;
        for _ in 0..30 {
            slow += (1.0 - slow) * rate_adjust(0.2, 1.0 / 30.0);
        }

        assert!(
            (fast - slow).abs() < 0.01,
            "one second of smoothing differed by frame rate: {fast:.4} against {slow:.4}"
        );
    }

    /// The caps mark peaks; they must never sit below the bar they cap.
    #[test]
    fn peak_caps_stay_at_or_above_their_bars() {
        let (analyzer, frame) = analyse_tone(1_000.0, 64);
        let _ = analyzer;

        for (index, (bar, cap)) in frame.bars.iter().zip(frame.peaks.iter()).enumerate() {
            assert!(
                cap >= bar,
                "bar {index} is {bar:.3} but its cap is {cap:.3}"
            );
        }
    }

    #[test]
    fn turning_peak_caps_off_clears_them() {
        let (_tap, mut monitor) = viz::channel();
        let mut analyzer = Analyzer::new(RATE);
        monitor.poll();

        let mut config = settings();
        config.show_peak_caps = false;

        let frame = analyzer.analyze(&mut monitor, &config, 1.0 / 60.0);
        assert!(frame.peaks.iter().all(|cap| *cap == 0.0));
    }

    /// Sensitivity is an input gain, so it should move the display.
    #[test]
    fn sensitivity_scales_the_reading() {
        // A fresh feed per reading. `analyze` polls for itself and starves a
        // feed that has gone quiet, so taking both readings from one filled
        // buffer would measure the second against a partly-erased history.
        let reading_at = |sensitivity: f32| -> f32 {
            let (mut tap, mut monitor) = viz::channel();
            let mut analyzer = Analyzer::new(RATE);

            for n in 0..viz::HISTORY_SAMPLES {
                let phase = std::f32::consts::TAU * 1_000.0 * n as f32 / RATE as f32;
                // Quiet, so there is room to grow before hitting the ceiling.
                tap.push_frame(&[phase.sin() * 0.02], 1.0);
                if n % 4000 == 0 {
                    monitor.poll();
                }
            }

            let mut config = settings();
            config.sensitivity = sensitivity;
            analyzer.analyze(&mut monitor, &config, 1.0 / 60.0).rms
        };

        let low = reading_at(1.0);
        let high = reading_at(4.0);

        assert!(
            high > low * 3.0,
            "sensitivity barely moved: {low:.4} to {high:.4}"
        );
    }

    /// A rate change has to remap the bins, or every bar points at the wrong
    /// frequency after the device switches.
    #[test]
    fn a_sample_rate_change_remaps_the_bars() {
        let mut analyzer = Analyzer::new(44_100);
        let before = analyzer.ranges.clone();

        analyzer.set_sample_rate(96_000);
        let after = analyzer.ranges.clone();

        assert_eq!(before.len(), after.len());
        assert!(
            before
                .iter()
                .zip(after.iter())
                .any(|(a, b)| a.lo != b.lo || a.hi != b.hi),
            "doubling the sample rate left the bin mapping untouched"
        );
    }

    /// Every bar must own at least one bin, at any count and any rate — an
    /// empty range would draw a bar that is permanently zero.
    #[test]
    fn every_bar_covers_at_least_one_bin() {
        for rate in [44_100, 48_000, 96_000, 192_000] {
            for bars in [MIN_BARS, 64, 128, MAX_BARS] {
                let mut analyzer = Analyzer::new(rate);
                analyzer.rebuild_ranges(bars);

                for (index, range) in analyzer.ranges.iter().enumerate() {
                    assert!(
                        range.hi > range.lo,
                        "at {rate} Hz with {bars} bars, bar {index} covers no bins"
                    );
                    assert!(
                        range.hi <= analyzer.fft_size / 2 + 1,
                        "at {rate} Hz with {bars} bars, bar {index} runs past the spectrum"
                    );
                }
            }
        }
    }

    /// The published inverse mapping has to agree with the binning the
    /// analyzer actually does, or a frequency axis would label the wrong bars.
    #[test]
    fn the_published_mapping_matches_the_real_binning() {
        for rate in [44_100_u32, 48_000, 96_000] {
            let mut analyzer = Analyzer::new(rate);
            analyzer.rebuild_ranges(64);

            let bin_hz = rate as f32 / analyzer.fft_size as f32;

            for freq in [40.0_f32, 100.0, 440.0, 1_000.0, 5_000.0, 12_000.0] {
                if freq >= top_hz(rate) {
                    continue;
                }

                let bar = bar_for_hz(freq, 64, rate);
                let range = analyzer.ranges[bar];

                let lo = range.lo as f32 * bin_hz;
                let hi = range.hi as f32 * bin_hz;

                assert!(
                    freq >= lo - bin_hz && freq <= hi + bin_hz,
                    "at {rate} Hz, {freq} Hz maps to bar {bar}, which covers                      {lo:.0}..{hi:.0} Hz"
                );
            }
        }
    }

    /// The bug this fixes: adjacent low bars reading one shared bin and
    /// coming back byte-identical, which draws the bass as a solid block.
    #[test]
    fn bars_finer_than_a_bin_still_differ_from_each_other() {
        let (mut tap, mut monitor) = viz::channel();
        let mut analyzer = Analyzer::new(96_000);

        for n in 0..viz::HISTORY_SAMPLES {
            let phase = std::f32::consts::TAU * 110.0 * n as f32 / 96_000.0;
            tap.push_frame(&[phase.sin()], 1.0);
            if n % 4000 == 0 {
                monitor.poll();
            }
        }

        let mut config = settings();
        config.bar_count = 64;
        let frame = analyzer.analyze(&mut monitor, &config, 1.0 / 60.0);

        // Bars 12..16 all sit inside one or two bins at this rate.
        let window = &frame.bars[12..16];
        let identical = window.windows(2).filter(|pair| pair[0] == pair[1]).count();

        assert_eq!(
            identical, 0,
            "neighbouring sub-bin bars came back identical: {window:?}"
        );
    }

    #[test]
    fn interpolation_lands_on_the_bins_it_sits_between() {
        let bins = [-60.0, -20.0, -40.0];

        assert_eq!(interpolate_db(&bins, 0.0), -60.0);
        assert_eq!(interpolate_db(&bins, 1.0), -20.0);
        assert_eq!(interpolate_db(&bins, 0.5), -40.0);

        // Past either end clamps rather than reading out of bounds.
        assert_eq!(interpolate_db(&bins, -5.0), -60.0);
        assert_eq!(interpolate_db(&bins, 99.0), -40.0);
        assert_eq!(interpolate_db(&[], 3.0), FLOOR_DB);
    }

    /// The band energies drive how bright the visualisers paint, so they have
    /// to separate a quiet passage from a loud one. The previous measure — a
    /// mean of decibels across each range — did not: it pinned bass near 1.0
    /// for almost anything, and the aurora came out as a white slab whatever
    /// was playing.
    #[test]
    fn band_energy_tracks_how_loud_the_music_is() {
        let reading = |amplitude: f32| -> f32 {
            let (mut tap, mut monitor) = viz::channel();
            let mut analyzer = Analyzer::new(RATE);

            for n in 0..viz::HISTORY_SAMPLES {
                let phase = std::f32::consts::TAU * 100.0 * n as f32 / RATE as f32;
                tap.push_frame(&[phase.sin() * amplitude], 1.0);
                if n % 4000 == 0 {
                    monitor.poll();
                }
            }

            analyzer.analyze(&mut monitor, &settings(), 1.0 / 60.0).bass
        };

        let loud = reading(0.9);
        let middling = reading(0.15);
        let quiet = reading(0.02);

        assert!(
            loud > middling && middling > quiet,
            "bass did not order by level: {loud:.3}, {middling:.3}, {quiet:.3}"
        );

        // And the separation has to be big enough to see, not a rounding
        // difference. Roughly 33 dB of input should span most of the range.
        assert!(
            loud - quiet > 0.35,
            "loud {loud:.3} and quiet {quiet:.3} are only {:.3} apart",
            loud - quiet
        );
    }

    /// The whole point of scaling the window: frequency resolution has to
    /// stay put as the device rate changes, or the bass end collapses into a
    /// single bin on a high-rate device.
    #[test]
    fn frequency_resolution_holds_across_sample_rates() {
        for rate in [44_100_u32, 48_000, 96_000, 192_000] {
            let analyzer = Analyzer::new(rate);
            let bin_hz = rate as f32 / analyzer.window_len() as f32;

            assert!(
                (20.0..=25.0).contains(&bin_hz),
                "at {rate} Hz the bins are {bin_hz:.1} Hz wide"
            );
        }
    }

    /// The buffers are all sized off the window, so a rate change has to
    /// resize every one of them together.
    #[test]
    fn a_rate_change_resizes_the_whole_transform() {
        let mut analyzer = Analyzer::new(44_100);
        assert_eq!(analyzer.window_len(), BASE_FFT_SIZE);

        analyzer.set_sample_rate(192_000);

        let size = analyzer.window_len();
        assert_eq!(size, BASE_FFT_SIZE * 4);
        assert_eq!(analyzer.samples.len(), size);
        assert_eq!(analyzer.window.len(), size);
        assert_eq!(analyzer.input.len(), size);
        assert_eq!(analyzer.output.len(), size / 2 + 1);
        assert_eq!(analyzer.bins_db.len(), size / 2 + 1);
    }

    /// A tone must land in the right bar at every rate — this is the case
    /// that failed on a real 96 kHz device before the window was scaled.
    #[test]
    fn a_low_tone_lands_correctly_at_a_high_sample_rate() {
        for rate in [44_100_u32, 96_000] {
            let (mut tap, mut monitor) = viz::channel();
            let mut analyzer = Analyzer::new(rate);

            for n in 0..viz::HISTORY_SAMPLES {
                let phase = std::f32::consts::TAU * 110.0 * n as f32 / rate as f32;
                tap.push_frame(&[phase.sin()], 1.0);
                if n % 4000 == 0 {
                    monitor.poll();
                }
            }

            let mut config = settings();
            config.bar_count = 64;
            analyzer.analyze(&mut monitor, &config, 1.0 / 60.0);
            let frame = analyzer.analyze(&mut monitor, &config, 1.0 / 60.0);

            let loudest = frame
                .bars
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
                .map(|(index, _)| index)
                .unwrap();

            let want = bar_for_hz(110.0, 64, rate);
            assert!(
                loudest.abs_diff(want) <= 1,
                "at {rate} Hz, 110 Hz peaked at bar {loudest}, expected about {want}"
            );
        }
    }

    /// When the audio stops the display has to settle, not freeze.
    #[test]
    fn a_stopped_feed_settles_to_silence() {
        let (mut tap, mut monitor) = viz::channel();
        let mut analyzer = Analyzer::new(RATE);
        let config = settings();

        for n in 0..viz::HISTORY_SAMPLES {
            let phase = std::f32::consts::TAU * 1_000.0 * n as f32 / RATE as f32;
            tap.push_frame(&[phase.sin()], 1.0);
            if n % 4000 == 0 {
                monitor.poll();
            }
        }

        assert!(
            analyzer.analyze(&mut monitor, &config, 1.0 / 60.0).active,
            "the fixture never registered"
        );

        // The tap goes quiet. Half a second of UI frames, no arrivals.
        let mut active_after = true;
        for _ in 0..30 {
            active_after = analyzer.analyze(&mut monitor, &config, 1.0 / 60.0).active;
        }

        assert!(
            !active_after,
            "the display was still showing audio half a second after it stopped"
        );

        let frame = analyzer.frame();
        let loudest = frame.bars.iter().copied().fold(0.0_f32, f32::max);
        assert!(
            loudest < 0.05,
            "bars were still at {loudest:.3} after the stop"
        );
    }

    /// The grace period exists so a UI frame that lands between two device
    /// buffers does not punch a hole in the waveform.
    #[test]
    fn a_single_frame_without_arrivals_does_not_erase_anything() {
        let (mut tap, mut monitor) = viz::channel();
        let mut analyzer = Analyzer::new(RATE);
        let config = settings();

        for n in 0..viz::HISTORY_SAMPLES {
            let phase = std::f32::consts::TAU * 1_000.0 * n as f32 / RATE as f32;
            tap.push_frame(&[phase.sin()], 1.0);
            if n % 4000 == 0 {
                monitor.poll();
            }
        }
        analyzer.analyze(&mut monitor, &config, 1.0 / 60.0);

        // Two empty frames — within the grace period.
        analyzer.analyze(&mut monitor, &config, 1.0 / 60.0);
        let frame = analyzer.analyze(&mut monitor, &config, 1.0 / 60.0);

        assert!(
            frame.active,
            "a brief gap between device buffers blanked the display"
        );
    }

    /// Bars are drawn as a fraction of a height, so anything outside 0..1
    /// would paint outside its rectangle.
    #[test]
    fn everything_stays_within_its_normalised_range() {
        let (_, frame) = analyse_tone(1_000.0, 64);

        for value in frame.bars.iter().chain(frame.peaks.iter()).chain([
            &frame.rms,
            &frame.bass,
            &frame.mid,
            &frame.treble,
            &frame.onset,
        ]) {
            assert!(
                (0.0..=1.0).contains(value),
                "a normalised value escaped its range: {value}"
            );
        }
    }
}
