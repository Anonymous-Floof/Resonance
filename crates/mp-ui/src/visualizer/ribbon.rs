//! Scrolling amplitude history.
//!
//! Unlike the other three this one has memory: it shows the last few seconds
//! rather than the last few milliseconds, so it needs its own buffer that
//! survives between frames.
//!
//! The history is advanced on a clock rather than once per repaint. A repaint
//! happens whenever egui feels like one — a mouse move, a tooltip, a window
//! resize — so pushing per repaint would make the ribbon scroll at a speed set
//! by how much the user was fidgeting.

use egui::{Painter, Pos2, Rect};
use mp_audio::viz::Frame;

use super::{Paint, filled_band};
use crate::theme::col_alpha;

/// Columns kept. At the rate below this is about six seconds.
const COLUMNS: usize = 360;

/// How often a new column is taken, in seconds.
const INTERVAL: f32 = 1.0 / 60.0;

/// How much of the panel height a full-scale signal uses.
const HEADROOM: f32 = 0.88;

/// One column: how far the signal swung while it was being recorded.
#[derive(Debug, Clone, Copy, Default)]
struct Column {
    /// Peak absolute level over the interval, `0.0..=1.0`.
    peak: f32,
    /// Loudness over the interval, `0.0..=1.0`. Drawn as the bright core.
    rms: f32,
}

pub struct History {
    columns: Vec<Column>,
    /// Where the next column goes; everything before it is older.
    write: usize,
    /// Time owed since the last column was taken.
    accumulated: f32,
    /// Largest values seen since the last column was taken.
    pending: Column,
}

impl History {
    pub fn new() -> Self {
        Self {
            columns: vec![Column::default(); COLUMNS],
            write: 0,
            accumulated: 0.0,
            pending: Column::default(),
        }
    }

    /// Feed the newest analysis in and advance the scroll.
    ///
    /// Takes the *maximum* over the frames between columns rather than the
    /// last one. Sampling would drop transients whenever a repaint happened to
    /// land between two drum hits, which is exactly the detail worth keeping.
    pub fn push(&mut self, frame: &Frame, dt: f32) {
        let peak = if frame.active {
            frame.peak.min(1.0)
        } else {
            0.0
        };
        let rms = if frame.active {
            frame.rms.min(1.0)
        } else {
            0.0
        };

        self.pending.peak = self.pending.peak.max(peak);
        self.pending.rms = self.pending.rms.max(rms);

        // A long stall should not fast-forward the ribbon by a hundred
        // identical columns, so the catch-up is bounded.
        self.accumulated = (self.accumulated + dt).min(INTERVAL * 8.0);

        while self.accumulated >= INTERVAL {
            self.accumulated -= INTERVAL;
            self.columns[self.write] = self.pending;
            self.write = (self.write + 1) % self.columns.len();
            self.pending = Column::default();
        }
    }

    pub fn draw(&self, painter: &Painter, rect: Rect, paint: &Paint) {
        let count = self.columns.len();
        let centre = rect.center().y;
        let half = rect.height() * 0.5 * HEADROOM;

        // One point per pixel column at most.
        let points = count.min(rect.width().ceil() as usize).max(2);

        let mut top = Vec::with_capacity(points);
        let mut bottom = Vec::with_capacity(points);

        for index in 0..points {
            let position = index as f32 / (points - 1) as f32;
            let x = rect.left() + rect.width() * position;

            // Oldest on the left, newest on the right, so the ribbon flows the
            // way time reads.
            let age = ((1.0 - position) * (count - 1) as f32) as usize;
            let column = self.columns[(self.write + count - 1 - age.min(count - 1)) % count];

            let amplitude = column.peak * half;
            top.push(Pos2::new(x, centre - amplitude));
            bottom.push(Pos2::new(x, centre + amplitude));
        }

        filled_band(
            painter,
            &top,
            &bottom,
            col_alpha(paint.primary(), 0.22),
            col_alpha(paint.primary(), 0.85),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(peak: f32) -> Frame {
        Frame {
            peak,
            rms: peak * 0.7,
            active: true,
            ..Frame::default()
        }
    }

    #[test]
    fn columns_advance_on_the_clock_not_on_the_repaint() {
        let mut history = History::new();

        // Twenty repaints inside a single column's worth of time.
        for _ in 0..20 {
            history.push(&frame(0.5), INTERVAL / 20.0);
        }

        assert_eq!(
            history.write, 1,
            "twenty repaints produced more than one column"
        );
    }

    /// A transient between repaints has to survive into the column.
    #[test]
    fn a_column_keeps_the_loudest_frame_it_saw() {
        let mut history = History::new();

        history.push(&frame(0.1), INTERVAL / 3.0);
        history.push(&frame(0.9), INTERVAL / 3.0);
        history.push(&frame(0.2), INTERVAL / 3.0);

        assert_eq!(history.columns[0].peak, 0.9);
    }

    #[test]
    fn a_long_stall_does_not_fast_forward_the_whole_ribbon() {
        let mut history = History::new();

        history.push(&frame(0.5), 30.0);

        assert!(
            history.write <= 8,
            "a thirty-second stall advanced {} columns",
            history.write
        );
    }

    #[test]
    fn silence_records_silence() {
        let mut history = History::new();

        let quiet = Frame {
            peak: 0.8,
            rms: 0.8,
            active: false,
            ..Frame::default()
        };
        history.push(&quiet, INTERVAL);

        assert_eq!(history.columns[0].peak, 0.0);
    }

    /// The ring wraps; the buffer must stay in range at every index.
    #[test]
    fn the_history_ring_wraps_without_running_off_the_end() {
        let mut history = History::new();

        for index in 0..COLUMNS * 3 {
            history.push(&frame((index % 10) as f32 / 10.0), INTERVAL);
        }

        assert!(history.write < COLUMNS);
        assert_eq!(history.columns.len(), COLUMNS);
    }
}
