//! State shared between the UI, the worker thread and the audio callback.
//!
//! Everything here is a plain atomic. The audio callback touches this on every
//! buffer, so it must never take a lock, allocate, or block — a stall of even a
//! few milliseconds is an audible dropout.
//!
//! Anything too large to be atomic (track titles, paths) travels as an [`Event`]
//! instead, so the callback never needs to read it.
//!
//! [`Event`]: crate::engine::Event

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

/// What the player is currently doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Status {
    Stopped = 0,
    Playing = 1,
    Paused = 2,
}

impl Status {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Playing,
            2 => Self::Paused,
            _ => Self::Stopped,
        }
    }

    pub fn is_playing(self) -> bool {
        matches!(self, Self::Playing)
    }
}

/// Lock-free state read by the UI and written by the worker and callback.
#[derive(Debug)]
pub struct Shared {
    status: AtomicU8,

    /// Frames of the current track already sent to the device.
    ///
    /// Written only by the callback, and reset by the worker between tracks (at
    /// which point the callback is guaranteed to be idle, because the ring has
    /// been drained).
    position_frames: AtomicU64,

    /// Total frames the worker has pushed for the current track.
    ///
    /// Once `end_of_track` is set and `position_frames` reaches this, the track
    /// has genuinely finished playing rather than merely finished decoding.
    pushed_frames: AtomicU64,

    /// Length of the current track in milliseconds; `0` means unknown.
    duration_ms: AtomicU64,

    device_rate: AtomicU32,
    device_channels: AtomicU32,

    /// Target gain as raw `f32` bits, already mapped through the volume curve.
    gain: AtomicU32,
    muted: AtomicBool,

    /// The worker has decoded everything for this track.
    end_of_track: AtomicBool,

    /// The ring is being refilled after a flush and is not ready to play.
    ///
    /// Set on every track change and seek. While it is set the callback outputs
    /// silence without advancing the position or counting an underrun: the ring
    /// is empty because we just emptied it, not because decoding fell behind.
    /// Starting playback from a nearly-empty ring would starve immediately.
    priming: AtomicBool,
    /// Set by the callback while the limiter is reducing gain.
    limiting: AtomicBool,

    /// Buffer underruns, surfaced in the debug overlay. Should stay at zero.
    xruns: AtomicU64,

    /// Samples the worker produced but could not fit into the ring.
    ///
    /// Any non-zero value is audible: a dropped sample is a step discontinuity
    /// in the waveform, which is heard as a click or crackle regardless of how
    /// low the volume is.
    dropped: AtomicU64,

    /// Flush handshake for seeks and track changes.
    ///
    /// The worker bumps `flush_request`; the callback empties the ring and
    /// copies the value into `flush_ack`. This is how stale audio is discarded
    /// without the worker ever touching the consumer side of the ring.
    flush_request: AtomicU64,
    flush_ack: AtomicU64,
}

impl Default for Shared {
    fn default() -> Self {
        Self::new()
    }
}

impl Shared {
    pub fn new() -> Self {
        Self {
            status: AtomicU8::new(Status::Stopped as u8),
            position_frames: AtomicU64::new(0),
            pushed_frames: AtomicU64::new(0),
            duration_ms: AtomicU64::new(0),
            device_rate: AtomicU32::new(48_000),
            device_channels: AtomicU32::new(2),
            gain: AtomicU32::new(1.0f32.to_bits()),
            muted: AtomicBool::new(false),
            end_of_track: AtomicBool::new(false),
            priming: AtomicBool::new(true),
            limiting: AtomicBool::new(false),
            xruns: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            flush_request: AtomicU64::new(0),
            flush_ack: AtomicU64::new(0),
        }
    }

    // -- status ------------------------------------------------------------

    pub fn status(&self) -> Status {
        Status::from_u8(self.status.load(Ordering::Acquire))
    }

    pub fn set_status(&self, status: Status) {
        self.status.store(status as u8, Ordering::Release);
    }

    // -- position ----------------------------------------------------------

    pub fn position_frames(&self) -> u64 {
        self.position_frames.load(Ordering::Relaxed)
    }

    pub fn advance_position(&self, frames: u64) {
        self.position_frames.fetch_add(frames, Ordering::Relaxed);
    }

    pub fn set_position_frames(&self, frames: u64) {
        self.position_frames.store(frames, Ordering::Relaxed);
    }

    /// Subtract `frames` from the position without losing a concurrent advance.
    ///
    /// At a gapless track boundary the worker rebases the position while the
    /// callback is still advancing it. Load-modify-store would silently discard
    /// whatever the callback added in between, which shows up as the elapsed
    /// time stuttering backwards at every seam.
    pub fn rebase_position(&self, frames: u64) {
        let _ =
            self.position_frames
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                    Some(current.saturating_sub(frames))
                });
    }

    pub fn pushed_frames(&self) -> u64 {
        self.pushed_frames.load(Ordering::Relaxed)
    }

    pub fn add_pushed_frames(&self, frames: u64) {
        self.pushed_frames.fetch_add(frames, Ordering::Relaxed);
    }

    pub fn set_pushed_frames(&self, frames: u64) {
        self.pushed_frames.store(frames, Ordering::Relaxed);
    }

    /// Playback position in seconds.
    pub fn position_secs(&self) -> f64 {
        let rate = f64::from(self.device_rate()).max(1.0);
        self.position_frames() as f64 / rate
    }

    /// Track length in seconds, or `None` when the container did not say.
    pub fn duration_secs(&self) -> Option<f64> {
        match self.duration_ms.load(Ordering::Relaxed) {
            0 => None,
            ms => Some(ms as f64 / 1000.0),
        }
    }

    pub fn set_duration(&self, duration: Option<std::time::Duration>) {
        let ms = duration.map_or(0, |d| d.as_millis().min(u128::from(u64::MAX)) as u64);
        self.duration_ms.store(ms, Ordering::Relaxed);
    }

    /// Progress through the track as `0.0..=1.0`, for the seek bar.
    ///
    /// Returns `0.0` when the length is unknown, since there is no meaningful
    /// fraction to show.
    pub fn progress(&self) -> f32 {
        match self.duration_secs() {
            Some(total) if total > 0.0 => (self.position_secs() / total).clamp(0.0, 1.0) as f32,
            _ => 0.0,
        }
    }

    // -- device ------------------------------------------------------------

    pub fn device_rate(&self) -> u32 {
        self.device_rate.load(Ordering::Relaxed)
    }

    pub fn device_channels(&self) -> usize {
        self.device_channels.load(Ordering::Relaxed) as usize
    }

    pub fn set_device(&self, rate: u32, channels: usize) {
        self.device_rate.store(rate, Ordering::Relaxed);
        self.device_channels
            .store(channels as u32, Ordering::Relaxed);
    }

    // -- volume ------------------------------------------------------------

    pub fn gain(&self) -> f32 {
        f32::from_bits(self.gain.load(Ordering::Relaxed))
    }

    pub fn set_gain(&self, gain: f32) {
        self.gain.store(gain.to_bits(), Ordering::Relaxed);
    }

    pub fn muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    /// The gain the callback should actually apply right now.
    pub fn effective_gain(&self) -> f32 {
        if self.muted() { 0.0 } else { self.gain() }
    }

    // -- track lifecycle ---------------------------------------------------

    pub fn end_of_track(&self) -> bool {
        self.end_of_track.load(Ordering::Acquire)
    }

    pub fn set_end_of_track(&self, done: bool) {
        self.end_of_track.store(done, Ordering::Release);
    }

    /// Whether the limiter is currently pulling the level down.
    ///
    /// Read by the UI for the clip indicator. Written by the callback, so it
    /// has to be an atomic rather than anything that could block.
    pub fn limiting(&self) -> bool {
        self.limiting.load(Ordering::Relaxed)
    }

    pub fn set_limiting(&self, limiting: bool) {
        self.limiting.store(limiting, Ordering::Relaxed);
    }

    pub fn priming(&self) -> bool {
        self.priming.load(Ordering::Acquire)
    }

    pub fn set_priming(&self, priming: bool) {
        self.priming.store(priming, Ordering::Release);
    }

    /// True once every decoded frame of the current track has been played out.
    pub fn track_fully_played(&self) -> bool {
        self.end_of_track() && self.position_frames() >= self.pushed_frames()
    }

    // -- diagnostics -------------------------------------------------------

    pub fn xruns(&self) -> u64 {
        self.xruns.load(Ordering::Relaxed)
    }

    pub fn note_xrun(&self) {
        self.xruns.fetch_add(1, Ordering::Relaxed);
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    pub fn note_dropped(&self, samples: u64) {
        self.dropped.fetch_add(samples, Ordering::Relaxed);
    }

    // -- flush handshake ---------------------------------------------------

    /// Ask the callback to discard everything currently queued.
    ///
    /// Returns the sequence number to wait for.
    pub fn request_flush(&self) -> u64 {
        self.flush_request.fetch_add(1, Ordering::AcqRel) + 1
    }

    pub fn flush_pending(&self) -> Option<u64> {
        let requested = self.flush_request.load(Ordering::Acquire);
        (requested != self.flush_ack.load(Ordering::Relaxed)).then_some(requested)
    }

    pub fn acknowledge_flush(&self, sequence: u64) {
        self.flush_ack.store(sequence, Ordering::Release);
    }

    pub fn flush_acknowledged(&self, sequence: u64) -> bool {
        self.flush_ack.load(Ordering::Acquire) >= sequence
    }

    /// Reset per-track counters. Called by the worker between tracks.
    pub fn reset_for_new_track(&self, duration: Option<std::time::Duration>) {
        self.set_position_frames(0);
        self.set_pushed_frames(0);
        self.set_end_of_track(false);
        self.set_priming(true);
        self.set_duration(duration);
    }
}

/// Map a `0.0..=1.0` slider position to a linear gain.
///
/// A linear mapping sounds wrong: almost all of the useful range bunches into
/// the top of the slider. Squaring is the usual approximation of the
/// perceptual curve, putting roughly -12 dB at the midpoint.
pub fn slider_to_gain(slider: f32) -> f32 {
    let slider = slider.clamp(0.0, 1.0);
    slider * slider
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn volume_curve_spans_silence_to_unity() {
        assert_eq!(slider_to_gain(0.0), 0.0);
        assert_eq!(slider_to_gain(1.0), 1.0);
        // Out-of-range input must not produce a gain above unity.
        assert_eq!(slider_to_gain(2.0), 1.0);
        assert_eq!(slider_to_gain(-1.0), 0.0);
    }

    #[test]
    fn volume_curve_is_monotonic() {
        let mut previous = -1.0;
        for step in 0..=100 {
            let gain = slider_to_gain(step as f32 / 100.0);
            assert!(gain > previous, "gain should rise at step {step}");
            previous = gain;
        }
    }

    #[test]
    fn progress_is_zero_when_length_is_unknown() {
        let shared = Shared::new();
        shared.set_device(48_000, 2);
        shared.set_duration(None);
        shared.advance_position(48_000);

        assert_eq!(shared.duration_secs(), None);
        assert_eq!(shared.progress(), 0.0);
    }

    #[test]
    fn progress_tracks_position_against_duration() {
        let shared = Shared::new();
        shared.set_device(48_000, 2);
        shared.set_duration(Some(Duration::from_secs(10)));
        shared.advance_position(48_000 * 5);

        assert!((shared.position_secs() - 5.0).abs() < 1e-9);
        assert!((shared.progress() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn muting_overrides_gain_without_losing_it() {
        let shared = Shared::new();
        shared.set_gain(0.8);
        shared.set_muted(true);

        assert_eq!(shared.effective_gain(), 0.0);
        // Unmuting must restore the previous level, not reset it.
        shared.set_muted(false);
        assert!((shared.effective_gain() - 0.8).abs() < 1e-6);
    }

    /// A track is only finished when the decoded frames have actually been
    /// played, not merely when decoding stopped.
    #[test]
    fn track_completion_waits_for_playout() {
        let shared = Shared::new();
        shared.reset_for_new_track(Some(Duration::from_secs(3)));
        shared.add_pushed_frames(1000);

        shared.set_end_of_track(true);
        assert!(
            !shared.track_fully_played(),
            "buffered audio is still queued"
        );

        shared.advance_position(1000);
        assert!(shared.track_fully_played());
    }

    /// Priming must not be mistaken for a stall: a freshly flushed ring is
    /// empty on purpose.
    #[test]
    fn a_new_track_starts_out_priming() {
        let shared = Shared::new();
        shared.set_priming(false);

        shared.reset_for_new_track(Some(Duration::from_secs(1)));
        assert!(shared.priming());

        shared.set_priming(false);
        assert!(!shared.priming());
    }

    #[test]
    fn flush_handshake_round_trips() {
        let shared = Shared::new();
        assert!(shared.flush_pending().is_none());

        let sequence = shared.request_flush();
        assert_eq!(shared.flush_pending(), Some(sequence));
        assert!(!shared.flush_acknowledged(sequence));

        shared.acknowledge_flush(sequence);
        assert!(shared.flush_acknowledged(sequence));
        assert!(shared.flush_pending().is_none());
    }
}
