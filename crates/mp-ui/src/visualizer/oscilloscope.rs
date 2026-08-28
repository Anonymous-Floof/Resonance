//! The waveform itself, trigger-aligned so it sits still.
//!
//! The alignment happens in the analyzer; all this has to do is draw the window
//! it is handed. Everything here is about making a one-pixel polyline look like
//! something rather than like a debug plot.

use egui::{Painter, Pos2, Rect, Shape, Stroke};
use mp_audio::viz::Frame;

use super::Paint;
use crate::theme::col_alpha;

/// How much of the panel height a full-scale signal uses.
///
/// Short of the full height on purpose: a track mastered right up to 0 dBFS
/// should look loud, not clipped off by the edge of its own panel.
const HEADROOM: f32 = 0.86;

/// Points drawn across the panel.
///
/// More than a couple of thousand is wasted — the panel does not have that many
/// pixels, and every extra point is another line segment to tessellate.
const MAX_POINTS: usize = 1024;

pub fn draw(painter: &Painter, rect: Rect, frame: &Frame, paint: &Paint) {
    let colour = paint.primary();
    let centre = rect.center().y;

    // The zero line, always drawn — it makes silence read as silence rather
    // than as a panel that failed to paint.
    painter.line_segment(
        [
            Pos2::new(rect.left(), centre),
            Pos2::new(rect.right(), centre),
        ],
        Stroke::new(1.0, col_alpha(colour, 0.18)),
    );

    let wave = &frame.wave;
    if wave.is_empty() || !frame.active {
        return;
    }

    // One point per pixel column at most; beyond that the extra detail lands
    // between pixels and only costs tessellation.
    let points = MAX_POINTS
        .min(wave.len())
        .min(rect.width().ceil() as usize * 2)
        .max(2);
    let half = rect.height() * 0.5 * HEADROOM;

    let mut path = Vec::with_capacity(points);
    for index in 0..points {
        let position = index as f32 / (points - 1) as f32;

        // Nearest sample rather than an interpolation: the window is longer
        // than the panel is wide, so this is decimation, and averaging would
        // quietly turn a loud high-frequency passage into a flat line.
        let sample_index = ((position * (wave.len() - 1) as f32) as usize).min(wave.len() - 1);
        let value = wave[sample_index].clamp(-1.0, 1.0);

        path.push(Pos2::new(
            rect.left() + rect.width() * position,
            centre - value * half,
        ));
    }

    // Drawn three times at decreasing width and increasing opacity. egui has
    // no blur, and a plain one-pixel line looks thin and clinical against a
    // dark panel; the stacked strokes read as a glow.
    for (width, alpha) in [(6.0, 0.10), (3.0, 0.22), (1.6, 1.0)] {
        painter.add(Shape::line(
            path.clone(),
            Stroke::new(width, col_alpha(colour, alpha)),
        ));
    }
}
