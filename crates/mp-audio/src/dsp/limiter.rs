//! A soft-knee peak limiter, guarding the output against the equalizer.
//!
//! Boosting four bands by 12 dB clips almost any modern master, and clipping is
//! the worst possible failure here because it sounds like the *music* is broken
//! rather than the settings. So this is on by default.
//!
//! Two decisions worth stating:
//!
//! * **No logarithms.** This runs in the audio callback, so the gain curve is a
//!   rational function of the linear peak rather than the usual dB-domain
//!   compressor maths. It has the same shape — unity below the knee, smoothly
//!   bending to the ceiling above it — and costs one division.
//! * **Instant attack, smoothed release.** Without lookahead, a smoothed attack
//!   cannot guarantee a ceiling: a transient arrives before the gain has moved.
//!   Clamping immediately and easing back gives a ceiling that genuinely holds,
//!   which is the entire point of a limiter, at the cost of a little distortion
//!   on the sharpest transients.

/// How the limiter should behave. Computed on the control thread.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    /// Ceiling as a linear amplitude. Output never exceeds this.
    pub ceiling: f32,
    /// Where the curve starts to bend, as a fraction of the ceiling.
    pub knee: f32,
    /// Per-sample release coefficient, precomputed from the sample rate.
    pub release: f32,
    pub enabled: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self::bypassed()
    }
}

impl Settings {
    /// Default ceiling, a hair below full scale.
    ///
    /// Leaving a little headroom matters for anything downstream that
    /// resamples or converts to a lossy format, both of which can overshoot a
    /// signal that sits exactly at 0 dBFS.
    pub const DEFAULT_CEILING_DB: f32 = -0.3;

    /// The knee starts 6 dB below the ceiling.
    pub const DEFAULT_KNEE: f32 = 0.5;

    /// Time to recover from gain reduction, in seconds.
    pub const DEFAULT_RELEASE_SECS: f32 = 0.150;

    pub fn bypassed() -> Self {
        Self {
            ceiling: 1.0,
            knee: Self::DEFAULT_KNEE,
            release: 0.0,
            enabled: false,
        }
    }

    /// Build for a given sample rate.
    pub fn new(enabled: bool, ceiling_db: f32, sample_rate: f32) -> Self {
        if !enabled || sample_rate <= 0.0 {
            return Self::bypassed();
        }

        // One-pole coefficient. The `exp` runs here, never in the callback.
        let release = 1.0 - (-1.0 / (sample_rate * Self::DEFAULT_RELEASE_SECS)).exp();

        Self {
            ceiling: super::eq::db_to_linear(ceiling_db.min(0.0)),
            knee: Self::DEFAULT_KNEE,
            release: release.clamp(0.0, 1.0),
            enabled,
        }
    }
}

/// The limiter's running state.
#[derive(Debug, Clone, Copy)]
pub struct Limiter {
    /// Current gain multiplier, 0..=1.
    gain: f32,
    /// Frames spent reducing, for the clip indicator.
    reduced_frames: u64,
}

impl Default for Limiter {
    fn default() -> Self {
        Self::new()
    }
}

impl Limiter {
    pub const fn new() -> Self {
        Self {
            gain: 1.0,
            reduced_frames: 0,
        }
    }

    /// Process one frame in place, applying a single gain across its channels.
    ///
    /// One gain for the whole frame, not per channel: reducing channels
    /// independently would move the stereo image every time a transient landed
    /// harder on one side.
    #[inline]
    pub fn process(&mut self, frame: &mut [f32], settings: &Settings) {
        if !settings.enabled {
            return;
        }

        let mut peak = 0.0_f32;
        for sample in frame.iter() {
            peak = peak.max(sample.abs());
        }

        let target = self.target_gain(peak, settings);

        // Down immediately, up gradually. See the module comment.
        if target < self.gain {
            self.gain = target;
            self.reduced_frames = self.reduced_frames.wrapping_add(1);
        } else {
            self.gain += (target - self.gain) * settings.release;
        }

        if self.gain < 1.0 {
            for sample in frame.iter_mut() {
                *sample *= self.gain;
            }
        }
    }

    /// The gain that would bring `peak` onto the soft-knee curve.
    #[inline]
    fn target_gain(&self, peak: f32, settings: &Settings) -> f32 {
        let knee_start = settings.ceiling * settings.knee;

        if !peak.is_finite() || peak <= knee_start {
            return 1.0;
        }

        // Map the region above the knee onto a curve that starts at the knee
        // with unity slope and approaches the ceiling asymptotically:
        //
        //     f(x) = knee + (ceiling - knee) * t / (1 + t),  t = (x - knee) / (ceiling - knee)
        //
        // so the output is always strictly below the ceiling, however loud the
        // input, without a hard corner where the limiting begins.
        let span = settings.ceiling - knee_start;
        if span <= 0.0 {
            return settings.ceiling / peak;
        }

        let t = (peak - knee_start) / span;
        let limited = knee_start + span * (t / (1.0 + t));

        (limited / peak).clamp(0.0, 1.0)
    }

    /// Whether the limiter is currently pulling the level down, for the UI's
    /// clip indicator.
    pub fn is_reducing(&self) -> bool {
        self.gain < 0.999
    }

    /// How much reduction is being applied, as a linear multiplier.
    pub fn gain(&self) -> f32 {
        self.gain
    }

    /// Release the limiter immediately. Used on seek and track change, where
    /// holding reduction from the previous audio would duck the new start.
    pub fn reset(&mut self) {
        self.gain = 1.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f32 = 48_000.0;

    fn settings() -> Settings {
        Settings::new(true, Settings::DEFAULT_CEILING_DB, RATE)
    }

    /// The one property that matters: nothing gets out above the ceiling.
    #[test]
    fn the_ceiling_is_never_exceeded() {
        let settings = settings();
        let mut limiter = Limiter::new();

        // Deliberately brutal: a signal four times full scale, arriving with no
        // warning, which is the worst case for a limiter without lookahead.
        for amplitude in [1.0, 2.0, 4.0, 10.0] {
            limiter.reset();
            for n in 0..10_000 {
                let mut frame = [
                    amplitude * (n as f32 * 0.1).sin(),
                    amplitude * (n as f32 * 0.13).cos(),
                ];
                limiter.process(&mut frame, &settings);

                for sample in frame {
                    assert!(
                        sample.abs() <= settings.ceiling + 1e-6,
                        "amplitude {amplitude} produced {sample} above the {} ceiling",
                        settings.ceiling
                    );
                }
            }
        }
    }

    /// Quiet material must come through completely untouched, or the limiter
    /// is a compressor nobody asked for.
    #[test]
    fn quiet_audio_passes_through_bit_for_bit() {
        let settings = settings();
        let mut limiter = Limiter::new();

        for n in 0..5_000 {
            let input = 0.4 * (n as f32 * 0.01).sin();
            let mut frame = [input, input];
            limiter.process(&mut frame, &settings);
            assert_eq!(frame[0], input);
        }

        assert!(!limiter.is_reducing());
    }

    #[test]
    fn a_disabled_limiter_does_nothing_at_all() {
        let settings = Settings::bypassed();
        let mut limiter = Limiter::new();

        let mut frame = [5.0, -5.0];
        limiter.process(&mut frame, &settings);
        assert_eq!(frame, [5.0, -5.0]);
    }

    /// Both channels must be scaled by the same amount, or a transient on one
    /// side pulls the image across.
    #[test]
    fn the_stereo_image_does_not_move() {
        let settings = settings();
        let mut limiter = Limiter::new();

        // Left is loud, right is quiet, at a constant ratio.
        for _ in 0..1_000 {
            let mut frame = [2.0, 0.5];
            limiter.process(&mut frame, &settings);
            let ratio = frame[0] / frame[1];
            assert!(
                (ratio - 4.0).abs() < 1e-4,
                "channel ratio drifted to {ratio}, should stay 4.0"
            );
        }
    }

    /// The knee is what makes it a limiter rather than a clipper: the onset of
    /// gain reduction has to be gradual.
    #[test]
    fn the_knee_bends_rather_than_corners() {
        let settings = settings();
        let limiter = Limiter::new();

        let knee_start = settings.ceiling * settings.knee;

        // Just below the knee: untouched.
        assert_eq!(limiter.target_gain(knee_start * 0.99, &settings), 1.0);

        // Just above: reducing, but only slightly.
        let just_above = limiter.target_gain(knee_start * 1.05, &settings);
        assert!(just_above < 1.0);
        assert!(
            just_above > 0.98,
            "the knee should bend gently, got {just_above}"
        );

        // Far above: reducing hard.
        let far_above = limiter.target_gain(4.0, &settings);
        assert!(far_above < 0.3);
    }

    /// Gain reduction has to recover, or one loud moment ducks the rest of the
    /// track.
    #[test]
    fn the_limiter_releases_after_a_transient() {
        let settings = settings();
        let mut limiter = Limiter::new();

        let mut loud = [3.0, 3.0];
        limiter.process(&mut loud, &settings);
        assert!(limiter.is_reducing());

        // A second of quiet.
        for _ in 0..RATE as usize {
            let mut quiet = [0.1, 0.1];
            limiter.process(&mut quiet, &settings);
        }

        assert!(
            !limiter.is_reducing(),
            "still reducing by {} after a second of quiet",
            limiter.gain()
        );
    }

    /// A NaN must not lock the limiter into permanent silence.
    #[test]
    fn a_non_finite_sample_does_not_poison_the_gain() {
        let settings = settings();
        let mut limiter = Limiter::new();

        let mut frame = [f32::NAN, 0.0];
        limiter.process(&mut frame, &settings);
        assert!(limiter.gain().is_finite());

        limiter.reset();
        let mut normal = [0.2, 0.2];
        limiter.process(&mut normal, &settings);
        assert_eq!(normal[0], 0.2);
    }

    #[test]
    fn resetting_releases_immediately() {
        let settings = settings();
        let mut limiter = Limiter::new();

        let mut loud = [5.0, 5.0];
        limiter.process(&mut loud, &settings);
        assert!(limiter.is_reducing());

        limiter.reset();
        assert!(!limiter.is_reducing());
    }
}
