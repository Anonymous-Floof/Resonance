//! Custom-painted widgets.
//!
//! egui's stock button is a bordered rectangle, which is the look we are
//! specifically trying to avoid. These paint their own backgrounds and animate
//! hover/press states, pulling every colour from the [`Theme`] so nothing
//! hard-codes a shade.

pub mod icons;

use egui::{
    Align, Align2, Color32, CornerRadius, Layout, Rect, Response, Sense, TextStyle, Ui, Vec2,
};

use crate::theme::{Theme, col, col_alpha};
use icons::Icon;

/// How long hover/press transitions take. Short enough to feel instant,
/// long enough to read as a transition rather than a flicker.
const HOVER_FADE: f32 = 0.12;

/// A square, borderless icon button, labelled by its icon.
///
/// `active` marks a toggled-on state (shuffle enabled, queue panel open) and
/// tints the icon with the accent colour.
pub fn icon_button(ui: &mut Ui, theme: &Theme, icon: Icon, size: f32, active: bool) -> Response {
    icon_button_labelled(ui, theme, icon, size, active, icon.label())
}

/// An icon button whose tooltip says something more specific than the icon's
/// own name.
///
/// This exists because `on_hover_text` *appends*: calling it on the response
/// from a button that already carries a tooltip stacks a second line under the
/// first rather than replacing it. The equalizer button read "Equalizer" over
/// "Equalizer". Passing the text in is the only way to have exactly one.
pub fn icon_button_labelled(
    ui: &mut Ui,
    theme: &Theme,
    icon: Icon,
    size: f32,
    active: bool,
    tooltip: &str,
) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());

    if ui.is_rect_visible(rect) {
        let ctx = ui.ctx();
        let hover =
            ctx.animate_bool_with_time(response.id.with("hover"), response.hovered(), HOVER_FADE);
        let press = ctx.animate_bool_with_time(
            response.id.with("press"),
            response.is_pointer_button_down_on(),
            HOVER_FADE * 0.5,
        );

        let p = &theme.palette;

        // Background only appears on interaction; at rest the button is just
        // its glyph.
        if hover > 0.0 {
            ui.painter().circle_filled(
                rect.center(),
                size * 0.5,
                col_alpha(p.bg_hover, hover * 0.9),
            );
        }

        let tint = if active { p.accent } else { p.text_secondary };
        let colour = if hover > 0.0 && !active {
            tint.mix(p.text_primary, hover)
        } else {
            tint
        };

        // Pressing nudges the glyph down a touch, which reads as physical.
        let icon_rect = rect.translate(Vec2::new(0.0, press * 1.0));
        icons::draw(
            ui.painter(),
            icon,
            icon_rect,
            col(colour),
            (size * 0.075).max(1.25),
        );
    }

    response.on_hover_text(tooltip)
}

/// The primary circular transport button (play/pause), filled with the accent.
pub fn transport_button(ui: &mut Ui, theme: &Theme, icon: Icon, size: f32) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());

    if ui.is_rect_visible(rect) {
        let ctx = ui.ctx();
        let hover =
            ctx.animate_bool_with_time(response.id.with("hover"), response.hovered(), HOVER_FADE);
        let press = ctx.animate_bool_with_time(
            response.id.with("press"),
            response.is_pointer_button_down_on(),
            HOVER_FADE * 0.5,
        );

        let p = &theme.palette;
        // Grow on hover, shrink slightly when pressed.
        let radius = size * 0.5 * (1.0 + hover * 0.04 - press * 0.06);
        let fill = p.accent.mix(p.accent_hover, hover);

        ui.painter().circle_filled(rect.center(), radius, col(fill));
        icons::draw(
            ui.painter(),
            icon,
            rect.shrink(size * 0.28),
            col(p.accent_contrast),
            (size * 0.07).max(1.5),
        );
    }

    response.on_hover_text(icon.label())
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// How large the secondary buttons are relative to the play button.
///
/// One ratio, applied everywhere, so the cluster keeps its proportions at any
/// size instead of each screen picking its own pair of numbers.
const SECONDARY_SCALE: f32 = 0.64;

/// Which optional controls a transport cluster carries.
#[derive(Debug, Clone, Copy)]
pub struct Transport {
    pub playing: bool,
    /// Diameter of the play button. Everything else is derived from it.
    pub size: f32,
    /// `Some(on)` to show shuffle, `None` to leave it out.
    pub shuffle: Option<bool>,
    /// `Some((icon, on))` to show repeat.
    pub repeat: Option<(Icon, bool)>,
}

impl Transport {
    /// Just the three buttons, for a screen with no room for the rest.
    pub fn minimal(playing: bool, size: f32) -> Self {
        Self {
            playing,
            size,
            shuffle: None,
            repeat: None,
        }
    }

    /// The width the cluster will occupy, so a caller can centre it.
    pub fn width(&self, theme: &Theme) -> f32 {
        let m = &theme.metrics;
        let secondary = self.size * SECONDARY_SCALE;

        let mut width = self.size + secondary * 2.0 + m.space(1.5) * 2.0;

        if self.shuffle.is_some() {
            width += secondary + m.space(1.0);
        }
        if self.repeat.is_some() {
            width += secondary + m.space(1.0);
        }

        width
    }
}

/// What the user pressed in the transport.
#[derive(Debug, Default, Clone, Copy)]
pub struct TransportHit {
    pub toggle_play: bool,
    pub next: bool,
    pub previous: bool,
    pub shuffle: bool,
    pub repeat: bool,
}

/// The play/pause cluster, drawn the same way everywhere it appears.
///
/// One function rather than a copy per screen, because the two copies had
/// already drifted: the player bar paired a 24 px skip button with a 36 px play
/// button, and the full-screen view paired 28 with 44.
///
/// The row's height is set explicitly, which is the whole reason the buttons
/// line up. `Ui::horizontal` gives its row `spacing.interact_size.y` — a fixed
/// 32 px here — and centres each child against *that* rather than against the
/// tallest child. Anything larger than 32 px overflowed and was pinned to the
/// top of the row, so the play button sat two pixels below its neighbours in
/// the player bar and six below them full-screen. Allocating a row as tall as
/// the largest button gives `Align::Center` something true to centre against.
pub fn transport(ui: &mut Ui, theme: &Theme, spec: Transport) -> TransportHit {
    let m = &theme.metrics;
    let mut hit = TransportHit::default();

    let secondary = spec.size * SECONDARY_SCALE;

    ui.allocate_ui_with_layout(
        Vec2::new(ui.available_width(), spec.size),
        Layout::left_to_right(Align::Center),
        |ui| {
            // Centre the cluster in whatever width it was given.
            let slack = (ui.available_width() - spec.width(theme)) * 0.5;
            ui.add_space(slack.max(0.0));

            if let Some(on) = spec.shuffle {
                hit.shuffle = icon_button(ui, theme, Icon::Shuffle, secondary, on).clicked();
                ui.add_space(m.space(1.0));
            }

            hit.previous = icon_button(ui, theme, Icon::Previous, secondary, false).clicked();

            ui.add_space(m.space(1.5));
            let icon = if spec.playing {
                Icon::Pause
            } else {
                Icon::Play
            };
            hit.toggle_play = transport_button(ui, theme, icon, spec.size).clicked();
            ui.add_space(m.space(1.5));

            hit.next = icon_button(ui, theme, Icon::Next, secondary, false).clicked();

            if let Some((icon, on)) = spec.repeat {
                ui.add_space(m.space(1.0));
                hit.repeat = icon_button(ui, theme, icon, secondary, on).clicked();
            }
        },
    );

    hit
}

/// Whether a row was activated, under whichever click setting is in force.
///
/// Shared so every list agrees, and so the hover text and the behaviour can be
/// derived from the same flag rather than drifting apart.
///
/// In single-click mode a genuine double-click reports `clicked` on the first
/// press and `double_clicked` on the second. Only the first is taken, so a
/// habitual double-click starts the track once instead of restarting it.
pub fn row_activated(response: &Response, single_click: bool) -> bool {
    if single_click {
        response.clicked()
    } else {
        response.double_clicked()
    }
}

/// The hover text that matches [`row_activated`].
pub fn activate_hint(single_click: bool) -> &'static str {
    if single_click {
        "Click to play"
    } else {
        "Double-click to play"
    }
}

/// One row in the left nav rail.
///
/// When `collapsed` the label is dropped and the icon centres, so the rail can
/// narrow without the rows reflowing awkwardly.
pub fn nav_item(
    ui: &mut Ui,
    theme: &Theme,
    icon: Icon,
    label: &str,
    selected: bool,
    collapsed: bool,
) -> Response {
    let m = &theme.metrics;
    let height = m.space(5.0);
    let width = ui.available_width();

    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click());

    if ui.is_rect_visible(rect) {
        let ctx = ui.ctx();
        let hover =
            ctx.animate_bool_with_time(response.id.with("hover"), response.hovered(), HOVER_FADE);
        let select =
            ctx.animate_bool_with_time(response.id.with("sel"), selected, HOVER_FADE * 1.5);

        let p = &theme.palette;
        let painter = ui.painter();

        // Selection is a filled pill; hover is a fainter version of the same
        // shape so the two states feel related.
        if select > 0.0 || hover > 0.0 {
            let alpha = (select * 0.16).max(hover * 0.09);
            painter.rect_filled(
                rect,
                CornerRadius::same(m.radius_medium),
                col_alpha(p.accent, alpha),
            );
        }

        // A short accent bar on the left edge marks the active view.
        if select > 0.0 {
            let bar_height = rect.height() * 0.45 * select;
            let bar = Rect::from_center_size(
                egui::Pos2::new(rect.left() + 2.0, rect.center().y),
                Vec2::new(3.0, bar_height),
            );
            painter.rect_filled(bar, CornerRadius::same(2), col(p.accent));
        }

        let content = if selected {
            p.text_primary
        } else {
            p.text_secondary.mix(p.text_primary, hover)
        };
        let icon_colour = if selected { p.accent } else { content };

        let icon_size = m.space(2.5);
        let icon_rect = if collapsed {
            Rect::from_center_size(rect.center(), Vec2::splat(icon_size))
        } else {
            Rect::from_center_size(
                egui::Pos2::new(rect.left() + m.space(2.25), rect.center().y),
                Vec2::splat(icon_size),
            )
        };

        icons::draw(painter, icon, icon_rect, col(icon_colour), 1.6);

        if !collapsed {
            let font = TextStyle::Name("nav".into()).resolve(ui.style());
            painter.text(
                egui::Pos2::new(rect.left() + m.space(4.5), rect.center().y),
                Align2::LEFT_CENTER,
                label,
                font,
                col(content),
            );
        }
    }

    if collapsed {
        response.on_hover_text(label)
    } else {
        response
    }
}

/// A small uppercase heading used to separate groups in the nav rail.
pub fn nav_section_label(ui: &mut Ui, theme: &Theme, text: &str) {
    let m = &theme.metrics;
    ui.add_space(m.space(1.5));

    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), m.space(2.0)),
        Sense::hover(),
    );

    if ui.is_rect_visible(rect) {
        let font = TextStyle::Name("caption".into()).resolve(ui.style());
        ui.painter().text(
            egui::Pos2::new(rect.left() + m.space(2.25), rect.center().y),
            Align2::LEFT_CENTER,
            text.to_uppercase(),
            font,
            col(theme.palette.text_muted),
        );
    }
}

/// Centred placeholder for a view with nothing in it yet.
///
/// Every list view starts empty until a library is scanned, so this carries
/// real weight in the shell rather than being a stopgap.
pub fn empty_state(ui: &mut Ui, theme: &Theme, icon: Icon, title: &str, body: &str) {
    let m = &theme.metrics;
    let p = &theme.palette;

    ui.vertical_centered(|ui| {
        ui.add_space(m.space(10.0));

        let icon_size = m.space(7.0);
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(icon_size), Sense::hover());
        if ui.is_rect_visible(rect) {
            ui.painter()
                .circle_filled(rect.center(), icon_size * 0.5, col_alpha(p.accent, 0.10));
            icons::draw(
                ui.painter(),
                icon,
                rect.shrink(icon_size * 0.28),
                col(p.accent),
                2.0,
            );
        }

        ui.add_space(m.space(2.0));
        ui.label(
            egui::RichText::new(title)
                .text_style(TextStyle::Name("title".into()))
                .color(col(p.text_primary)),
        );
        ui.add_space(m.space(0.5));
        ui.label(
            egui::RichText::new(body)
                .text_style(TextStyle::Name("subtitle".into()))
                .color(col(p.text_muted)),
        );
    });
}

/// A filled accent button for primary actions.
pub fn accent_button(ui: &mut Ui, theme: &Theme, label: &str) -> Response {
    let m = &theme.metrics;
    let font = TextStyle::Button.resolve(ui.style());

    let text_size = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font.clone(), Color32::WHITE)
        .size();
    let size = Vec2::new(
        text_size.x + m.space(3.5),
        (text_size.y + m.space(1.75)).max(m.space(4.0)),
    );

    let (rect, response) = ui.allocate_exact_size(size, Sense::click());

    if ui.is_rect_visible(rect) {
        let ctx = ui.ctx();
        let hover =
            ctx.animate_bool_with_time(response.id.with("hover"), response.hovered(), HOVER_FADE);
        let p = &theme.palette;

        ui.painter().rect_filled(
            rect,
            CornerRadius::same(m.radius_medium),
            col(p.accent.mix(p.accent_hover, hover)),
        );
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            label,
            font,
            col(p.accent_contrast),
        );
    }

    response
}

/// A horizontal track with a filled portion and a handle that appears on hover.
///
/// Used for both the seek bar and the volume slider. Returns `Some(fraction)`
/// while the user is dragging or has just clicked, and `None` otherwise, so the
/// caller can distinguish "user is scrubbing" from "playback advanced".
///
/// `enabled` false renders the track greyed and ignores input - the seek bar
/// looks like this until something is actually playing.
pub fn scrubber(
    ui: &mut Ui,
    theme: &Theme,
    fraction: f32,
    width: f32,
    enabled: bool,
) -> (Response, Option<f32>) {
    let m = &theme.metrics;
    let p = &theme.palette;

    let height = m.space(2.0);
    let sense = if enabled {
        Sense::click_and_drag()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), sense);

    // The visible track is thinner than the hit area: a 4px bar is hard to
    // grab, but a 16px hit target is comfortable.
    let track_height = 4.0;
    let track = Rect::from_center_size(rect.center(), Vec2::new(rect.width(), track_height));

    let hover = ui.ctx().animate_bool_with_time(
        response.id.with("hover"),
        response.hovered() && enabled,
        HOVER_FADE,
    );

    let mut seek = None;
    let mut shown = fraction.clamp(0.0, 1.0);

    if enabled
        && (response.dragged() || response.clicked())
        && let Some(pos) = response.interact_pointer_pos()
    {
        let f = ((pos.x - track.left()) / track.width()).clamp(0.0, 1.0);
        shown = f;
        seek = Some(f);
    }

    if ui.is_rect_visible(rect) {
        let radius = CornerRadius::same((track_height * 0.5) as u8);
        let painter = ui.painter();

        painter.rect_filled(track, radius, col_alpha(p.text_muted, 0.30));

        let filled_width = track.width() * shown;
        if filled_width > 0.0 {
            let filled = Rect::from_min_size(track.min, Vec2::new(filled_width, track.height()));
            let fill = if enabled { p.accent } else { p.text_muted };
            painter.rect_filled(filled, radius, col(fill));
        }

        // Handle fades in on hover rather than always sitting there.
        if hover > 0.0 {
            let centre = egui::Pos2::new(track.left() + filled_width, track.center().y);
            painter.circle_filled(centre, 5.0 * hover, col(p.accent));
            painter.circle_filled(centre, 2.5 * hover, col(p.bg_base));
        }
    }

    (response, seek)
}

/// A horizontal rule matching the theme's border colour.
pub fn separator(ui: &mut Ui, theme: &Theme) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
    if ui.is_rect_visible(rect) {
        ui.painter()
            .rect_filled(rect, CornerRadius::ZERO, col(theme.palette.border));
    }
}

/// Format a duration the way a player should: `3:07`, or `1:02:14` past an hour.
pub fn format_duration(seconds: f64) -> String {
    if !seconds.is_finite() || seconds < 0.0 {
        return "--:--".to_owned();
    }

    let total = seconds.round() as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);

    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::theme::Theme;

    fn theme() -> Theme {
        Theme::new(&mp_core::config::Appearance::default(), None)
    }

    /// The secondary buttons are sized from the primary rather than picked
    /// per screen, which is what let the two copies drift apart.
    #[test]
    fn the_cluster_grows_in_proportion() {
        let theme = theme();

        let small = Transport::minimal(false, 32.0).width(&theme);
        let large = Transport::minimal(false, 64.0).width(&theme);

        assert!(large > small, "a bigger play button needs a wider cluster");
    }

    /// The optional buttons have to be in the measured width, or the caller
    /// centres the cluster against the wrong number and it sits off to one
    /// side.
    #[test]
    fn optional_buttons_are_counted_in_the_width() {
        let theme = theme();

        let bare = Transport::minimal(false, 40.0);
        let full = Transport {
            shuffle: Some(false),
            repeat: Some((Icon::Repeat, false)),
            ..bare
        };

        let bare_width = bare.width(&theme);
        let full_width = full.width(&theme);

        assert!(
            full_width > bare_width,
            "five buttons should measure wider than three ({full_width} vs {bare_width})"
        );

        // Two extra buttons, each with its own gap.
        let secondary = 40.0 * SECONDARY_SCALE;
        let expected = bare_width + (secondary + theme.metrics.space(1.0)) * 2.0;
        assert!((full_width - expected).abs() < 0.01);
    }

    #[test]
    fn shuffle_and_repeat_are_independent() {
        let theme = theme();
        let base = Transport::minimal(false, 40.0);

        let only_shuffle = Transport {
            shuffle: Some(true),
            ..base
        };
        let only_repeat = Transport {
            repeat: Some((Icon::Repeat, true)),
            ..base
        };

        assert!((only_shuffle.width(&theme) - only_repeat.width(&theme)).abs() < 0.01);
        assert!(only_shuffle.width(&theme) > base.width(&theme));
    }

    #[test]
    fn a_minimal_transport_carries_no_optional_buttons() {
        let spec = Transport::minimal(true, 40.0);

        assert!(spec.playing);
        assert!(spec.shuffle.is_none());
        assert!(spec.repeat.is_none());
    }

    #[test]
    fn durations_use_minute_seconds_below_an_hour() {
        assert_eq!(format_duration(0.0), "0:00");
        assert_eq!(format_duration(7.0), "0:07");
        assert_eq!(format_duration(67.0), "1:07");
        assert_eq!(format_duration(187.0), "3:07");
        assert_eq!(format_duration(3599.0), "59:59");
    }

    #[test]
    fn durations_add_an_hour_field_when_needed() {
        assert_eq!(format_duration(3600.0), "1:00:00");
        assert_eq!(format_duration(3734.0), "1:02:14");
        // Long DJ sets and mixes are a real case for this player.
        assert_eq!(format_duration(7325.0), "2:02:05");
    }

    #[test]
    fn durations_round_rather_than_truncate() {
        assert_eq!(format_duration(186.6), "3:07");
        assert_eq!(format_duration(59.5), "1:00");
    }

    #[test]
    fn invalid_durations_degrade_to_a_placeholder() {
        // Unknown length is normal before a stream reports its duration.
        assert_eq!(format_duration(-1.0), "--:--");
        assert_eq!(format_duration(f64::NAN), "--:--");
        assert_eq!(format_duration(f64::INFINITY), "--:--");
    }
}
