//! A spectrum wrapped into a ring.
//!
//! The plan puts this around the album art in full-screen Now Playing, so it is
//! built as a ring with an empty middle: whatever is behind it shows through.

use egui::{Painter, Pos2, Rect, Shape, Stroke};
use mp_audio::viz::Frame;

use super::Paint;
use crate::theme::{col, col_alpha};

/// Fraction of the available radius the ring starts at.
const INNER: f32 = 0.42;

/// How far past the ring a full-scale band reaches, as a fraction of radius.
const REACH: f32 = 0.42;

/// Where the low frequencies sit, in radians from straight up.
///
/// Starting at the top and running clockwise puts the bass where the eye lands
/// first, and makes the ring readable as a spectrum rather than as decoration.
const START_ANGLE: f32 = -std::f32::consts::FRAC_PI_2;

pub fn draw(painter: &Painter, rect: Rect, frame: &Frame, paint: &Paint) {
    let count = frame.bars.len();
    if count == 0 {
        return;
    }

    let centre = rect.center();
    let radius = rect.width().min(rect.height()) * 0.5;
    if radius < 8.0 {
        return;
    }

    let inner = radius * INNER;
    let reach = radius * REACH;

    // The ring itself pulses with the bass, which is what makes the whole
    // thing feel driven by the music rather than by the top end alone.
    let pulse = inner * (1.0 + frame.bass * 0.06 + frame.onset * 0.05);

    painter.circle_stroke(
        centre,
        pulse,
        Stroke::new(1.5, col_alpha(paint.primary(), 0.25 + frame.onset * 0.35)),
    );

    // Spokes are drawn as line segments rather than wedges: at sixty-four
    // bands a wedge is only a few pixels wide at the inner edge, so the
    // difference is invisible and a segment is a quarter of the geometry.
    let step = std::f32::consts::TAU / count as f32;
    let thickness = ((std::f32::consts::TAU * inner / count as f32) * 0.65).clamp(1.0, 14.0);

    for index in 0..count {
        let (level, cap) = frame.bar(index);
        let t = if count > 1 {
            index as f32 / (count - 1) as f32
        } else {
            0.0
        };

        let angle = START_ANGLE + step * index as f32;
        let (sin, cos) = angle.sin_cos();
        let direction = egui::Vec2::new(cos, sin);

        let from = centre + direction * pulse;
        let to = centre + direction * (pulse + level * reach);

        painter.add(Shape::LineSegment {
            points: [from, to],
            stroke: Stroke::new(thickness, col(paint.at(t))),
        });

        // A dot at the peak, the ring's equivalent of the spectrum's caps.
        if cap > 0.02 {
            let mark: Pos2 = centre + direction * (pulse + cap * reach);
            painter.circle_filled(
                mark,
                (thickness * 0.35).max(1.0),
                col_alpha(paint.at(t), 0.8),
            );
        }
    }
}
