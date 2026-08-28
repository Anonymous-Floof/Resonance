//! Joining one track to the next without a gap.
//!
//! The naive track change flushes the ring, opens the next file and starts
//! again. That is correct and simple, and it puts a silence of one buffer or
//! more between every pair of tracks — which is fine for a shuffle of singles
//! and ruins a live album or a continuous mix.
//!
//! Gapless instead keeps pushing into the *same* ring across the boundary, so
//! the last sample of one track is followed immediately by the first sample of
//! the next. The cost is bookkeeping: decoding is now ahead of playback by up
//! to a ring's worth of audio, so "which track is playing" and "how far into
//! it are we" stop being answerable from the decoder alone.
//!
//! [`Seam`] is that bookkeeping, kept apart from the engine so it can be tested
//! without a sound device. It records the frame count at which the new track
//! begins and reports when playback has actually crossed it — which is the
//! moment, and the only moment, at which the position, the duration and the
//! now-playing display should change.

use std::path::PathBuf;
use std::time::Duration;

/// A pending track boundary inside the buffered audio.
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    /// Total frames pushed when the previous track ended.
    ///
    /// Playback has crossed into the new track once the position reaches this.
    pub at_frame: u64,
    pub path: PathBuf,
    /// Index of the new track within the queue.
    pub index: usize,
    pub duration: Option<Duration>,
    /// Level correction for the new track, applied only once it starts.
    pub replay_gain_db: Option<f32>,
}

/// Tracks pending boundaries in the buffered stream.
///
/// More than one can be in flight at once: three short tracks can easily fit
/// inside a two-second ring, and dropping the second boundary would leave the
/// display stuck on the first.
#[derive(Debug, Default)]
pub struct Seam {
    pending: Vec<Pending>,
}

impl Seam {
    pub fn new() -> Self {
        Self::default()
    }

    /// Note that a new track's audio starts at `at_frame`.
    pub fn push(&mut self, boundary: Pending) {
        self.pending.push(boundary);
    }

    /// Whether any boundary is waiting to be crossed.
    pub fn is_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Discard every boundary. Used when the stream is flushed — a seek or an
    /// explicit skip makes all of them meaningless.
    pub fn clear(&mut self) {
        self.pending.clear();
    }

    /// The boundary playback has just crossed, if any.
    ///
    /// Returns at most one per call so the caller emits one event per track,
    /// even when several boundaries were passed inside a single buffer.
    pub fn crossed(&mut self, position_frames: u64) -> Option<Pending> {
        let index = self
            .pending
            .iter()
            .position(|boundary| position_frames >= boundary.at_frame)?;

        Some(self.pending.remove(index))
    }

    /// Shift every remaining boundary down by `frames`.
    ///
    /// Called when the position counter is rebased at a track change, so the
    /// boundaries that are still ahead stay in the same coordinate system.
    pub fn rebase(&mut self, frames: u64) {
        for boundary in &mut self.pending {
            boundary.at_frame = boundary.at_frame.saturating_sub(frames);
        }
    }

    /// The next boundary that has not been crossed yet.
    pub fn peek(&self) -> Option<&Pending> {
        self.pending.first()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boundary(at_frame: u64, name: &str) -> Pending {
        Pending {
            at_frame,
            path: PathBuf::from(name),
            index: 0,
            duration: Some(Duration::from_secs(180)),
            replay_gain_db: None,
        }
    }

    #[test]
    fn a_boundary_is_not_crossed_until_playback_reaches_it() {
        let mut seam = Seam::new();
        seam.push(boundary(48_000, "second.mp3"));

        assert!(seam.crossed(0).is_none());
        assert!(seam.crossed(47_999).is_none());
        assert!(seam.is_pending());

        let crossed = seam.crossed(48_000).expect("the seam should be crossed");
        assert_eq!(crossed.path, PathBuf::from("second.mp3"));
        assert!(!seam.is_pending());
    }

    /// A buffer can span a whole short track. Reporting only the newest would
    /// skip a track in the display and in the play history.
    #[test]
    fn several_boundaries_inside_one_buffer_are_reported_in_order() {
        let mut seam = Seam::new();
        seam.push(boundary(1_000, "a.mp3"));
        seam.push(boundary(2_000, "b.mp3"));
        seam.push(boundary(3_000, "c.mp3"));

        // A single jump past all three.
        let first = seam.crossed(5_000).unwrap();
        assert_eq!(first.path, PathBuf::from("a.mp3"));

        let second = seam.crossed(5_000).unwrap();
        assert_eq!(second.path, PathBuf::from("b.mp3"));

        let third = seam.crossed(5_000).unwrap();
        assert_eq!(third.path, PathBuf::from("c.mp3"));

        assert!(seam.crossed(5_000).is_none());
    }

    /// After a track change the position counter restarts, so the boundaries
    /// still ahead have to move with it or they fire immediately.
    #[test]
    fn rebasing_keeps_later_boundaries_in_the_future() {
        let mut seam = Seam::new();
        seam.push(boundary(1_000, "a.mp3"));
        seam.push(boundary(4_000, "b.mp3"));

        let crossed = seam.crossed(1_200).unwrap();
        assert_eq!(crossed.at_frame, 1_000);

        // Position is rebased to 200 (1_200 - 1_000).
        seam.rebase(1_000);
        assert_eq!(seam.peek().unwrap().at_frame, 3_000);

        assert!(seam.crossed(200).is_none(), "b should still be ahead");
        assert!(seam.crossed(3_000).is_some());
    }

    /// A seek invalidates everything buffered, boundaries included.
    #[test]
    fn flushing_discards_pending_boundaries() {
        let mut seam = Seam::new();
        seam.push(boundary(1_000, "a.mp3"));
        seam.push(boundary(2_000, "b.mp3"));

        seam.clear();

        assert!(!seam.is_pending());
        assert!(seam.crossed(u64::MAX).is_none());
    }

    /// Rebasing must not wrap a boundary around to an enormous number.
    #[test]
    fn rebasing_past_a_boundary_clamps_at_zero() {
        let mut seam = Seam::new();
        seam.push(boundary(100, "a.mp3"));

        seam.rebase(500);

        assert_eq!(seam.peek().unwrap().at_frame, 0);
        assert!(
            seam.crossed(0).is_some(),
            "a clamped boundary fires at once"
        );
    }

    #[test]
    fn an_empty_seam_reports_nothing() {
        let mut seam = Seam::new();
        assert!(seam.is_empty());
        assert!(seam.peek().is_none());
        assert!(seam.crossed(u64::MAX).is_none());
    }
}
