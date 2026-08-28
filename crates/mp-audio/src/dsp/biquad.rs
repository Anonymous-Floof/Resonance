//! Second-order IIR filter sections — the building block of the equalizer.
//!
//! The coefficient formulas are the RBJ Audio EQ Cookbook's, kept in-tree
//! rather than pulled from a crate. They are forty lines of arithmetic, and
//! having them here is what lets [`Coefficients::magnitude_db`] sit beside them:
//! the UI draws the equalizer's response curve from the *same* coefficients the
//! audio thread runs, so the curve on screen cannot drift from the sound.
//!
//! Everything expensive — `sin`, `cos`, `sqrt`, `powf` — happens when
//! coefficients are built, which is on the control thread. The audio thread only
//! ever multiplies and adds.

use std::f32::consts::PI;

/// A biquad's coefficients, already normalised so `a0 == 1`.
///
/// `Copy` and free of indirection on purpose: these are shipped to the audio
/// thread by value through a lock-free queue, and must never require an
/// allocation or a lock to read.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coefficients {
    pub b0: f32,
    pub b1: f32,
    pub b2: f32,
    pub a1: f32,
    pub a2: f32,
}

impl Default for Coefficients {
    fn default() -> Self {
        Self::identity()
    }
}

impl Coefficients {
    /// A filter that passes its input through untouched.
    pub const fn identity() -> Self {
        Self {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
        }
    }

    /// Whether this section does nothing, so the bank can skip it entirely.
    ///
    /// A ten-band equalizer sitting at flat is the common case, and skipping a
    /// band costs one comparison against the five multiply-adds it saves.
    pub fn is_identity(&self) -> bool {
        self.b0 == 1.0 && self.b1 == 0.0 && self.b2 == 0.0 && self.a1 == 0.0 && self.a2 == 0.0
    }

    /// A peaking (bell) filter: `gain_db` at `freq`, tapering either side.
    pub fn peaking(freq: f32, q: f32, gain_db: f32, sample_rate: f32) -> Self {
        if gain_db.abs() < f32::EPSILON {
            return Self::identity();
        }

        let Some(w0) = angular_frequency(freq, sample_rate) else {
            return Self::identity();
        };

        // `A` is the *amplitude* half-gain: the cookbook's peaking form applies
        // A to the numerator and 1/A to the denominator, which together give
        // the full requested gain at the centre.
        let a = amplitude(gain_db);
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q.max(0.05));

        Self::normalised(
            1.0 + alpha * a,
            -2.0 * cos,
            1.0 - alpha * a,
            1.0 + alpha / a,
            -2.0 * cos,
            1.0 - alpha / a,
        )
    }

    /// A low shelf: `gain_db` below `freq`, unity above.
    ///
    /// The bottom band of a graphic equalizer is a shelf rather than a bell
    /// because "more bass" means *all* of the bottom end, not a narrow bump at
    /// 32 Hz that most speakers cannot reproduce anyway.
    pub fn low_shelf(freq: f32, slope: f32, gain_db: f32, sample_rate: f32) -> Self {
        if gain_db.abs() < f32::EPSILON {
            return Self::identity();
        }

        let Some(w0) = angular_frequency(freq, sample_rate) else {
            return Self::identity();
        };

        let a = amplitude(gain_db);
        let (sin, cos) = w0.sin_cos();
        let alpha = shelf_alpha(sin, a, slope);
        let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;

        Self::normalised(
            a * ((a + 1.0) - (a - 1.0) * cos + two_sqrt_a_alpha),
            2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
            a * ((a + 1.0) - (a - 1.0) * cos - two_sqrt_a_alpha),
            (a + 1.0) + (a - 1.0) * cos + two_sqrt_a_alpha,
            -2.0 * ((a - 1.0) + (a + 1.0) * cos),
            (a + 1.0) + (a - 1.0) * cos - two_sqrt_a_alpha,
        )
    }

    /// A high shelf: `gain_db` above `freq`, unity below.
    pub fn high_shelf(freq: f32, slope: f32, gain_db: f32, sample_rate: f32) -> Self {
        if gain_db.abs() < f32::EPSILON {
            return Self::identity();
        }

        let Some(w0) = angular_frequency(freq, sample_rate) else {
            return Self::identity();
        };

        let a = amplitude(gain_db);
        let (sin, cos) = w0.sin_cos();
        let alpha = shelf_alpha(sin, a, slope);
        let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;

        Self::normalised(
            a * ((a + 1.0) + (a - 1.0) * cos + two_sqrt_a_alpha),
            -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
            a * ((a + 1.0) + (a - 1.0) * cos - two_sqrt_a_alpha),
            (a + 1.0) - (a - 1.0) * cos + two_sqrt_a_alpha,
            2.0 * ((a - 1.0) - (a + 1.0) * cos),
            (a + 1.0) - (a - 1.0) * cos - two_sqrt_a_alpha,
        )
    }

    /// Divide through by `a0`, refusing anything that is not finite.
    fn normalised(b0: f32, b1: f32, b2: f32, a0: f32, a1: f32, a2: f32) -> Self {
        if a0.abs() < 1e-12 {
            return Self::identity();
        }

        let coefficients = Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
        };

        // A non-finite coefficient would put NaN into the output and, because
        // this is an IIR filter, keep it there for the rest of the session.
        // Falling back to a pass-through is always safe.
        if coefficients.is_finite() && coefficients.is_stable() {
            coefficients
        } else {
            Self::identity()
        }
    }

    fn is_finite(&self) -> bool {
        self.b0.is_finite()
            && self.b1.is_finite()
            && self.b2.is_finite()
            && self.a1.is_finite()
            && self.a2.is_finite()
    }

    /// Whether both poles lie inside the unit circle.
    ///
    /// For a second-order section this is Jury's criterion, and it is two
    /// comparisons. An unstable section does not merely sound wrong: it grows
    /// without bound until the output is a full-scale square wave.
    pub fn is_stable(&self) -> bool {
        self.a2.abs() < 1.0 && self.a1.abs() < 1.0 + self.a2
    }

    /// Magnitude response at `freq`, in decibels.
    ///
    /// Evaluated as `|H(e^jw)|` directly from the coefficients, so it describes
    /// the filter that is actually running rather than the one that was asked
    /// for. This is what the equalizer curve is drawn from.
    pub fn magnitude_db(&self, freq: f32, sample_rate: f32) -> f32 {
        let w = 2.0 * PI * freq / sample_rate;

        // z^-1 and z^-2 on the unit circle.
        let (sin1, cos1) = w.sin_cos();
        let (sin2, cos2) = (2.0 * w).sin_cos();

        let num_real = self.b0 + self.b1 * cos1 + self.b2 * cos2;
        let num_imag = -(self.b1 * sin1 + self.b2 * sin2);
        let den_real = 1.0 + self.a1 * cos1 + self.a2 * cos2;
        let den_imag = -(self.a1 * sin1 + self.a2 * sin2);

        let num = (num_real * num_real + num_imag * num_imag).sqrt();
        let den = (den_real * den_real + den_imag * den_imag).sqrt();

        if den < 1e-20 {
            return 0.0;
        }

        20.0 * (num / den).max(1e-9).log10()
    }
}

/// Per-channel filter memory.
///
/// Transposed direct form II: two state variables, and the numerically best
/// behaved of the four standard forms in single precision — which matters here
/// because these run for hours without being reset.
#[derive(Debug, Clone, Copy, Default)]
pub struct State {
    s1: f32,
    s2: f32,
}

impl State {
    pub const fn new() -> Self {
        Self { s1: 0.0, s2: 0.0 }
    }

    /// Filter one sample.
    #[inline]
    pub fn process(&mut self, input: f32, c: &Coefficients) -> f32 {
        let output = c.b0 * input + self.s1;
        self.s1 = c.b1 * input - c.a1 * output + self.s2;
        self.s2 = c.b2 * input - c.a2 * output;
        output
    }

    /// Forget the past. Used on seek and track change, where the previous
    /// audio has nothing to do with what comes next.
    pub fn reset(&mut self) {
        self.s1 = 0.0;
        self.s2 = 0.0;
    }

    /// Whether the state has gone non-finite and needs clearing.
    ///
    /// Denormals and NaN both get in through pathological input; an IIR filter
    /// will happily keep either forever.
    pub fn is_healthy(&self) -> bool {
        self.s1.is_finite() && self.s2.is_finite()
    }
}

/// Convert a decibel gain to the cookbook's `A` term.
fn amplitude(gain_db: f32) -> f32 {
    10.0_f32.powf(gain_db / 40.0)
}

/// `alpha` for the shelving forms, at shelf slope `slope`.
fn shelf_alpha(sin: f32, a: f32, slope: f32) -> f32 {
    let slope = slope.clamp(0.1, 2.0);
    sin / 2.0 * ((a + 1.0 / a) * (1.0 / slope - 1.0) + 2.0).max(0.0).sqrt()
}

/// Normalised angular frequency, or `None` if the frequency is unusable.
///
/// A band above the Nyquist frequency cannot be realised — at 44.1 kHz the
/// 16 kHz band is fine but a 24 kHz one would alias into nonsense — so it
/// becomes a pass-through rather than a wrong filter.
fn angular_frequency(freq: f32, sample_rate: f32) -> Option<f32> {
    if !(freq.is_finite() && sample_rate.is_finite()) || freq <= 0.0 || sample_rate <= 0.0 {
        return None;
    }
    // Leave a little headroom below Nyquist: the cookbook forms degenerate as
    // w0 approaches pi.
    if freq >= sample_rate * 0.495 {
        return None;
    }
    Some(2.0 * PI * freq / sample_rate)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f32 = 48_000.0;

    /// Drive a filter with a sine and measure how much it changed the level.
    ///
    /// This is the check that matters: it measures the *implementation*, not
    /// the coefficient formula, so a mistake in the difference equation shows
    /// up here even when the analytic response is perfect.
    fn measured_gain_db(c: &Coefficients, freq: f32) -> f32 {
        let mut state = State::new();

        // Long enough for the transient to die away completely.
        let settle = 20_000;
        let measure = 40_000;

        let mut sum_in = 0.0_f64;
        let mut sum_out = 0.0_f64;

        for n in 0..(settle + measure) {
            let phase = 2.0 * PI * freq * n as f32 / RATE;
            let input = phase.sin();
            let output = state.process(input, c);

            if n >= settle {
                sum_in += f64::from(input) * f64::from(input);
                sum_out += f64::from(output) * f64::from(output);
            }
        }

        assert!(state.is_healthy(), "filter state went non-finite");
        20.0 * (sum_out.sqrt() / sum_in.sqrt()).log10() as f32
    }

    #[test]
    fn a_peaking_filter_hits_its_requested_gain_at_the_centre() {
        for gain_db in [-12.0, -6.0, -3.0, 3.0, 6.0, 12.0] {
            for freq in [63.0, 500.0, 4_000.0, 12_000.0] {
                let c = Coefficients::peaking(freq, 1.41, gain_db, RATE);
                let analytic = c.magnitude_db(freq, RATE);
                assert!(
                    (analytic - gain_db).abs() < 0.01,
                    "{freq} Hz at {gain_db} dB: analytic response was {analytic}"
                );
            }
        }
    }

    /// The plan's acceptance criterion: measured response within 0.5 dB of the
    /// analytic curve. It holds far tighter than that.
    #[test]
    fn the_measured_response_matches_the_analytic_curve() {
        let cases = [
            (Coefficients::peaking(1_000.0, 1.41, 6.0, RATE), 1_000.0),
            (Coefficients::peaking(1_000.0, 1.41, -6.0, RATE), 1_000.0),
            (Coefficients::peaking(250.0, 1.41, 12.0, RATE), 250.0),
            (Coefficients::peaking(250.0, 1.41, 12.0, RATE), 1_000.0),
            (Coefficients::low_shelf(63.0, 1.0, 9.0, RATE), 30.0),
            (Coefficients::high_shelf(8_000.0, 1.0, -9.0, RATE), 16_000.0),
        ];

        for (c, freq) in cases {
            let analytic = c.magnitude_db(freq, RATE);
            let measured = measured_gain_db(&c, freq);
            assert!(
                (analytic - measured).abs() < 0.1,
                "at {freq} Hz: analytic {analytic:.3} dB but measured {measured:.3} dB"
            );
        }
    }

    /// A shelf has to lift the whole band below it, not just its corner.
    #[test]
    fn a_low_shelf_lifts_everything_beneath_it() {
        let c = Coefficients::low_shelf(100.0, 1.0, 10.0, RATE);

        // Deep in the shelf, the full gain.
        assert!((c.magnitude_db(10.0, RATE) - 10.0).abs() < 0.5);
        assert!((c.magnitude_db(20.0, RATE) - 10.0).abs() < 0.6);
        // At the corner, half of it, by construction.
        assert!((c.magnitude_db(100.0, RATE) - 5.0).abs() < 0.5);
        // Well above, untouched.
        assert!(c.magnitude_db(4_000.0, RATE).abs() < 0.3);
    }

    #[test]
    fn a_high_shelf_lifts_everything_above_it() {
        let c = Coefficients::high_shelf(8_000.0, 1.0, 10.0, RATE);

        assert!(c.magnitude_db(200.0, RATE).abs() < 0.3);
        assert!((c.magnitude_db(8_000.0, RATE) - 5.0).abs() < 0.5);
        assert!((c.magnitude_db(20_000.0, RATE) - 10.0).abs() < 0.6);
    }

    /// A peaking filter must not touch frequencies far from its centre, or ten
    /// of them stacked would colour everything.
    #[test]
    fn a_peaking_filter_leaves_distant_frequencies_alone() {
        let c = Coefficients::peaking(1_000.0, 1.41, 12.0, RATE);
        assert!(c.magnitude_db(50.0, RATE).abs() < 0.5);
        assert!(c.magnitude_db(16_000.0, RATE).abs() < 0.5);
    }

    #[test]
    fn zero_gain_is_a_pass_through() {
        let c = Coefficients::peaking(1_000.0, 1.41, 0.0, RATE);
        assert!(c.is_identity());

        let mut state = State::new();
        for n in 0..1_000 {
            let input = (n as f32 * 0.01).sin();
            assert_eq!(state.process(input, &c), input);
        }
    }

    /// An unstable section does not sound bad, it destroys the output. Every
    /// setting the UI can produce has to stay inside the unit circle.
    #[test]
    fn every_reachable_setting_is_stable() {
        for rate in [8_000.0, 44_100.0, 48_000.0, 96_000.0, 192_000.0] {
            for freq in super::super::eq::BAND_FREQUENCIES {
                for gain_db in [-12.0, -6.0, 0.0, 6.0, 12.0] {
                    for q in [0.5, 1.41, 4.0] {
                        let c = Coefficients::peaking(freq, q, gain_db, rate);
                        assert!(
                            c.is_stable(),
                            "unstable at {freq} Hz, {gain_db} dB, Q {q}, rate {rate}"
                        );
                    }
                }
            }
        }
    }

    /// Bands above Nyquist cannot be realised; they must become pass-throughs
    /// rather than filters with meaningless coefficients.
    #[test]
    fn a_band_above_nyquist_is_disabled_not_mangled() {
        // 16 kHz at an 8 kHz sample rate is well past Nyquist.
        let c = Coefficients::peaking(16_000.0, 1.41, 12.0, 8_000.0);
        assert!(c.is_identity());

        // ...but it is perfectly realisable at 48 kHz.
        let c = Coefficients::peaking(16_000.0, 1.41, 12.0, 48_000.0);
        assert!(!c.is_identity());
        assert!(c.is_stable());
    }

    #[test]
    fn nonsense_inputs_fall_back_to_a_pass_through() {
        assert!(Coefficients::peaking(f32::NAN, 1.41, 6.0, RATE).is_identity());
        assert!(Coefficients::peaking(-100.0, 1.41, 6.0, RATE).is_identity());
        assert!(Coefficients::peaking(1_000.0, 1.41, 6.0, 0.0).is_identity());
        assert!(Coefficients::low_shelf(1_000.0, 0.0, 6.0, RATE).is_finite());
    }

    /// Resetting has to actually clear the tail, or a seek carries a fragment
    /// of the previous position into the new one.
    #[test]
    fn resetting_clears_the_filter_tail() {
        let c = Coefficients::peaking(1_000.0, 1.41, 12.0, RATE);
        let mut state = State::new();

        for _ in 0..100 {
            state.process(1.0, &c);
        }
        state.reset();

        // With no history, the first sample of silence must produce silence.
        assert_eq!(state.process(0.0, &c), 0.0);
    }
}
