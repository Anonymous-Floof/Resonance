//! The complete signal chain, and the fade envelope that keeps it quiet.
//!
//! [`Chain`] lives on the audio thread and owns every piece of mutable filter
//! state. [`Params`] is built on the control thread and handed over by value.
//! Nothing here allocates, locks, or calls a transcendental function.

use crate::dsp::eq::{Bank, BankState};
use crate::dsp::limiter::{Limiter, Settings as LimiterSettings};
use crate::viz::Tap;

/// How long a pause, resume, seek or track change is ramped over.
///
/// Long enough to remove the step discontinuity that causes a click, short
/// enough that pressing pause still feels instant. Ten milliseconds is roughly
/// the shortest gap the ear reads as "stopped" rather than "glitched".
pub const FADE_SECS: f32 = 0.010;

/// How long a coefficient change is crossfaded over.
///
/// Swapping biquad coefficients underneath a running filter changes its
/// transfer function between one sample and the next, which is heard as a zip
/// or a click while dragging a slider. Crossfading two banks costs a second set
/// of filters for twenty milliseconds and removes the artefact entirely.
pub const COEFFICIENT_FADE_SECS: f32 = 0.020;

/// Everything the chain needs, computed on the control thread.
#[derive(Debug, Clone, Copy)]
pub struct Params {
    pub bank: Bank,
    pub limiter: LimiterSettings,
    /// Linear gain from the track's ReplayGain tags, or 1.0.
    pub replay_gain: f32,
    /// Per-sample smoothing coefficient for the volume control.
    pub volume_smoothing: f32,
    /// Per-sample step for the fade envelope.
    pub fade_step: f32,
    /// Per-sample step for a coefficient crossfade.
    pub coefficient_step: f32,
}

impl Default for Params {
    fn default() -> Self {
        Self::for_rate(48_000.0)
    }
}

impl Params {
    /// Neutral parameters for a given sample rate.
    pub fn for_rate(sample_rate: f32) -> Self {
        let rate = if sample_rate > 0.0 {
            sample_rate
        } else {
            48_000.0
        };
        Self {
            bank: Bank::bypassed(),
            limiter: LimiterSettings::bypassed(),
            replay_gain: 1.0,
            volume_smoothing: volume_smoothing_for(rate),
            fade_step: 1.0 / (rate * FADE_SECS).max(1.0),
            coefficient_step: 1.0 / (rate * COEFFICIENT_FADE_SECS).max(1.0),
        }
    }
}

/// One-pole coefficient for the volume control, ~15 ms.
fn volume_smoothing_for(sample_rate: f32) -> f32 {
    (1.0 - (-1.0 / (sample_rate * 0.015)).exp()).clamp(0.0, 1.0)
}

/// Why the chain is fading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fade {
    /// Ramping to silence.
    Out,
    /// Ramping back up.
    In,
}

/// The audio thread's signal chain.
pub struct Chain {
    /// The bank currently in use, with its filter memory.
    active: (Bank, BankState),
    /// A newly-arrived bank being crossfaded in.
    incoming: Option<(Bank, BankState)>,
    /// 0.0 = fully active bank, 1.0 = fully incoming.
    blend: f32,

    limiter: Limiter,

    /// Smoothed volume, tracking its target rather than jumping to it.
    volume: f32,
    /// Fade envelope, 0.0..=1.0.
    envelope: f32,
    fade: Fade,

    params: Params,

    /// Where the visualisers read from, when one is running.
    ///
    /// `None` costs a branch per frame and nothing else, so the tap is only
    /// attached while something is actually drawing.
    tap: Option<Tap>,
}

impl Default for Chain {
    fn default() -> Self {
        Self::new()
    }
}

impl Chain {
    pub fn new() -> Self {
        Self {
            active: (Bank::bypassed(), BankState::new()),
            incoming: None,
            blend: 0.0,
            limiter: Limiter::new(),
            volume: 0.0,
            // Start faded out so the very first callback ramps up rather than
            // starting mid-waveform.
            envelope: 0.0,
            fade: Fade::In,
            params: Params::default(),
            tap: None,
        }
    }

    /// Attach or detach the visualiser tap.
    ///
    /// Returns whatever was attached before, so a caller rebuilding the output
    /// stream can decide whether to carry the old tap over or start a new one.
    pub fn set_tap(&mut self, tap: Option<Tap>) -> Option<Tap> {
        std::mem::replace(&mut self.tap, tap)
    }

    /// Replace the non-filter parameters.
    ///
    /// A new equalizer bank goes through [`Self::set_bank`] instead, because it
    /// needs to be crossfaded rather than swapped.
    pub fn set_params(&mut self, params: Params) {
        let bank = params.bank;
        self.params = params;
        self.set_bank(bank);
    }

    /// Begin crossfading to a new equalizer bank.
    pub fn set_bank(&mut self, bank: Bank) {
        // Nothing to crossfade to, and no point paying for one.
        if same_bank(&self.active.0, &bank) {
            return;
        }

        // A bank arriving while another is still fading: the one in flight
        // becomes the new starting point, so the blend never jumps backwards.
        if self.blend >= 1.0
            && let Some(incoming) = self.incoming.take()
        {
            self.active = incoming;
        }

        self.incoming = Some((bank, BankState::new()));
        self.blend = 0.0;
    }

    pub fn limiter(&self) -> &Limiter {
        &self.limiter
    }

    /// Start ramping down. Idempotent.
    pub fn fade_out(&mut self) {
        self.fade = Fade::Out;
    }

    /// Start ramping up. Idempotent.
    pub fn fade_in(&mut self) {
        self.fade = Fade::In;
    }

    /// Whether a fade-out has finished, so the caller can safely stop.
    pub fn is_silent(&self) -> bool {
        self.envelope <= 0.0
    }

    /// Forget everything that belongs to the audio just played.
    ///
    /// Filter tails, limiter reduction and the fade envelope all describe the
    /// previous position; carrying any of them across a seek is audible.
    pub fn reset(&mut self) {
        self.active.1.reset();
        if let Some((_, state)) = &mut self.incoming {
            state.reset();
        }
        self.limiter.reset();
        self.envelope = 0.0;
        self.fade = Fade::In;
    }

    /// Snap the volume to its target instead of gliding to it.
    ///
    /// Used when starting from silence, where gliding up from 0 would fade in
    /// the first fifteen milliseconds of every track a second time.
    pub fn prime_volume(&mut self, target: f32) {
        self.volume = target;
    }

    /// Run the chain over an interleaved block, in place.
    ///
    /// `target_volume` is read once per block rather than per sample: it comes
    /// from an atomic, and the smoothing makes the difference inaudible.
    pub fn process(&mut self, block: &mut [f32], channels: usize, target_volume: f32) {
        let channels = channels.max(1);
        if block.len() < channels {
            return;
        }

        let replay_gain = self.params.replay_gain;
        let smoothing = self.params.volume_smoothing;
        let fade_step = self.params.fade_step;
        let coefficient_step = self.params.coefficient_step;

        for frame in block.chunks_mut(channels) {
            // 1. Level correction from the track's own tags. Static for the
            //    block, so this is one multiply per sample.
            if replay_gain != 1.0 {
                for sample in frame.iter_mut() {
                    *sample *= replay_gain;
                }
            }

            // 2. Equalizer, crossfading if a new bank has arrived. Each bank's
            //    preamp is applied inside, so it crossfades along with the
            //    filtering rather than stepping the instant a preset changes.
            self.filter_frame(frame);

            // Advanced per frame, not per block: a per-block step is a small
            // discontinuity every buffer, which is exactly the artefact the
            // crossfade exists to remove.
            if self.incoming.is_some() {
                self.blend = (self.blend + coefficient_step).min(1.0);
            }

            // 3. Limiter, one gain across the frame.
            self.limiter.process(frame, &self.params.limiter);

            // 4. Volume, smoothed, and the fade envelope.
            self.volume += (target_volume - self.volume) * smoothing;

            self.envelope = match self.fade {
                Fade::In => (self.envelope + fade_step).min(1.0),
                Fade::Out => (self.envelope - fade_step).max(0.0),
            };

            // 5. Visualiser tap, taken here rather than after the volume
            //    control on purpose. Volume is a listening preference, not a
            //    property of the music: tapping after it would shrink the
            //    spectrum every time you turned the sound down, and undo the
            //    sensitivity the user had set. The fade envelope *is* applied,
            //    so pausing makes the display settle rather than freeze.
            if let Some(tap) = &mut self.tap {
                let envelope = self.envelope;
                tap.push_frame(frame, envelope);
            }

            let gain = self.volume * self.envelope;
            for sample in frame.iter_mut() {
                *sample *= gain;
            }
        }

        // Once the crossfade completes, the incoming bank becomes the active
        // one and the second set of filters stops running.
        if self.blend >= 1.0 {
            if let Some(incoming) = self.incoming.take() {
                self.active = incoming;
            }
            self.blend = 0.0;
        }

        // An IIR filter that has taken a NaN stays broken until it is cleared.
        // Checking once per block is cheap and makes that unrecoverable state
        // recoverable.
        if self.active.1.sanitise() {
            tracing::warn!("cleared a non-finite equalizer state");
        }
    }

    /// Apply the equalizer to one frame, blending banks if a fade is running.
    ///
    /// The preamp belongs to its bank and is applied here, inside the blend.
    /// Applying it outside would make a preset change step the level in a
    /// single sample even though the filtering itself crossfaded smoothly —
    /// which is audible as exactly the click this is meant to prevent.
    #[inline]
    fn filter_frame(&mut self, frame: &mut [f32]) {
        match &mut self.incoming {
            None => {
                let (bank, state) = &mut self.active;
                let preamp = bank.preamp();

                if bank.is_flat() {
                    if preamp != 1.0 {
                        for sample in frame.iter_mut() {
                            *sample *= preamp;
                        }
                    }
                    return;
                }

                for (channel, sample) in frame.iter_mut().enumerate() {
                    *sample = state.process(channel, *sample * preamp, bank);
                }
            }
            Some((new_bank, new_state)) => {
                let (old_bank, old_state) = &mut self.active;
                let blend = self.blend;
                let old_preamp = old_bank.preamp();
                let new_preamp = new_bank.preamp();

                // Both banks run over the same input and are mixed. The old one
                // has to keep running even near full blend, so its state is
                // current if another change arrives mid-fade.
                for (channel, sample) in frame.iter_mut().enumerate() {
                    let input = *sample;
                    let old = old_state.process(channel, input * old_preamp, old_bank);
                    let new = new_state.process(channel, input * new_preamp, new_bank);
                    *sample = old + (new - old) * blend;
                }
            }
        }
    }
}

/// Whether two banks would produce identical filtering.
fn same_bank(a: &Bank, b: &Bank) -> bool {
    if a.is_enabled() != b.is_enabled() || a.sample_rate() != b.sample_rate() {
        return false;
    }
    if a.preamp() != b.preamp() {
        return false;
    }
    (0..crate::dsp::eq::BAND_COUNT).all(|index| a.band(index) == b.band(index))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::eq::BAND_COUNT;

    const RATE: f32 = 48_000.0;
    const CHANNELS: usize = 2;

    fn params() -> Params {
        Params::for_rate(RATE)
    }

    /// Run `frames` of a constant signal and return the output.
    fn run(chain: &mut Chain, frames: usize, value: f32, volume: f32) -> Vec<f32> {
        let mut block = vec![value; frames * CHANNELS];
        chain.process(&mut block, CHANNELS, volume);
        block
    }

    /// The chain starts silent and ramps in, so the first callback after a
    /// track starts does not begin mid-waveform.
    #[test]
    fn playback_fades_in_rather_than_starting_at_full_level() {
        let mut chain = Chain::new();
        chain.set_params(params());
        chain.prime_volume(1.0);

        let out = run(&mut chain, 8, 1.0, 1.0);
        assert!(out[0] < 0.05, "the first sample should be near silent");
        assert!(out[out.len() - 1] > out[0], "and it should be rising");
    }

    /// A fade has to actually reach silence, or pause leaves a quiet tone.
    #[test]
    fn a_fade_out_reaches_true_silence() {
        let mut chain = Chain::new();
        chain.set_params(params());
        chain.prime_volume(1.0);

        // Get to full level first.
        run(&mut chain, 2_000, 1.0, 1.0);
        chain.fade_out();

        let fade_frames = (RATE * FADE_SECS) as usize + 16;
        let out = run(&mut chain, fade_frames, 1.0, 1.0);

        assert!(chain.is_silent());
        assert_eq!(out[out.len() - 1], 0.0);
    }

    /// The whole point of the envelope: no step discontinuity at a pause.
    #[test]
    fn pausing_produces_no_step_discontinuity() {
        let mut chain = Chain::new();
        chain.set_params(params());
        chain.prime_volume(1.0);

        run(&mut chain, 2_000, 1.0, 1.0);
        chain.fade_out();

        let out = run(&mut chain, (RATE * FADE_SECS) as usize + 16, 1.0, 1.0);

        // A click is a large sample-to-sample jump. A 10 ms ramp over a
        // full-scale signal moves by well under a thousandth per sample.
        for pair in out.chunks(CHANNELS).collect::<Vec<_>>().windows(2) {
            let step = (pair[1][0] - pair[0][0]).abs();
            assert!(step < 0.01, "envelope stepped by {step}");
        }
    }

    #[test]
    fn volume_glides_instead_of_jumping() {
        let mut chain = Chain::new();
        chain.set_params(params());
        chain.prime_volume(0.0);
        chain.fade_in();

        // Envelope up first, so it is not the thing being measured.
        run(&mut chain, 2_000, 1.0, 0.0);

        let out = run(&mut chain, 64, 1.0, 1.0);
        assert!(out[0] < 0.2, "volume should not jump to target instantly");
        assert!(out[out.len() - 1] > out[0]);
    }

    /// A flat, disabled chain must be transparent apart from the envelope.
    #[test]
    fn a_neutral_chain_changes_nothing() {
        let mut chain = Chain::new();
        chain.set_params(params());
        chain.prime_volume(1.0);

        // Let both the envelope and the volume settle.
        run(&mut chain, 4_000, 0.5, 1.0);

        let out = run(&mut chain, 16, 0.5, 1.0);
        for sample in out {
            assert!(
                (sample - 0.5).abs() < 1e-3,
                "a neutral chain altered the signal to {sample}"
            );
        }
    }

    /// Swapping coefficients mid-stream is the classic source of zipper noise.
    #[test]
    fn changing_the_equalizer_does_not_click() {
        let mut chain = Chain::new();
        chain.set_params(params());
        chain.prime_volume(1.0);
        run(&mut chain, 4_000, 0.0, 1.0);

        // A sine, so a discontinuity is obvious against a smooth waveform.
        let frames = 4_000;
        let mut block = Vec::with_capacity(frames * CHANNELS);
        for n in 0..frames {
            let value = (2.0 * std::f32::consts::PI * 220.0 * n as f32 / RATE).sin() * 0.5;
            block.push(value);
            block.push(value);
        }

        // Slam the equalizer from flat to a heavy boost part-way through.
        let mut boosted = params();
        boosted.bank = Bank::new(&[12.0; BAND_COUNT], 0.0, RATE, true);

        let split = (frames / 2) * CHANNELS;
        chain.process(&mut block[..split], CHANNELS, 1.0);
        chain.set_params(boosted);
        chain.process(&mut block[split..], CHANNELS, 1.0);

        // The largest legitimate step for a 220 Hz sine at this rate is small;
        // an uncrossfaded coefficient swap would produce a far bigger one.
        let max_step = block
            .chunks(CHANNELS)
            .collect::<Vec<_>>()
            .windows(2)
            .map(|w| (w[1][0] - w[0][0]).abs())
            .fold(0.0_f32, f32::max);

        assert!(
            max_step < 0.25,
            "the equalizer change produced a {max_step} step, which is a click"
        );
    }

    /// Setting the same bank twice must not restart the crossfade, or holding
    /// a slider still would keep the second filter bank running forever.
    #[test]
    fn resending_an_identical_bank_is_a_no_op() {
        let mut chain = Chain::new();
        let mut boosted = params();
        boosted.bank = Bank::new(&[6.0; BAND_COUNT], 0.0, RATE, true);

        chain.set_params(boosted);
        run(&mut chain, 8_000, 0.1, 1.0);
        assert!(
            chain.incoming.is_none(),
            "the crossfade should have finished"
        );

        chain.set_params(boosted);
        assert!(
            chain.incoming.is_none(),
            "an identical bank should not start a new crossfade"
        );
    }

    /// After a seek nothing from the old position may survive.
    #[test]
    fn resetting_clears_the_filter_tail_and_the_envelope() {
        let mut chain = Chain::new();
        let mut boosted = params();
        boosted.bank = Bank::new(&[12.0; BAND_COUNT], 0.0, RATE, true);
        chain.set_params(boosted);

        run(&mut chain, 8_000, 1.0, 1.0);
        chain.reset();

        assert!(chain.is_silent(), "the envelope should be back at zero");

        // Silence in, silence out - nothing ringing from before.
        let out = run(&mut chain, 1, 0.0, 1.0);
        assert_eq!(out[0], 0.0);
    }

    /// ReplayGain is a plain level change ahead of everything else.
    #[test]
    fn replay_gain_scales_the_input() {
        let mut chain = Chain::new();
        let mut quiet = params();
        quiet.replay_gain = 0.5;
        chain.set_params(quiet);
        chain.prime_volume(1.0);

        run(&mut chain, 4_000, 1.0, 1.0);
        let out = run(&mut chain, 8, 1.0, 1.0);

        for sample in out {
            assert!((sample - 0.5).abs() < 1e-3, "expected 0.5, got {sample}");
        }
    }

    /// The limiter has to be inside the chain, not merely available.
    #[test]
    fn the_chain_limits_what_the_equalizer_boosts() {
        let mut chain = Chain::new();
        let mut hot = params();
        hot.bank = Bank::new(&[12.0; BAND_COUNT], 0.0, RATE, true);
        hot.limiter = LimiterSettings::new(true, LimiterSettings::DEFAULT_CEILING_DB, RATE);
        chain.set_params(hot);
        chain.prime_volume(1.0);

        let ceiling = hot.limiter.ceiling;

        // Full-scale input through a +12 dB curve: without the limiter this
        // would be four times over.
        for _ in 0..40 {
            let out = run(&mut chain, 256, 0.9, 1.0);
            for sample in out {
                assert!(
                    sample.abs() <= ceiling + 1e-4,
                    "{sample} escaped the {ceiling} ceiling"
                );
            }
        }
    }

    /// Mono, 5.1 and 7.1 all have to work; the chain is indexed by channel.
    #[test]
    fn any_channel_count_is_handled() {
        for channels in [1, 2, 6, 8] {
            let mut chain = Chain::new();
            chain.set_params(params());
            chain.prime_volume(1.0);

            let mut block = vec![0.25; 512 * channels];
            chain.process(&mut block, channels, 1.0);

            assert!(block.iter().all(|s| s.is_finite()));
        }
    }

    /// A zero-length or partial block must not panic or index out of range.
    #[test]
    fn short_blocks_are_ignored_rather_than_panicking() {
        let mut chain = Chain::new();
        chain.set_params(params());

        chain.process(&mut [], 2, 1.0);
        chain.process(&mut [0.5], 2, 1.0);
    }
}
