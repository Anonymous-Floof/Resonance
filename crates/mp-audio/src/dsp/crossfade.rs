//! Blending the end of one track into the start of the next.
//!
//! The mixing itself is here, as plain functions over sample slices, so the
//! part that decides *what the audio should be* can be tested without a sound
//! device, a decoder, or a clock. What remains in the engine is the part that
//! can only be judged by ear: when to start, and where the two streams come
//! from.
//!
//! Gain is computed per frame rather than per sample so the channels of one
//! frame never drift apart — a stereo image that shears mid-fade is audible
//! even when the levels are right.

use mp_core::config::CrossfadeCurve;

/// Gains for the outgoing and incoming tracks at a point in the fade.
///
/// `progress` runs `0.0..=1.0` across the fade: at 0 the outgoing track is at
/// full level and the incoming one is silent, at 1 the reverse.
#[must_use]
pub fn gains(curve: CrossfadeCurve, progress: f32) -> (f32, f32) {
    let t = progress.clamp(0.0, 1.0);

    match curve {
        // Sums to 1.0 throughout. Correct for material that is correlated —
        // the same note held across a join — and dips in the middle for
        // anything that is not, which is most music.
        CrossfadeCurve::Linear => (1.0 - t, t),

        // Sums to 1.0 in *power* rather than amplitude, which is what keeps
        // perceived loudness steady across the blend. The usual choice, and
        // the default for that reason.
        CrossfadeCurve::EqualPower => {
            let angle = t * std::f32::consts::FRAC_PI_2;
            (angle.cos(), angle.sin())
        }
    }
}

/// Mix `incoming` over `outgoing` in place, advancing the fade as it goes.
///
/// Both slices are interleaved at `channels`. `outgoing` is written in place
/// and becomes the mixed result. Mixing stops at whichever slice is shorter,
/// and the number of frames actually mixed is returned so the caller can keep
/// its own position in step.
///
/// `frame` is the fade position in frames at the start of the call and `total`
/// its full length; both are counted by the caller so a mix split across
/// several blocks stays continuous.
pub fn mix(
    outgoing: &mut [f32],
    incoming: &[f32],
    channels: usize,
    curve: CrossfadeCurve,
    frame: u64,
    total: u64,
) -> usize {
    if channels == 0 || total == 0 {
        return 0;
    }

    let frames = (outgoing.len().min(incoming.len())) / channels;

    for f in 0..frames {
        // Guarding the division here rather than hoisting it keeps a fade that
        // runs past its own length pinned at fully-faded instead of wrapping.
        let progress = ((frame + f as u64) as f32 / total as f32).clamp(0.0, 1.0);
        let (out_gain, in_gain) = gains(curve, progress);

        let base = f * channels;
        for c in 0..channels {
            let i = base + c;
            outgoing[i] = outgoing[i] * out_gain + incoming[i] * in_gain;
        }
    }

    frames
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINEAR: CrossfadeCurve = CrossfadeCurve::Linear;
    const POWER: CrossfadeCurve = CrossfadeCurve::EqualPower;

    #[test]
    fn a_fade_starts_on_the_outgoing_track_and_ends_on_the_incoming_one() {
        for curve in [LINEAR, POWER] {
            let (out, incoming) = gains(curve, 0.0);
            assert!((out - 1.0).abs() < 1e-6, "{curve:?}");
            assert!(incoming.abs() < 1e-6, "{curve:?}");

            let (out, incoming) = gains(curve, 1.0);
            assert!(out.abs() < 1e-6, "{curve:?}");
            assert!((incoming - 1.0).abs() < 1e-6, "{curve:?}");
        }
    }

    #[test]
    fn progress_outside_the_fade_is_clamped_rather_than_wrapped() {
        // A fade that overruns must stay fully faded. Wrapping would jump the
        // outgoing track back to full volume, which is an audible thump.
        assert_eq!(gains(LINEAR, 5.0), gains(LINEAR, 1.0));
        assert_eq!(gains(LINEAR, -5.0), gains(LINEAR, 0.0));
    }

    #[test]
    fn linear_gains_sum_to_one() {
        for step in 0..=10 {
            let t = step as f32 / 10.0;
            let (a, b) = gains(LINEAR, t);
            assert!((a + b - 1.0).abs() < 1e-6, "at {t}");
        }
    }

    #[test]
    fn equal_power_holds_power_rather_than_amplitude() {
        // The property that makes it the default: a + b dips below 1 in the
        // middle, but a^2 + b^2 stays at 1 throughout, which is what the ear
        // tracks.
        for step in 0..=10 {
            let t = step as f32 / 10.0;
            let (a, b) = gains(POWER, t);
            assert!(
                (a * a + b * b - 1.0).abs() < 1e-6,
                "power was not constant at {t}"
            );
        }

        let (a, b) = gains(POWER, 0.5);
        assert!(
            a + b > 1.0,
            "equal power should exceed unity at the midpoint"
        );
    }

    #[test]
    fn the_midpoint_of_a_linear_fade_is_half_of_each() {
        let (a, b) = gains(LINEAR, 0.5);
        assert!((a - 0.5).abs() < 1e-6);
        assert!((b - 0.5).abs() < 1e-6);
    }

    #[test]
    fn mixing_at_the_start_of_a_fade_is_the_outgoing_track() {
        let mut out = vec![1.0, 1.0, 1.0, 1.0];
        let incoming = vec![-1.0, -1.0, -1.0, -1.0];

        let frames = mix(&mut out, &incoming, 2, LINEAR, 0, 100);

        assert_eq!(frames, 2);
        assert!((out[0] - 1.0).abs() < 0.05, "{out:?}");
    }

    #[test]
    fn mixing_at_the_end_of_a_fade_is_the_incoming_track() {
        let mut out = vec![1.0, 1.0];
        let incoming = vec![-1.0, -1.0];

        mix(&mut out, &incoming, 2, LINEAR, 100, 100);

        for sample in &out {
            assert!((sample + 1.0).abs() < 1e-6, "{out:?}");
        }
    }

    #[test]
    fn every_channel_of_a_frame_gets_the_same_gain() {
        // The bug this guards: computing gain per sample rather than per frame
        // shears the stereo image, which is audible even when levels are right.
        let mut out = vec![1.0; 8];
        let incoming = vec![0.0; 8];

        mix(&mut out, &incoming, 2, POWER, 0, 4);

        for frame in out.chunks(2) {
            assert!(
                (frame[0] - frame[1]).abs() < 1e-9,
                "channels drifted apart: {frame:?}"
            );
        }
    }

    #[test]
    fn mixing_stops_at_the_shorter_slice() {
        let mut out = vec![1.0; 8];
        let incoming = vec![0.5; 4];

        let frames = mix(&mut out, &incoming, 2, LINEAR, 0, 100);

        assert_eq!(frames, 2, "only the frames both streams had");
        // The unmixed tail is left untouched for the caller to deal with.
        assert!((out[6] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_zero_length_fade_mixes_nothing() {
        // Guards a division by zero, and the config allows 0.0 seconds since
        // that is how crossfade is switched off.
        let mut out = vec![1.0; 4];
        assert_eq!(mix(&mut out, &[0.0; 4], 2, LINEAR, 0, 0), 0);
        assert!((out[0] - 1.0).abs() < 1e-6, "samples must be left alone");
    }

    #[test]
    fn a_zero_channel_stream_is_refused_rather_than_dividing_by_zero() {
        let mut out = vec![1.0; 4];
        assert_eq!(mix(&mut out, &[0.0; 4], 0, LINEAR, 0, 10), 0);
    }

    #[test]
    fn a_fade_never_amplifies() {
        // Summing two full-scale streams without gain would clip. Neither gain
        // may exceed unity at any point of either curve.
        for curve in [LINEAR, POWER] {
            for step in 0..=100 {
                let (a, b) = gains(curve, step as f32 / 100.0);
                assert!(a <= 1.0 + 1e-6 && b <= 1.0 + 1e-6, "{curve:?} at {step}");
                assert!(a >= -1e-6 && b >= -1e-6, "{curve:?} at {step}");
            }
        }
    }
}
