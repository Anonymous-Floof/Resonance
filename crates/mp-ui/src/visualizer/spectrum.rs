//! Frequency bars with falling peak markers.

use egui::{CornerRadius, Painter, Rect, Vec2};
use mp_audio::viz::Frame;
use mp_core::config::Visualizer as VizSettings;

use super::{Paint, vertical_gradient};
use crate::theme::{col, col_alpha};

/// Fraction of each bar's slot left as a gap.
const GAP: f32 = 0.22;

/// How tall a bar is when its band is silent, in pixels.
///
/// Bars that vanish entirely make the display look broken during quiet
/// passages. A visible floor reads as "nothing here right now".
const FLOOR_PX: f32 = 2.0;

/// Thickness of a peak cap.
const CAP_PX: f32 = 2.0;

pub fn draw(painter: &Painter, rect: Rect, frame: &Frame, paint: &Paint, settings: &VizSettings) {
    let count = frame.bars.len();
    if count == 0 {
        return;
    }

    let slot = rect.width() / count as f32;
    // Sub-pixel bars would alias into a shimmering mess, so at very high bar
    // counts in a narrow panel the gap is given up before the bar is.
    let gap = (slot * GAP).min(slot - 1.0).max(0.0);
    let width = (slot - gap).max(1.0);

    let full_height = rect.height();

    for index in 0..count {
        let (level, cap) = frame.bar(index);
        let t = if count > 1 {
            index as f32 / (count - 1) as f32
        } else {
            0.0
        };

        let left = rect.left() + slot * index as f32 + gap * 0.5;
        let height = (level * full_height).max(FLOOR_PX);

        let bar = Rect::from_min_size(
            egui::Pos2::new(left, rect.bottom() - height),
            Vec2::new(width, height),
        );

        // Bright at the top where the level is, fading into the background at
        // the base — so the eye reads the tip rather than the whole column.
        vertical_gradient(
            painter,
            bar,
            col(paint.at(t)),
            col_alpha(paint.base_at(t), 0.35),
        );

        if settings.show_peak_caps && cap > 0.001 {
            let y = rect.bottom() - (cap * full_height).max(FLOOR_PX);
            let cap_rect =
                Rect::from_min_size(egui::Pos2::new(left, y - CAP_PX), Vec2::new(width, CAP_PX));
            painter.rect_filled(
                cap_rect,
                CornerRadius::same(1),
                col_alpha(paint.at(t), 0.85),
            );
        }
    }
}
