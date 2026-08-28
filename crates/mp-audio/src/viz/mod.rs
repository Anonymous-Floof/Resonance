//! The tap that carries audio from the callback to the visualisers.
//!
//! Drawing needs to see the samples, and the samples exist only inside the
//! audio callback — a thread that must never block, allocate, or wait on the
//! UI. So the connection between them is deliberately one-way and *lossy*: the
//! callback pushes into a small lock-free ring and, if that ring is full
//! because the UI is busy, it throws the samples away and carries on. A
//! visualiser that stutters is a cosmetic problem; a callback that blocks is a
//! dropout.
//!
//! The ring carries mono, because every visualiser here wants a single
//! waveform and downmixing eight channels once in the callback is far cheaper
//! than shipping eight and downmixing on the other side.
//!
//! [`Tap`] is the producer, owned by the [`Chain`](crate::dsp::Chain) inside
//! the callback. [`Monitor`] is the consumer, owned by the UI, which keeps a
//! rolling history so it always has a full FFT window to look at even though
//! each frame only delivers a fraction of one.

pub mod analyzer;

pub use analyzer::{Analyzer, Frame};

/// How many mono samples the ring holds.
///
/// At 48 kHz this is roughly 170 ms — comfortably more than the ~16 ms a
/// 60 fps UI needs between reads, so only a real stall causes a drop.
pub const RING_SAMPLES: usize = 8192;

/// How much recent audio the consumer keeps.
///
/// Has to exceed one FFT window plus one frame's worth of arrivals, or the
/// spectrum would be analysing a window that is partly stale. The window grows
/// with the sample rate (see [`analyzer::fft_size_for`]), so this is sized for
/// the largest one with room to spare.
pub const HISTORY_SAMPLES: usize = 16_384;

/// Build a connected tap and monitor.
pub fn channel() -> (Tap, Monitor) {
    let (producer, consumer) = rtrb::RingBuffer::<f32>::new(RING_SAMPLES);
    (
        Tap { producer },
        Monitor {
            consumer,
            history: vec![0.0; HISTORY_SAMPLES].into_boxed_slice(),
            write: 0,
            received: 0,
        },
    )
}

/// The producer end, written from the audio callback.
///
/// Every method here is real-time safe: no allocation, no locks, no
/// transcendental math.
pub struct Tap {
    producer: rtrb::Producer<f32>,
}

impl Tap {
    /// Push one interleaved frame as a single mono sample.
    ///
    /// `envelope` is the fade envelope, applied so that pausing makes the
    /// visualiser settle instead of freezing mid-waveform. The volume control
    /// is deliberately *not* applied — see [`Chain::process`] for why.
    ///
    /// [`Chain::process`]: crate::dsp::Chain::process
    #[inline]
    pub fn push_frame(&mut self, frame: &[f32], envelope: f32) {
        if frame.is_empty() {
            return;
        }

        let mut sum = 0.0;
        for sample in frame {
            sum += *sample;
        }

        // A full ring means the UI has not read for over a tenth of a second.
        // Dropping is the whole point: the alternative is waiting, here, on the
        // audio thread.
        let _ = self.producer.push(sum / frame.len() as f32 * envelope);
    }

    /// Whether the consumer has gone away.
    pub fn is_abandoned(&self) -> bool {
        self.producer.is_abandoned()
    }
}

/// The consumer end, read from the UI thread.
///
/// Keeps a rolling window of recent audio, because one UI frame's worth of
/// arrivals (~800 samples at 60 fps) is smaller than one FFT window (2048).
pub struct Monitor {
    consumer: rtrb::Consumer<f32>,
    history: Box<[f32]>,
    /// Where the next arriving sample goes.
    write: usize,
    received: u64,
}

impl Monitor {
    /// Drain everything waiting into the history. Returns how many arrived.
    ///
    /// Call once per UI frame, before reading.
    pub fn poll(&mut self) -> usize {
        let mut count = 0;
        let len = self.history.len();

        while let Ok(sample) = self.consumer.pop() {
            self.history[self.write] = sample;
            self.write = (self.write + 1) % len;
            count += 1;
        }

        self.received += count as u64;
        count
    }

    /// Total samples ever received. Distinguishes "silent" from "not running".
    pub fn received(&self) -> u64 {
        self.received
    }

    /// Advance the history with silence, for audio that never arrived.
    ///
    /// The callback stops pushing altogether once it has faded out and there is
    /// nothing to play — it returns before it touches the chain at all. Without
    /// this the history would hold the last loud window indefinitely, and the
    /// visualiser would freeze mid-shape on pause instead of settling, which
    /// reads as a hang rather than a stop.
    ///
    /// Capped at the length of the history: filling it once erases everything,
    /// and a long stall should not spin here.
    pub fn starve(&mut self, samples: usize) {
        let len = self.history.len();

        for _ in 0..samples.min(len) {
            self.history[self.write] = 0.0;
            self.write = (self.write + 1) % len;
        }
    }

    /// Whether the audio side has gone (the device closed, or was rebuilt).
    pub fn is_abandoned(&self) -> bool {
        self.consumer.is_abandoned()
    }

    /// Copy the most recent `out.len()` samples into `out`, oldest first.
    ///
    /// Asking for more than the history holds pads the front with silence
    /// rather than failing, so a visualiser can start drawing immediately
    /// instead of waiting for the buffer to warm up.
    pub fn latest(&self, out: &mut [f32]) {
        let len = self.history.len();
        let wanted = out.len().min(len);

        // The window ends at `write` (exclusive) and runs backwards from there.
        let start = (self.write + len - wanted) % len;

        for (index, slot) in out.iter_mut().take(wanted).enumerate() {
            *slot = self.history[(start + index) % len];
        }

        // Only reachable when the caller wants a longer window than we keep.
        for slot in out.iter_mut().skip(wanted) {
            *slot = 0.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pushed_frame_arrives_downmixed_to_mono() {
        let (mut tap, mut monitor) = channel();

        tap.push_frame(&[1.0, 0.0], 1.0);
        tap.push_frame(&[0.5, 0.5], 1.0);

        assert_eq!(monitor.poll(), 2);

        let mut out = [0.0; 2];
        monitor.latest(&mut out);
        assert_eq!(out, [0.5, 0.5]);
    }

    /// Pausing has to make the visualiser settle rather than freeze, so the
    /// fade envelope travels with the audio.
    #[test]
    fn the_fade_envelope_scales_what_the_visualiser_sees() {
        let (mut tap, mut monitor) = channel();

        tap.push_frame(&[1.0, 1.0], 0.25);
        monitor.poll();

        let mut out = [0.0; 1];
        monitor.latest(&mut out);
        assert_eq!(out[0], 0.25);
    }

    /// The contract is explicitly lossy: a UI that stops reading must not be
    /// able to stall the audio thread.
    #[test]
    fn a_full_ring_drops_samples_instead_of_blocking() {
        let (mut tap, mut monitor) = channel();

        // Far more than the ring can hold. Every push has to return.
        for index in 0..RING_SAMPLES * 3 {
            tap.push_frame(&[index as f32], 1.0);
        }

        assert_eq!(monitor.poll(), RING_SAMPLES);
    }

    #[test]
    fn the_latest_window_is_in_chronological_order() {
        let (mut tap, mut monitor) = channel();

        for index in 0..100 {
            tap.push_frame(&[index as f32], 1.0);
        }
        monitor.poll();

        let mut out = [0.0; 5];
        monitor.latest(&mut out);
        assert_eq!(out, [95.0, 96.0, 97.0, 98.0, 99.0]);
    }

    /// The history is a ring; a window spanning the wrap must not come back
    /// scrambled.
    #[test]
    fn a_window_spanning_the_wrap_point_stays_in_order() {
        let (mut tap, mut monitor) = channel();

        for index in 0..HISTORY_SAMPLES + 50 {
            tap.push_frame(&[index as f32], 1.0);
            // Drained as we go, or the ring itself would drop.
            if index % 1000 == 0 {
                monitor.poll();
            }
        }
        monitor.poll();

        let mut out = [0.0; 8];
        monitor.latest(&mut out);

        for pair in out.windows(2) {
            assert_eq!(pair[1] - pair[0], 1.0, "history came back out of order");
        }
    }

    /// A stopped stream has to settle, not freeze on its last loud window.
    #[test]
    fn starving_the_history_erases_what_was_there() {
        let (mut tap, mut monitor) = channel();

        for _ in 0..2048 {
            tap.push_frame(&[0.9], 1.0);
        }
        monitor.poll();

        let mut out = [0.0; 512];
        monitor.latest(&mut out);
        assert!(out.iter().any(|s| *s > 0.5), "the fixture never arrived");

        monitor.starve(HISTORY_SAMPLES);

        monitor.latest(&mut out);
        assert!(
            out.iter().all(|s| *s == 0.0),
            "loud audio survived a full starve"
        );
    }

    #[test]
    fn starving_more_than_the_history_holds_is_bounded() {
        let (_tap, mut monitor) = channel();

        // Would loop for a very long time if it were not capped.
        monitor.starve(usize::MAX);

        assert!(monitor.write < HISTORY_SAMPLES);
    }

    #[test]
    fn an_empty_monitor_reads_as_silence() {
        let (_tap, monitor) = channel();

        let mut out = [1.0; 64];
        monitor.latest(&mut out);
        assert!(out.iter().all(|sample| *sample == 0.0));
    }
}
