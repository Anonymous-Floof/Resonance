//! The ten-band graphic equalizer.
//!
//! Ten cascaded biquads per channel. The bands are the ISO octave centres, and
//! the outer two are shelves rather than bells: "more bass" should mean all of
//! the bottom end, not a narrow bump at 32 Hz that most speakers cannot
//! reproduce, and the same reasoning applies at the top.
//!
//! A [`Bank`] is built on the control thread and is `Copy`; a [`BankState`]
//! lives on the audio thread and holds the filter memory. Keeping them apart is
//! what lets coefficients be replaced mid-stream without disturbing the filter
//! history, which is what makes a slider drag sound like a slider drag instead
//! of a series of clicks.

use crate::dsp::biquad::{Coefficients, State};

/// ISO octave-band centre frequencies, in hertz.
pub const BAND_FREQUENCIES: [f32; 10] = [
    31.5, 63.0, 125.0, 250.0, 500.0, 1_000.0, 2_000.0, 4_000.0, 8_000.0, 16_000.0,
];

pub const BAND_COUNT: usize = BAND_FREQUENCIES.len();

/// Q for the eight bell bands.
///
/// √2 gives each band roughly one octave of bandwidth, so ten of them tile the
/// spectrum with mild overlap: enough that the composite curve is smooth,
/// little enough that one slider does not swamp its neighbours.
pub const DEFAULT_Q: f32 = 1.41;

/// Shelf slope for the outer two bands. 1.0 is the gentlest non-resonant slope.
pub const SHELF_SLOPE: f32 = 1.0;

/// Most channels the equalizer will filter.
///
/// The test machine's output is 8-channel. Anything beyond this passes through
/// unfiltered rather than being dropped — an unequalized channel is a far
/// smaller problem than a silent one.
pub const MAX_CHANNELS: usize = 8;

/// Human-readable labels for the band frequencies.
pub fn band_label(index: usize) -> String {
    let freq = BAND_FREQUENCIES.get(index).copied().unwrap_or(0.0);
    if freq >= 1_000.0 {
        let k = freq / 1_000.0;
        if (k - k.round()).abs() < 0.05 {
            format!("{}k", k.round() as u32)
        } else {
            format!("{k:.1}k")
        }
    } else if (freq - freq.round()).abs() < 0.05 {
        format!("{}", freq.round() as u32)
    } else {
        format!("{freq:.1}")
    }
}

/// A complete set of equalizer coefficients, ready for the audio thread.
#[derive(Debug, Clone, Copy)]
pub struct Bank {
    bands: [Coefficients; BAND_COUNT],
    /// Linear gain applied before the filters.
    preamp: f32,
    /// False when the whole bank should be bypassed.
    enabled: bool,
    /// Rate these coefficients were computed for, so a device change can be
    /// detected rather than silently detuning every band.
    sample_rate: f32,
}

impl Default for Bank {
    fn default() -> Self {
        Self::bypassed()
    }
}

impl Bank {
    /// A bank that does nothing.
    pub fn bypassed() -> Self {
        Self {
            bands: [Coefficients::identity(); BAND_COUNT],
            preamp: 1.0,
            enabled: false,
            sample_rate: 0.0,
        }
    }

    /// Build from the user's settings.
    ///
    /// Runs on the control thread: forty transcendental functions, which is
    /// nothing there and unacceptable in the callback.
    pub fn new(gains_db: &[f32], preamp_db: f32, sample_rate: f32, enabled: bool) -> Self {
        if !enabled || sample_rate <= 0.0 {
            return Self::bypassed();
        }

        let mut bands = [Coefficients::identity(); BAND_COUNT];

        for (index, coefficients) in bands.iter_mut().enumerate() {
            let freq = BAND_FREQUENCIES[index];
            // A settings file with too few bands is honoured as far as it goes
            // rather than rejected; the rest stay flat.
            let gain_db = gains_db.get(index).copied().unwrap_or(0.0);

            *coefficients = match index {
                0 => Coefficients::low_shelf(freq, SHELF_SLOPE, gain_db, sample_rate),
                index if index == BAND_COUNT - 1 => {
                    Coefficients::high_shelf(freq, SHELF_SLOPE, gain_db, sample_rate)
                }
                _ => Coefficients::peaking(freq, DEFAULT_Q, gain_db, sample_rate),
            };
        }

        Self {
            bands,
            preamp: db_to_linear(preamp_db),
            enabled,
            sample_rate,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn sample_rate(&self) -> f32 {
        self.sample_rate
    }

    pub fn preamp(&self) -> f32 {
        self.preamp
    }

    pub fn band(&self, index: usize) -> Coefficients {
        self.bands.get(index).copied().unwrap_or_default()
    }

    /// Whether every band is flat, so the audio thread can skip the bank.
    pub fn is_flat(&self) -> bool {
        !self.enabled || self.bands.iter().all(Coefficients::is_identity)
    }

    /// The composite response at `freq`, in decibels, preamp included.
    ///
    /// This is what the equalizer view draws. It is derived from the same
    /// coefficients the audio thread runs, so the curve cannot claim something
    /// the sound does not do.
    pub fn response_db(&self, freq: f32) -> f32 {
        if !self.enabled || self.sample_rate <= 0.0 {
            return 0.0;
        }

        let bands: f32 = self
            .bands
            .iter()
            .map(|c| c.magnitude_db(freq, self.sample_rate))
            .sum();

        bands + linear_to_db(self.preamp)
    }

    /// The largest boost anywhere in the audible range, in decibels.
    ///
    /// Used to suggest a preamp that keeps a boosted curve from clipping.
    pub fn peak_gain_db(&self) -> f32 {
        if !self.enabled {
            return 0.0;
        }

        // Sampled logarithmically: the curve is smooth, and this is only ever
        // called when a slider moves.
        let mut peak = f32::NEG_INFINITY;
        for step in 0..=120 {
            let freq = 20.0 * 10.0_f32.powf(step as f32 / 40.0);
            if freq >= self.sample_rate * 0.5 {
                break;
            }
            peak = peak.max(self.response_db(freq));
        }

        if peak.is_finite() { peak } else { 0.0 }
    }

    /// A preamp that would bring the loudest point back to unity.
    ///
    /// Boosting four bands by 12 dB and wondering why it distorts is the single
    /// most common equalizer complaint; this is the number that fixes it.
    pub fn suggested_preamp_db(&self) -> f32 {
        (-self.peak_gain_db()).min(0.0)
    }
}

/// Per-channel filter memory for a [`Bank`].
///
/// Fixed size and allocated once, because this lives on the audio thread.
#[derive(Debug)]
pub struct BankState {
    states: [[State; BAND_COUNT]; MAX_CHANNELS],
}

impl Default for BankState {
    fn default() -> Self {
        Self::new()
    }
}

impl BankState {
    pub const fn new() -> Self {
        Self {
            states: [[State::new(); BAND_COUNT]; MAX_CHANNELS],
        }
    }

    /// Filter one sample of one channel.
    ///
    /// Bands that are flat are skipped, so a bypassed or partly-used equalizer
    /// costs a handful of comparisons rather than ten biquads.
    #[inline]
    pub fn process(&mut self, channel: usize, input: f32, bank: &Bank) -> f32 {
        if channel >= MAX_CHANNELS {
            return input;
        }

        let states = &mut self.states[channel];
        let mut sample = input;

        for (state, coefficients) in states.iter_mut().zip(bank.bands.iter()) {
            if coefficients.is_identity() {
                continue;
            }
            sample = state.process(sample, coefficients);
        }

        sample
    }

    /// Clear every band on every channel.
    ///
    /// Called on seek and track change: the filter tail belongs to audio that
    /// is no longer playing, and letting it ring into the new position is
    /// audible as a click.
    pub fn reset(&mut self) {
        for channel in &mut self.states {
            for state in channel {
                state.reset();
            }
        }
    }

    /// Clear any channel whose state has gone non-finite.
    ///
    /// A single NaN reaching an IIR filter would otherwise silence that channel
    /// permanently, and the user would have to restart the app to get it back.
    pub fn sanitise(&mut self) -> bool {
        let mut repaired = false;
        for channel in &mut self.states {
            for state in channel {
                if !state.is_healthy() {
                    state.reset();
                    repaired = true;
                }
            }
        }
        repaired
    }
}

/// Decibels to a linear amplitude multiplier.
pub fn db_to_linear(db: f32) -> f32 {
    if db.abs() < f32::EPSILON {
        return 1.0;
    }
    10.0_f32.powf(db / 20.0)
}

/// Linear amplitude multiplier to decibels.
pub fn linear_to_db(linear: f32) -> f32 {
    if linear <= 1e-9 {
        return -180.0;
    }
    20.0 * linear.log10()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f32 = 48_000.0;

    fn gains(values: &[(usize, f32)]) -> Vec<f32> {
        let mut out = vec![0.0; BAND_COUNT];
        for (index, gain) in values {
            out[*index] = *gain;
        }
        out
    }

    #[test]
    fn a_disabled_bank_is_a_pass_through() {
        let bank = Bank::new(&gains(&[(3, 12.0)]), 6.0, RATE, false);
        assert!(bank.is_flat());
        assert_eq!(bank.response_db(1_000.0), 0.0);

        let mut state = BankState::new();
        for n in 0..500 {
            let input = (n as f32 * 0.1).sin();
            assert_eq!(state.process(0, input, &bank), input);
        }
    }

    /// The headline property: a slider set to N dB produces N dB at that band.
    #[test]
    fn each_band_delivers_its_gain_at_its_own_frequency() {
        for (index, freq) in BAND_FREQUENCIES.iter().enumerate() {
            let bank = Bank::new(&gains(&[(index, 6.0)]), 0.0, RATE, true);
            let response = bank.response_db(*freq);

            // The outer bands are shelves, which by construction sit at half
            // their gain at the corner frequency.
            let expected = if index == 0 || index == BAND_COUNT - 1 {
                3.0
            } else {
                6.0
            };

            assert!(
                (response - expected).abs() < 0.6,
                "band {index} ({freq} Hz) responded {response:.2} dB, expected {expected}"
            );
        }
    }

    /// Neighbouring bands overlap a little, but one slider must not visibly
    /// move a band two steps away.
    #[test]
    fn a_band_stays_out_of_its_distant_neighbours() {
        let bank = Bank::new(&gains(&[(5, 12.0)]), 0.0, RATE, true);

        assert!((bank.response_db(1_000.0) - 12.0).abs() < 0.5);
        // Two octaves down and up.
        assert!(bank.response_db(250.0).abs() < 1.5);
        assert!(bank.response_db(4_000.0).abs() < 1.5);
    }

    #[test]
    fn the_preamp_shifts_the_whole_curve() {
        let flat = Bank::new(&gains(&[]), 0.0, RATE, true);
        let quiet = Bank::new(&gains(&[]), -6.0, RATE, true);

        for freq in [50.0, 500.0, 5_000.0] {
            assert!((flat.response_db(freq)).abs() < 0.01);
            assert!((quiet.response_db(freq) + 6.0).abs() < 0.05);
        }
    }

    /// The measured signal has to agree with the drawn curve; otherwise the
    /// equalizer view is decoration.
    #[test]
    fn the_drawn_curve_matches_what_the_filters_actually_do() {
        let bank = Bank::new(&gains(&[(5, 9.0), (2, -6.0)]), 0.0, RATE, true);

        for freq in [125.0, 1_000.0, 3_000.0] {
            let predicted = bank.response_db(freq);
            let measured = measure(&bank, freq);
            assert!(
                (predicted - measured).abs() < 0.5,
                "at {freq} Hz the curve says {predicted:.2} dB but the filters did {measured:.2} dB"
            );
        }
    }

    /// Run a sine through the whole bank and measure the level change.
    fn measure(bank: &Bank, freq: f32) -> f32 {
        let mut state = BankState::new();
        let settle = 30_000;
        let measure = 60_000;

        let mut sum_in = 0.0_f64;
        let mut sum_out = 0.0_f64;

        for n in 0..(settle + measure) {
            let phase = 2.0 * std::f32::consts::PI * freq * n as f32 / RATE;
            let input = phase.sin();
            let output = state.process(0, input * bank.preamp(), bank);

            if n >= settle {
                sum_in += f64::from(input) * f64::from(input);
                sum_out += f64::from(output) * f64::from(output);
            }
        }

        20.0 * (sum_out.sqrt() / sum_in.sqrt()).log10() as f32
    }

    /// Channels must not bleed into each other: the equalizer keeps separate
    /// filter memory per channel, and getting that wrong collapses stereo.
    #[test]
    fn channels_are_filtered_independently() {
        let bank = Bank::new(&gains(&[(5, 12.0)]), 0.0, RATE, true);
        let mut shared = BankState::new();

        // Feed channel 0 a signal and channel 1 silence.
        let mut channel_one_output = 0.0_f32;
        for n in 0..1_000 {
            let input = (n as f32 * 0.05).sin();
            shared.process(0, input, &bank);
            channel_one_output += shared.process(1, 0.0, &bank).abs();
        }

        assert_eq!(
            channel_one_output, 0.0,
            "silence on one channel came out non-silent"
        );
    }

    #[test]
    fn resetting_clears_every_band_and_channel() {
        let bank = Bank::new(&gains(&[(4, 12.0)]), 0.0, RATE, true);
        let mut state = BankState::new();

        for channel in 0..MAX_CHANNELS {
            for _ in 0..200 {
                state.process(channel, 1.0, &bank);
            }
        }
        state.reset();

        for channel in 0..MAX_CHANNELS {
            assert_eq!(state.process(channel, 0.0, &bank), 0.0);
        }
    }

    /// A NaN in an IIR filter is permanent until the state is cleared. It must
    /// be detected and repaired rather than silencing a channel for the rest of
    /// the session.
    #[test]
    fn a_poisoned_channel_is_repaired() {
        let bank = Bank::new(&gains(&[(4, 12.0)]), 0.0, RATE, true);
        let mut state = BankState::new();

        state.process(0, f32::NAN, &bank);
        assert!(
            state.process(0, 1.0, &bank).is_nan(),
            "NaN should propagate"
        );

        assert!(state.sanitise(), "the poisoned state should be detected");
        assert!(state.process(0, 1.0, &bank).is_finite());
    }

    #[test]
    fn extra_channels_pass_through_rather_than_going_silent() {
        let bank = Bank::new(&gains(&[(4, 12.0)]), 0.0, RATE, true);
        let mut state = BankState::new();
        assert_eq!(state.process(MAX_CHANNELS + 3, 0.5, &bank), 0.5);
    }

    /// The number the UI offers when a boosted curve would clip.
    #[test]
    fn a_boosted_curve_suggests_a_preamp_that_undoes_it() {
        let bank = Bank::new(&gains(&[(2, 12.0), (3, 12.0)]), 0.0, RATE, true);

        let peak = bank.peak_gain_db();
        assert!(peak > 12.0, "stacked bands should exceed one band's gain");

        let suggested = bank.suggested_preamp_db();
        assert!((suggested + peak).abs() < 0.01);
        assert!(suggested < 0.0);
    }

    /// A flat curve needs no correction; suggesting one would be noise.
    #[test]
    fn a_flat_curve_suggests_no_preamp() {
        let bank = Bank::new(&gains(&[]), 0.0, RATE, true);
        assert_eq!(bank.suggested_preamp_db(), 0.0);
    }

    /// A cut-only curve must not suggest a positive preamp: raising the level
    /// to compensate for a deliberate cut is not what anyone asked for.
    #[test]
    fn a_cut_curve_does_not_suggest_a_boost() {
        let bank = Bank::new(&gains(&[(4, -12.0)]), 0.0, RATE, true);
        assert!(bank.suggested_preamp_db() <= 0.0);
    }

    #[test]
    fn band_labels_read_the_way_a_person_would_write_them() {
        assert_eq!(band_label(0), "31.5");
        assert_eq!(band_label(1), "63");
        assert_eq!(band_label(5), "1k");
        assert_eq!(band_label(9), "16k");
    }

    #[test]
    fn decibel_conversion_round_trips() {
        for db in [-24.0, -6.0, 0.0, 6.0, 12.0] {
            let round_tripped = linear_to_db(db_to_linear(db));
            assert!(
                (round_tripped - db).abs() < 0.001,
                "{db} became {round_tripped}"
            );
        }
        assert_eq!(db_to_linear(0.0), 1.0);
    }

    /// A settings file from a build with fewer bands must still load.
    #[test]
    fn a_short_gain_list_leaves_the_rest_flat() {
        let bank = Bank::new(&[6.0, 6.0], 0.0, RATE, true);
        assert!(bank.response_db(50.0) > 1.0);
        assert!(bank.response_db(8_000.0).abs() < 0.5);
    }
}
