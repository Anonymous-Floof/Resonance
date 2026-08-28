//! Themed backdrops for the shell's panels.
//!
//! The content area and the player bar can take their background from the
//! cover of whatever is playing, or from the visualiser, instead of being a
//! flat colour. That is a decoration, and decoration behind text has exactly
//! one job: to be noticed slightly and never to be in the way.
//!
//! So everything here is built around a ceiling rather than a slider that goes
//! to "unreadable". Even at full strength the art shows through at [`MAX_SHOW`]
//! — enough to tint the room, not enough to compete with a track listing. The
//! scrim is also stronger at the top of the content area, where the headings
//! are, and lighter at the bottom where there is usually nothing but list.
//!
//! ## Why the blur is free
//!
//! The album-art wash draws the *64 px* thumbnail stretched across the whole
//! panel. Upscaling that far through linear filtering is a blur — a good one,
//! smooth and cheap — and it means the effect costs one already-cached texture
//! rather than a shader or a second decode. Drawing the large cover instead
//! would be sharper, which is precisely wrong: a legible photograph behind a
//! list is noise, and a soft field of its colours is atmosphere.

use egui::{Rect, TextureHandle, Ui, Vec2};
use mp_core::color::Rgb;
use mp_core::config::SurfaceStyle;

use crate::theme::{col, col_alpha};
use crate::visualizer;

/// The most of the backdrop that is ever allowed to show through.
///
/// Past roughly a third, body text over a busy cover stops being comfortable
/// to read. The intensity setting scales within this, so the highest setting
/// is still a background.
const MAX_SHOW: f32 = 0.34;

/// Extra scrim where a panel's text sits, as a fraction of the shown strength.
const TEXT_WEIGHT: f32 = 0.35;

/// Fraction of a tall panel the visualiser rises through.
///
/// Kept low so lists stay clean: the movement belongs down near the player,
/// not behind the fourth row of a track listing.
const VIZ_HEIGHT: f32 = 0.55;

/// Below this height a panel is a bar, not a page.
///
/// The band exists to keep a column of list text clear. A player bar has no
/// such column, so reserving the top half of it achieves nothing and leaves
/// the visualiser a forty-pixel smear along the bottom.
const SHORT_PANEL: f32 = 220.0;

/// How much of the backdrop shows, given a user-facing 0..1 intensity.
pub fn strength(intensity: f32) -> f32 {
    intensity.clamp(0.0, 1.0) * MAX_SHOW
}

/// Paint the album-art wash into `rect`, over a base fill.
///
/// `texture` should be a small thumbnail — see the module note on why.
pub fn album_art(ui: &Ui, rect: Rect, base: Rgb, texture: &TextureHandle, intensity: f32) {
    let painter = ui.painter().with_clip_rect(rect);
    painter.rect_filled(rect, egui::CornerRadius::ZERO, col(base));

    if rect.width() <= 0.0 || rect.height() <= 0.0 {
        return;
    }

    // Cover rather than fit: a letterboxed backdrop with bars of flat colour
    // down the sides looks like a mistake.
    let art = cover(rect, texture.size_vec2());

    // Painted directly rather than through `egui::Image`, which needs a `Ui`
    // to size itself against and would fit the texture inside the rect instead
    // of letting it overflow. The clip above is what crops the overflow.
    painter.image(
        texture.id(),
        art,
        Rect::from_min_max(egui::Pos2::ZERO, egui::Pos2::new(1.0, 1.0)),
        egui::Color32::WHITE,
    );

    scrim(ui, rect, base, intensity);
}

/// Paint a scrim over whatever is already in `rect`.
///
/// Two layers: a flat one that sets the overall strength, and a second that
/// adds weight where the text is. Where that is depends on the shape of the
/// panel, which is why this is not simply one alpha.
pub fn scrim(ui: &Ui, rect: Rect, base: Rgb, intensity: f32) {
    let painter = ui.painter().with_clip_rect(rect);
    let show = strength(intensity);

    painter.rect_filled(rect, egui::CornerRadius::ZERO, col_alpha(base, 1.0 - show));

    let extra = col_alpha(base, show * TEXT_WEIGHT);

    if rect.height() <= SHORT_PANEL {
        // A player bar carries text and controls across its whole face, so the
        // extra weight goes everywhere. Fading it out downwards, as a page
        // does, would leave the seek bar and the clock sitting on the brightest
        // part of the visualiser.
        painter.rect_filled(rect, egui::CornerRadius::ZERO, extra);
        return;
    }

    // A page's headings are at the top, and the bottom is usually list. Fade
    // the extra weight out downwards so the backdrop is strongest where it can
    // least get in the way.
    let top = Rect::from_min_max(
        rect.min,
        egui::Pos2::new(rect.right(), rect.top() + rect.height() * 0.45),
    );
    visualizer::vertical_gradient(&painter, top, extra, col_alpha(base, 0.0));
}

/// The band of a content panel the visualiser is allowed to occupy.
///
/// Along the bottom, so a list reads against plain background and the movement
/// sits nearer the player it belongs to.
pub fn visualizer_band(rect: Rect) -> Rect {
    if rect.height() <= SHORT_PANEL {
        return rect;
    }

    Rect::from_min_max(
        egui::Pos2::new(rect.left(), rect.bottom() - rect.height() * VIZ_HEIGHT),
        rect.max,
    )
}

/// The largest rectangle of `aspect`'s proportions that *covers* `bounds`,
/// centred. The overflow is clipped by the caller.
fn cover(bounds: Rect, aspect: Vec2) -> Rect {
    if aspect.x <= 0.0 || aspect.y <= 0.0 {
        return bounds;
    }

    let scale = (bounds.width() / aspect.x).max(bounds.height() / aspect.y);
    Rect::from_center_size(bounds.center(), aspect * scale)
}

/// Whether a style needs the visualiser to be analysed this frame.
pub fn needs_visualizer(styles: [SurfaceStyle; 2]) -> bool {
    styles.contains(&SurfaceStyle::Visualizer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(w: f32, h: f32) -> Rect {
        Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(w, h))
    }

    /// The ceiling is the whole safety argument: at any setting the panel is
    /// still mostly its own colour.
    #[test]
    fn the_backdrop_never_takes_over() {
        assert!((strength(1.0) - MAX_SHOW).abs() < 1e-6);
        assert!(strength(1.0) < 0.5, "a backdrop must stay a background");

        assert_eq!(strength(0.0), 0.0);
        // Out-of-range values from a hand-edited config are clamped, not
        // trusted.
        assert_eq!(strength(-4.0), 0.0);
        assert!((strength(9.0) - MAX_SHOW).abs() < 1e-6);
    }

    #[test]
    fn strength_rises_with_the_setting() {
        assert!(strength(0.25) < strength(0.5));
        assert!(strength(0.5) < strength(1.0));
    }

    /// Covering, not fitting: no bars of flat colour down the sides.
    #[test]
    fn a_cover_fills_the_whole_area() {
        let bounds = rect(1000.0, 400.0);

        for aspect in [
            Vec2::new(1.0, 1.0),
            Vec2::new(4.0, 3.0),
            Vec2::new(9.0, 16.0),
        ] {
            let drawn = cover(bounds, aspect);

            assert!(
                drawn.width() >= bounds.width() - 0.01 && drawn.height() >= bounds.height() - 0.01,
                "{aspect:?} left a gap: {drawn:?} inside {bounds:?}"
            );
            assert!(
                (drawn.center() - bounds.center()).length() < 0.01,
                "the crop should be centred"
            );

            // And the proportions are kept, or the cover is stretched.
            let ratio = drawn.width() / drawn.height();
            assert!((ratio - aspect.x / aspect.y).abs() < 0.01);
        }
    }

    #[test]
    fn a_degenerate_texture_does_not_divide_by_zero() {
        let bounds = rect(100.0, 100.0);
        assert_eq!(cover(bounds, Vec2::ZERO), bounds);
        assert_eq!(cover(bounds, Vec2::new(-1.0, 5.0)), bounds);
    }

    /// In a tall panel the visualiser stays out of the top of the list.
    #[test]
    fn the_visualizer_band_hugs_the_bottom_of_a_page() {
        let bounds = rect(800.0, 600.0);
        let band = visualizer_band(bounds);

        assert_eq!(band.bottom(), bounds.bottom());
        assert!(
            band.top() > bounds.top(),
            "it should not reach the headings"
        );
        assert!(band.height() < bounds.height());
    }

    /// A player bar has no list above it to protect, so it gets all of itself.
    /// Half of a ninety-pixel bar is not a visualiser, it is a smudge.
    #[test]
    fn a_short_bar_gets_its_whole_height() {
        let bar = rect(1400.0, 90.0);
        assert_eq!(visualizer_band(bar), bar);

        // And the switch happens somewhere sensible between the two.
        assert_eq!(visualizer_band(rect(1400.0, 200.0)), rect(1400.0, 200.0));
        assert!(visualizer_band(rect(1400.0, 500.0)).height() < 500.0);
    }

    #[test]
    fn the_visualizer_is_only_analysed_when_something_wants_it() {
        assert!(!needs_visualizer([
            SurfaceStyle::Solid,
            SurfaceStyle::Solid
        ]));
        assert!(!needs_visualizer([
            SurfaceStyle::Solid,
            SurfaceStyle::AlbumArt
        ]));
        assert!(needs_visualizer([
            SurfaceStyle::Solid,
            SurfaceStyle::Visualizer
        ]));
        assert!(needs_visualizer([
            SurfaceStyle::Visualizer,
            SurfaceStyle::AlbumArt
        ]));
    }
}
