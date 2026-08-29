//! Sample-rate conversion from a file's rate to the output device's rate.
//!
//! This matters more than it sounds: the overwhelming majority of music is
//! 44.1 kHz, while Windows shared-mode output is usually 48 kHz. Without
//! conversion every track plays ~8.8% fast and sharp.
//!
//! `rubato` 5 reworked its API around the `audioadapter` traits, so buffers are
//! described by a wrapper rather than passed as bare slices. That is what lets
//! us feed **planar** input straight from the decoder and get **interleaved**
//! output for the device in one step, with no intermediate copy.

use rubato::audioadapter_buffers::direct::{InterleavedSlice, SequentialSliceOfVecs};
use rubato::{
    Async, FixedAsync, Resampler as _, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};

use crate::error::AudioError;

/// Input frames handed to the resampler per call.
///
/// A larger chunk amortises the sinc filter's setup cost; a smaller one keeps
/// latency down. 1024 frames is ~23 ms at 44.1 kHz, comfortably below the
/// ring buffer's depth.
const CHUNK_FRAMES: usize = 1024;

/// Converts planar `f32` at one rate into interleaved `f32` at another.
///
/// When the rates already match, this becomes a pure interleaver and no
/// filtering is performed at all.
pub struct Resampler {
    /// `None` when input and output rates match.
    inner: Option<Async<f32>>,
    channels: usize,
    from_rate: u32,
    to_rate: u32,

    /// Planar staging area: decoded frames accumulate here until a full chunk
    /// is available.
    pending: Vec<Vec<f32>>,
    /// Interleaved scratch the resampler writes into.
    out: Vec<f32>,
}

impl Resampler {
    pub fn new(from_rate: u32, to_rate: u32, channels: usize) -> Result<Self, AudioError> {
        let channels = channels.max(1);

        let inner = if from_rate == to_rate {
            None
        } else {
            let params = SincInterpolationParameters {
                // 256 taps is transparent for music without being extravagant.
                sinc_len: 256,
                f_cutoff: Some(rubato::calculate_cutoff::<f32>(
                    256,
                    WindowFunction::BlackmanHarris2,
                )),
                interpolation: SincInterpolationType::Quadratic,
                oversampling_factor: 256,
                window: WindowFunction::BlackmanHarris2,
            };

            let ratio = f64::from(to_rate) / f64::from(from_rate);

            Some(
                Async::<f32>::new_sinc(
                    ratio,
                    // No dynamic ratio changes in M1, so the relative range is
                    // just above 1.0. Crossfade and speed control would widen it.
                    1.1,
                    &params,
                    CHUNK_FRAMES,
                    channels,
                    FixedAsync::Input,
                )
                .map_err(|err| AudioError::Resampler(err.to_string()))?,
            )
        };

        // Worst-case output for one chunk, plus headroom for the sinc filter's
        // variable output length.
        let max_out = inner
            .as_ref()
            .map_or(CHUNK_FRAMES, rubato::Resampler::output_frames_max);

        Ok(Self {
            inner,
            channels,
            from_rate,
            to_rate,
            pending: vec![Vec::new(); channels],
            out: vec![0.0; max_out * channels],
        })
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// True when input and output rates match and no filtering happens.
    pub fn is_passthrough(&self) -> bool {
        self.inner.is_none()
    }

    /// Largest number of samples a single [`pull`](Self::pull) can return.
    ///
    /// Callers must reserve at least this much space before pulling. Upsampling
    /// multiplies the block size — 1024 input frames at 44.1 kHz become ~2230 at
    /// 96 kHz, and on an 8-channel device that is nearly 18k samples from one
    /// call.
    pub fn max_output_samples(&self) -> usize {
        match &self.inner {
            Some(inner) => rubato::Resampler::output_frames_max(inner) * self.channels,
            None => CHUNK_FRAMES * self.channels,
        }
    }

    /// How many device frames one source frame becomes, on average.
    ///
    /// Used to convert a source-rate position into a device-rate one.
    pub fn ratio(&self) -> f64 {
        f64::from(self.to_rate) / f64::from(self.from_rate)
    }

    /// Add decoded planar frames to the staging area.
    ///
    /// `planes` often holds a different channel count than the device wants.
    /// Two distinct cases, and conflating them is a real bug:
    ///
    /// - **Mono source**: copy the single plane to every output channel, so a
    ///   mono file is centred rather than stuck in the left speaker.
    /// - **Multi-channel source**: map 1:1 as far as the source goes and leave
    ///   the rest silent. Repeating the last plane instead would put the right
    ///   channel into the centre, LFE and surrounds of a 5.1/7.1 device.
    ///
    /// A proper matrixed upmix belongs with the rest of the DSP work in M3;
    /// silence is the correct, unsurprising default until then.
    pub fn push(&mut self, planes: &[Vec<f32>], frames: usize) {
        if planes.is_empty() || frames == 0 {
            return;
        }

        let mono_source = planes.len() == 1;

        for ch in 0..self.channels {
            let source = if mono_source {
                Some(0)
            } else {
                planes.get(ch).map(|_| ch)
            };

            let Some(index) = source else {
                // No corresponding source channel: keep it silent, but keep the
                // length in step with the other planes.
                let padded = self.pending[ch].len() + frames;
                self.pending[ch].resize(padded, 0.0);
                continue;
            };

            let src = &planes[index];
            let take = frames.min(src.len());
            self.pending[ch].extend_from_slice(&src[..take]);

            // A malformed chunk with short planes would otherwise desynchronise
            // the channels against each other.
            if take < frames {
                let padded = self.pending[ch].len() + (frames - take);
                self.pending[ch].resize(padded, 0.0);
            }
        }
    }

    /// Frames currently staged and not yet converted.
    pub fn pending_frames(&self) -> usize {
        self.pending.first().map_or(0, Vec::len)
    }

    /// Convert one chunk if enough input is staged.
    ///
    /// Returns interleaved device-rate samples, or `None` when more input is
    /// needed. Call repeatedly until it returns `None`.
    pub fn pull(&mut self) -> Option<&[f32]> {
        let staged = self.pending_frames();

        let Some(resampler) = self.inner.as_mut() else {
            return Self::interleave_passthrough(
                &mut self.pending,
                &mut self.out,
                self.channels,
                CHUNK_FRAMES,
            );
        };

        let needed = resampler.input_frames_next();
        if staged < needed {
            return None;
        }

        let input = SequentialSliceOfVecs::new(&self.pending, self.channels, needed)
            .expect("staging buffer is sized for the channel count");

        let max_out = resampler.output_frames_max();
        if self.out.len() < max_out * self.channels {
            self.out.resize(max_out * self.channels, 0.0);
        }

        let mut output = InterleavedSlice::new_mut(&mut self.out, self.channels, max_out)
            .expect("output scratch is sized for the channel count");

        let (consumed, produced) = resampler
            .process_into_buffer(&input, &mut output, None)
            .ok()?;

        for plane in &mut self.pending {
            plane.drain(..consumed.min(plane.len()));
        }

        Some(&self.out[..produced * self.channels])
    }

    /// Flush staged input at end of track, padding the final partial chunk.
    ///
    /// Without this the last few milliseconds of every track are dropped.
    pub fn drain(&mut self) -> Option<&[f32]> {
        let staged = self.pending_frames();
        if staged == 0 {
            return None;
        }

        let Some(resampler) = self.inner.as_mut() else {
            return Self::interleave_passthrough(
                &mut self.pending,
                &mut self.out,
                self.channels,
                staged,
            );
        };

        // Pad up to a full chunk with silence so the filter can run; the
        // resampler is told the real length via `partial_len` so the padding
        // does not become audible tail.
        let needed = resampler.input_frames_next();
        for plane in &mut self.pending {
            plane.resize(needed, 0.0);
        }

        let input = SequentialSliceOfVecs::new(&self.pending, self.channels, needed)
            .expect("staging buffer is sized for the channel count");

        let max_out = resampler.output_frames_max();
        if self.out.len() < max_out * self.channels {
            self.out.resize(max_out * self.channels, 0.0);
        }

        let mut output = InterleavedSlice::new_mut(&mut self.out, self.channels, max_out)
            .expect("output scratch is sized for the channel count");

        let indexing = rubato::Indexing {
            input_offset: 0,
            output_offset: 0,
            partial_len: Some(staged),
            active_channels_mask: None,
        };

        let (_, produced) = resampler
            .process_into_buffer(&input, &mut output, Some(&indexing))
            .ok()?;

        for plane in &mut self.pending {
            plane.clear();
        }

        Some(&self.out[..produced * self.channels])
    }

    /// Discard staged input and filter history, for a seek or track change.
    pub fn reset(&mut self) {
        for plane in &mut self.pending {
            plane.clear();
        }
        if let Some(resampler) = self.inner.as_mut() {
            resampler.reset();
        }
    }

    /// Interleave staged planar frames directly, used when no conversion is
    /// needed. Split out as an associated function to keep the borrow checker
    /// happy about touching two fields at once.
    fn interleave_passthrough<'a>(
        pending: &mut [Vec<f32>],
        out: &'a mut Vec<f32>,
        channels: usize,
        want: usize,
    ) -> Option<&'a [f32]> {
        let available = pending.first().map_or(0, Vec::len);
        let frames = want.min(available);
        if frames == 0 {
            return None;
        }

        if out.len() < frames * channels {
            out.resize(frames * channels, 0.0);
        }

        for frame in 0..frames {
            for (ch, plane) in pending.iter().enumerate().take(channels) {
                out[frame * channels + ch] = plane[frame];
            }
        }

        for plane in pending.iter_mut() {
            plane.drain(..frames.min(plane.len()));
        }

        Some(&out[..frames * channels])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One second of a sine at `freq`, as planar mono.
    fn sine(rate: u32, freq: f32, frames: usize) -> Vec<Vec<f32>> {
        let plane = (0..frames)
            .map(|i| {
                let t = i as f32 / rate as f32;
                (std::f32::consts::TAU * freq * t).sin()
            })
            .collect();
        vec![plane]
    }

    #[test]
    fn matching_rates_bypass_the_filter() {
        let r = Resampler::new(44_100, 44_100, 2).unwrap();
        assert!(r.is_passthrough());
    }

    #[test]
    fn differing_rates_engage_the_filter() {
        let r = Resampler::new(44_100, 48_000, 2).unwrap();
        assert!(!r.is_passthrough());
        assert!((r.ratio() - 48_000.0 / 44_100.0).abs() < 1e-9);
    }

    #[test]
    fn passthrough_interleaves_channels_in_order() {
        let mut r = Resampler::new(48_000, 48_000, 2).unwrap();
        let planes = vec![vec![1.0, 2.0, 3.0], vec![-1.0, -2.0, -3.0]];
        r.push(&planes, 3);

        let out = r.pull().expect("passthrough yields whatever is staged");
        assert_eq!(out, &[1.0, -1.0, 2.0, -2.0, 3.0, -3.0]);
    }

    /// A mono file on a stereo device should come out centred, not silent on
    /// one side.
    #[test]
    fn mono_is_duplicated_across_channels() {
        let mut r = Resampler::new(48_000, 48_000, 2).unwrap();
        r.push(&[vec![0.5, 0.25]], 2);

        let out = r.pull().unwrap();
        assert_eq!(out, &[0.5, 0.5, 0.25, 0.25]);
    }

    /// Mono upmix must reach every channel of a surround device, not just the
    /// front pair.
    #[test]
    fn mono_fills_all_channels_of_a_surround_device() {
        let mut r = Resampler::new(48_000, 48_000, 8).unwrap();
        r.push(&[vec![0.5]], 1);

        let out = r.pull().unwrap();
        assert_eq!(out, &[0.5; 8]);
    }

    /// The bug this guards: on an 8-channel device a stereo file must not have
    /// its right channel copied into the centre, LFE and surrounds.
    #[test]
    fn stereo_on_a_surround_device_leaves_extra_channels_silent() {
        let mut r = Resampler::new(48_000, 48_000, 8).unwrap();
        r.push(&[vec![-1.0], vec![1.0]], 1);

        let out = r.pull().unwrap();
        assert_eq!(out[0], -1.0, "front left");
        assert_eq!(out[1], 1.0, "front right");
        assert!(
            out[2..].iter().all(|&s| s == 0.0),
            "channels beyond the source must be silent, got {:?}",
            &out[2..]
        );
    }

    /// The output should be longer than the input by roughly the rate ratio.
    #[test]
    fn upsampling_produces_proportionally_more_frames() {
        let channels = 1;
        let mut r = Resampler::new(44_100, 48_000, channels).unwrap();

        let input_frames = CHUNK_FRAMES * 8;
        let planes = sine(44_100, 440.0, input_frames);
        r.push(&planes, input_frames);

        let mut produced = 0;
        while let Some(chunk) = r.pull() {
            produced += chunk.len() / channels;
        }

        let expected = input_frames as f64 * (48_000.0 / 44_100.0);
        // The filter holds some input back as history, so allow a chunk of slack.
        let slack = CHUNK_FRAMES as f64 * 1.5;
        assert!(
            (produced as f64 - expected).abs() < slack,
            "produced {produced}, expected about {expected}"
        );
    }

    /// A resampled sine must stay smooth. Any dropped or duplicated block shows
    /// up as a step between consecutive samples far larger than the waveform
    /// itself can produce - which is what crackling actually is.
    #[test]
    fn resampled_output_has_no_discontinuities() {
        let channels = 1;
        let freq = 440.0;
        let (from, to) = (44_100, 96_000);

        let mut r = Resampler::new(from, to, channels).unwrap();

        let input_frames = CHUNK_FRAMES * 16;
        let planes = sine(from, freq, input_frames);
        r.push(&planes, input_frames);

        let mut out = Vec::new();
        while let Some(block) = r.pull() {
            out.extend_from_slice(block);
        }

        assert!(
            out.len() > CHUNK_FRAMES,
            "expected a useful amount of output"
        );

        // The largest step a clean sine can take between two samples at the
        // output rate, with generous headroom for filter ripple.
        let ceiling = (std::f32::consts::TAU * freq / to as f32) * 3.0;

        // Skip the filter warm-up, which legitimately ramps from silence.
        let skip = 512.min(out.len() / 4);
        let mut worst = 0.0f32;
        for pair in out[skip..].windows(2) {
            worst = worst.max((pair[1] - pair[0]).abs());
        }

        assert!(
            worst < ceiling,
            "discontinuity of {worst} exceeds the {ceiling} a clean sine allows"
        );
    }

    #[test]
    fn reset_discards_staged_input() {
        let mut r = Resampler::new(44_100, 48_000, 2).unwrap();
        r.push(&[vec![0.1; 512], vec![0.1; 512]], 512);
        assert_eq!(r.pending_frames(), 512);

        r.reset();
        assert_eq!(r.pending_frames(), 0);
    }
}
