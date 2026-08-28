//! The visual identity of the app.
//!
//! A [`Palette`] is a small set of semantic colour roles and a [`Metrics`] is
//! the matching spacing/radius scale. Together they generate a complete
//! `egui::Style`, so every widget inherits the look without call sites hand
//! picking colours. When something needs a colour it asks for a role
//! (`palette.text_muted`), never a literal.
//!
//! This lands before any audio code on purpose: retrofitting a look onto egui
//! is far harder than building against one from the start.

use egui::{Color32, CornerRadius, Margin, Shadow, Stroke, TextStyle, Vec2};
use mp_core::color::Rgb;
use mp_core::config::{Appearance, Density, SurfaceStyle, ThemeMode};

use crate::fonts::STRONG_FAMILY;

/// Minimum contrast we hold text to against its background.
const MIN_TEXT_CONTRAST: f32 = 4.5;
/// Minimum contrast for accent used as a fill or focus ring.
const MIN_ACCENT_CONTRAST: f32 = 3.0;

// ---------------------------------------------------------------------------
// Palette
// ---------------------------------------------------------------------------

/// Semantic colour roles. Layered backgrounds give depth without borders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub dark_mode: bool,

    /// The window background, furthest back.
    pub bg_base: Rgb,
    /// Panels sitting on the base: nav rail, player bar.
    pub bg_surface: Rgb,
    /// Cards, popups, menus - the layer nearest the viewer.
    pub bg_elevated: Rgb,
    /// Row hover.
    pub bg_hover: Rgb,
    /// Row pressed / selected.
    pub bg_active: Rgb,

    pub text_primary: Rgb,
    pub text_secondary: Rgb,
    pub text_muted: Rgb,

    pub accent: Rgb,
    pub accent_hover: Rgb,
    /// Readable when placed on top of `accent`.
    pub accent_contrast: Rgb,

    pub border: Rgb,
    pub border_strong: Rgb,

    pub success: Rgb,
    pub warning: Rgb,
    pub error: Rgb,
}

impl Palette {
    /// The default dark shell: near-black with a faint blue cast.
    pub fn dark(accent: Rgb) -> Self {
        let bg_base = Rgb::new(0x0E, 0x0E, 0x12);
        let accent = accent.ensure_contrast(bg_base, MIN_ACCENT_CONTRAST);

        Self {
            dark_mode: true,
            bg_base,
            bg_surface: Rgb::new(0x15, 0x15, 0x1B),
            bg_elevated: Rgb::new(0x1D, 0x1D, 0x25),
            bg_hover: Rgb::new(0x26, 0x26, 0x30),
            bg_active: Rgb::new(0x2F, 0x2F, 0x3B),

            text_primary: Rgb::new(0xF2, 0xF2, 0xF5),
            text_secondary: Rgb::new(0xA8, 0xA8, 0xB4),
            text_muted: Rgb::new(0x6E, 0x6E, 0x7C),

            accent,
            accent_hover: accent.lighten(0.15),
            accent_contrast: accent.readable_foreground(),

            border: Rgb::new(0x26, 0x26, 0x30),
            border_strong: Rgb::new(0x3A, 0x3A, 0x48),

            success: Rgb::new(0x4A, 0xD6, 0x8E),
            warning: Rgb::new(0xF5, 0xB3, 0x4A),
            error: Rgb::new(0xF2, 0x66, 0x6B),
        }
    }

    /// A light shell, warm rather than clinical white.
    pub fn light(accent: Rgb) -> Self {
        let bg_base = Rgb::new(0xFA, 0xFA, 0xFC);
        let accent = accent.ensure_contrast(bg_base, MIN_ACCENT_CONTRAST);

        Self {
            dark_mode: false,
            bg_base,
            bg_surface: Rgb::new(0xFF, 0xFF, 0xFF),
            bg_elevated: Rgb::new(0xFF, 0xFF, 0xFF),
            bg_hover: Rgb::new(0xF0, 0xF0, 0xF4),
            bg_active: Rgb::new(0xE4, 0xE4, 0xEA),

            text_primary: Rgb::new(0x14, 0x14, 0x1A),
            text_secondary: Rgb::new(0x55, 0x55, 0x63),
            text_muted: Rgb::new(0x8A, 0x8A, 0x99),

            accent,
            accent_hover: accent.darken(0.12),
            accent_contrast: accent.readable_foreground(),

            border: Rgb::new(0xE2, 0xE2, 0xE8),
            border_strong: Rgb::new(0xC8, 0xC8, 0xD2),

            success: Rgb::new(0x1E, 0x9E, 0x62),
            warning: Rgb::new(0xB8, 0x7A, 0x14),
            error: Rgb::new(0xCF, 0x36, 0x3C),
        }
    }

    /// Build the palette described by the user's appearance settings.
    ///
    /// `art_accent` is the colour sampled from the current album art. It is
    /// only consulted in [`ThemeMode::Adaptive`]; the other modes stay stable
    /// so the UI does not shift under the user as tracks change.
    pub fn from_appearance(appearance: &Appearance, art_accent: Option<Rgb>) -> Self {
        let configured = Rgb::parse_hex_or(&appearance.accent, Rgb::new(0x7C, 0x5C, 0xFF));

        match appearance.theme {
            ThemeMode::Dark => Self::dark(configured),
            ThemeMode::Light => Self::light(configured),
            ThemeMode::Adaptive => Self::dark(art_accent.unwrap_or(configured)),
        }
    }

    /// Blend two palettes. Drives the crossfade when the adaptive accent moves.
    pub fn lerp(self, other: Self, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        let mix = |a: Rgb, b: Rgb| a.mix(b, t);

        Self {
            // Discrete fields snap at the midpoint rather than interpolating.
            dark_mode: if t < 0.5 {
                self.dark_mode
            } else {
                other.dark_mode
            },
            bg_base: mix(self.bg_base, other.bg_base),
            bg_surface: mix(self.bg_surface, other.bg_surface),
            bg_elevated: mix(self.bg_elevated, other.bg_elevated),
            bg_hover: mix(self.bg_hover, other.bg_hover),
            bg_active: mix(self.bg_active, other.bg_active),
            text_primary: mix(self.text_primary, other.text_primary),
            text_secondary: mix(self.text_secondary, other.text_secondary),
            text_muted: mix(self.text_muted, other.text_muted),
            accent: mix(self.accent, other.accent),
            accent_hover: mix(self.accent_hover, other.accent_hover),
            accent_contrast: mix(self.accent_contrast, other.accent_contrast),
            border: mix(self.border, other.border),
            border_strong: mix(self.border_strong, other.border_strong),
            success: mix(self.success, other.success),
            warning: mix(self.warning, other.warning),
            error: mix(self.error, other.error),
        }
    }

    /// Guarantee body text clears WCAG AA against the surface it sits on.
    ///
    /// Only the adaptive path can produce a failing combination, but running it
    /// everywhere means a hand-edited config cannot make the UI unreadable.
    pub fn enforce_contrast(mut self) -> Self {
        self.text_primary = self
            .text_primary
            .ensure_contrast(self.bg_base, MIN_TEXT_CONTRAST);
        self.text_secondary = self
            .text_secondary
            .ensure_contrast(self.bg_base, MIN_TEXT_CONTRAST);
        self.accent_contrast = self.accent.readable_foreground();
        self
    }
}

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

/// The spacing and shape scale. Derived from density so the whole UI tightens
/// or relaxes together instead of each view inventing its own numbers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    /// Base spacing unit; every gap is a multiple of this.
    pub unit: f32,
    pub radius_small: u8,
    pub radius_medium: u8,
    pub radius_large: u8,
    /// Height of one track row in a list.
    pub row_height: f32,
    pub nav_width: f32,
    pub nav_collapsed_width: f32,
    pub player_bar_height: f32,
    pub queue_width: f32,
    /// Cover art size in list rows.
    pub thumb_size: f32,
}

impl Metrics {
    pub fn for_density(density: Density) -> Self {
        match density {
            Density::Comfortable => Self {
                unit: 8.0,
                radius_small: 6,
                radius_medium: 10,
                radius_large: 16,
                row_height: 52.0,
                nav_width: 232.0,
                nav_collapsed_width: 64.0,
                player_bar_height: 92.0,
                queue_width: 320.0,
                thumb_size: 40.0,
            },
            Density::Compact => Self {
                unit: 6.0,
                radius_small: 4,
                radius_medium: 8,
                radius_large: 12,
                row_height: 38.0,
                nav_width: 200.0,
                nav_collapsed_width: 56.0,
                player_bar_height: 76.0,
                queue_width: 280.0,
                thumb_size: 28.0,
            },
        }
    }

    /// `n` spacing units, for padding and gaps.
    pub fn space(&self, n: f32) -> f32 {
        self.unit * n
    }
}

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

/// How opaque a card is once the content area has a backdrop.
///
/// High enough that body text keeps its contrast, low enough that the wash
/// behind is visibly continuous rather than interrupted by every panel.
const CARD_ALPHA_OVER_BACKDROP: f32 = 0.86;

// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    pub palette: Palette,
    pub metrics: Metrics,

    /// How opaque a card sitting inside the content area is.
    ///
    /// Cards are solid over a plain background — there is nothing behind them
    /// to reveal — and slightly translucent once the content area has a
    /// backdrop, so the wash reads as one continuous field with panels
    /// floating on it. Fully opaque cards over a backdrop looked like bands of
    /// colour showing through the gaps, which is not a design, it is a leak.
    card_alpha: f32,
}

impl Theme {
    pub fn new(appearance: &Appearance, art_accent: Option<Rgb>) -> Self {
        Self {
            palette: Palette::from_appearance(appearance, art_accent).enforce_contrast(),
            metrics: Metrics::for_density(appearance.density),
            card_alpha: if appearance.content_background == SurfaceStyle::Solid {
                1.0
            } else {
                CARD_ALPHA_OVER_BACKDROP
            },
        }
    }

    /// The fill for a card inside the content area.
    ///
    /// Use this rather than `palette.bg_surface` for anything drawn *within*
    /// the content panel, so it picks up the backdrop automatically.
    pub fn card_fill(&self) -> Color32 {
        col_alpha(self.palette.bg_surface, self.card_alpha)
    }

    /// Whether cards are currently letting a backdrop through.
    pub fn cards_are_translucent(&self) -> bool {
        self.card_alpha < 1.0
    }

    /// Push this theme onto an egui context.
    ///
    /// egui keeps a separate style per light/dark theme and picks between them
    /// from the system preference. Our palette has already decided which mode
    /// it is, so both slots get the same style and `set_theme` pins the
    /// choice. Otherwise a user whose OS is in light mode would see egui swap
    /// our dark palette out from under them.
    pub fn apply(&self, ctx: &egui::Context) {
        let style = std::sync::Arc::new(self.style());

        ctx.set_style_of(egui::Theme::Dark, style.clone());
        ctx.set_style_of(egui::Theme::Light, style);
        ctx.set_theme(if self.palette.dark_mode {
            egui::ThemePreference::Dark
        } else {
            egui::ThemePreference::Light
        });
    }

    /// Build the full `egui::Style` for this theme.
    pub fn style(&self) -> egui::Style {
        let m = &self.metrics;

        let mut style = egui::Style {
            visuals: self.visuals(),
            ..Default::default()
        };

        style.spacing.item_spacing = Vec2::new(m.space(1.0), m.space(0.75));
        style.spacing.button_padding = Vec2::new(m.space(1.5), m.space(0.75));
        style.spacing.window_margin = Margin::same(m.space(1.5) as i8);
        style.spacing.menu_margin = Margin::same(m.space(0.75) as i8);
        style.spacing.indent = m.space(2.5);
        style.spacing.interact_size = Vec2::new(0.0, m.space(4.0));
        style.spacing.slider_width = m.space(20.0);
        style.spacing.combo_width = m.space(16.0);
        style.spacing.icon_width = m.space(2.0);
        style.spacing.icon_spacing = m.space(0.75);
        style.spacing.scroll.bar_width = m.space(1.0);
        style.spacing.scroll.floating = true;

        // A pointing-hand cursor on interactive things reads as "modern app"
        // rather than "native toolkit", which is the target here.
        style.visuals.interact_cursor = Some(egui::CursorIcon::PointingHand);

        style.text_styles = self.text_styles();
        style.animation_time = 0.12;

        style
    }

    fn visuals(&self) -> egui::Visuals {
        let p = &self.palette;
        let m = &self.metrics;

        let mut visuals = if p.dark_mode {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        };

        visuals.dark_mode = p.dark_mode;
        visuals.panel_fill = col(p.bg_base);
        visuals.window_fill = col(p.bg_elevated);
        visuals.extreme_bg_color = col(p.bg_base);
        visuals.faint_bg_color = col(p.bg_surface);
        visuals.code_bg_color = col(p.bg_elevated);

        visuals.override_text_color = Some(col(p.text_primary));
        visuals.weak_text_color = Some(col(p.text_muted));
        visuals.warn_fg_color = col(p.warning);
        visuals.error_fg_color = col(p.error);
        visuals.hyperlink_color = col(p.accent);

        visuals.window_corner_radius = CornerRadius::same(m.radius_large);
        visuals.menu_corner_radius = CornerRadius::same(m.radius_medium);
        visuals.window_stroke = Stroke::new(1.0, col(p.border));

        // Soft, low-opacity shadows. Heavy drop shadows are the other classic
        // dated-UI tell.
        visuals.window_shadow = Shadow {
            offset: [0, 8],
            blur: 32,
            spread: 0,
            color: shadow_color(p.dark_mode),
        };
        visuals.popup_shadow = Shadow {
            offset: [0, 4],
            blur: 20,
            spread: 0,
            color: shadow_color(p.dark_mode),
        };

        visuals.selection.bg_fill = col(p.accent).linear_multiply(0.35);
        visuals.selection.stroke = Stroke::new(1.0, col(p.accent));

        visuals.widgets.noninteractive = widget(
            p.bg_surface,
            p.bg_surface,
            p.border,
            p.text_secondary,
            m.radius_medium,
            0.0,
        );
        visuals.widgets.inactive = widget(
            p.bg_elevated,
            p.bg_surface,
            p.border,
            p.text_primary,
            m.radius_medium,
            0.0,
        );
        visuals.widgets.hovered = widget(
            p.bg_hover,
            p.bg_hover,
            p.border_strong,
            p.text_primary,
            m.radius_medium,
            1.0,
        );
        visuals.widgets.active = widget(
            p.bg_active,
            p.bg_active,
            p.accent,
            p.text_primary,
            m.radius_medium,
            0.0,
        );
        visuals.widgets.open = widget(
            p.bg_elevated,
            p.bg_elevated,
            p.border_strong,
            p.text_primary,
            m.radius_medium,
            0.0,
        );

        visuals.button_frame = true;
        visuals.collapsing_header_frame = false;
        visuals.indent_has_left_vline = false;
        visuals.striped = false;
        visuals.slider_trailing_fill = true;
        visuals.handle_shape = egui::style::HandleShape::Circle;
        visuals.image_loading_spinners = false;

        visuals
    }

    fn text_styles(&self) -> std::collections::BTreeMap<TextStyle, egui::FontId> {
        use egui::FontFamily;

        let strong = FontFamily::Name(STRONG_FAMILY.into());
        let body = FontFamily::Proportional;

        [
            (TextStyle::Small, egui::FontId::new(11.0, body.clone())),
            (TextStyle::Body, egui::FontId::new(14.0, body.clone())),
            (TextStyle::Button, egui::FontId::new(14.0, strong.clone())),
            (TextStyle::Heading, egui::FontId::new(26.0, strong.clone())),
            (
                TextStyle::Monospace,
                egui::FontId::new(13.0, FontFamily::Monospace),
            ),
            // Named styles used by the shell.
            (
                TextStyle::Name("title".into()),
                egui::FontId::new(19.0, strong.clone()),
            ),
            (
                TextStyle::Name("subtitle".into()),
                egui::FontId::new(13.0, body.clone()),
            ),
            (
                TextStyle::Name("nav".into()),
                egui::FontId::new(14.0, strong),
            ),
            (
                TextStyle::Name("caption".into()),
                egui::FontId::new(12.0, body),
            ),
        ]
        .into_iter()
        .collect()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert a core colour into an egui one at the crate boundary.
pub fn col(rgb: Rgb) -> Color32 {
    Color32::from_rgb(rgb.r, rgb.g, rgb.b)
}

/// The same, with alpha applied.
pub fn col_alpha(rgb: Rgb, alpha: f32) -> Color32 {
    let a = (alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
    Color32::from_rgba_unmultiplied(rgb.r, rgb.g, rgb.b, a)
}

fn shadow_color(dark_mode: bool) -> Color32 {
    if dark_mode {
        Color32::from_black_alpha(120)
    } else {
        Color32::from_black_alpha(28)
    }
}

fn widget(
    bg_fill: Rgb,
    weak_bg_fill: Rgb,
    stroke: Rgb,
    text: Rgb,
    radius: u8,
    expansion: f32,
) -> egui::style::WidgetVisuals {
    egui::style::WidgetVisuals {
        bg_fill: col(bg_fill),
        weak_bg_fill: col(weak_bg_fill),
        bg_stroke: Stroke::new(1.0, col(stroke)),
        fg_stroke: Stroke::new(1.0, col(text)),
        corner_radius: CornerRadius::same(radius),
        expansion,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Over a plain background there is nothing behind a card, so making it
    /// translucent would only wash out its own contrast.
    #[test]
    fn cards_are_solid_until_there_is_a_backdrop() {
        let mut appearance = Appearance {
            content_background: SurfaceStyle::Solid,
            ..Appearance::default()
        };

        let plain = Theme::new(&appearance, None);
        assert!(!plain.cards_are_translucent());
        assert_eq!(plain.card_fill().a(), 255);

        for style in [SurfaceStyle::AlbumArt, SurfaceStyle::Visualizer] {
            appearance.content_background = style;
            let themed = Theme::new(&appearance, None);

            assert!(themed.cards_are_translucent(), "{style:?}");
            assert!(themed.card_fill().a() < 255, "{style:?}");
            // Still opaque enough to read against.
            assert!(themed.card_fill().a() > 200, "{style:?} is too see-through");
        }
    }

    /// The player bar has its own backdrop, so it is the surface rather than a
    /// card on one — its setting must not change the cards inside the content.
    #[test]
    fn the_player_bar_setting_does_not_affect_cards() {
        let appearance = Appearance {
            content_background: SurfaceStyle::Solid,
            player_background: SurfaceStyle::AlbumArt,
            ..Appearance::default()
        };

        assert!(!Theme::new(&appearance, None).cards_are_translucent());
    }

    fn appearance(theme: ThemeMode, accent: &str) -> Appearance {
        Appearance {
            theme,
            accent: accent.to_owned(),
            ..Appearance::default()
        }
    }

    #[test]
    fn dark_and_light_palettes_have_the_right_polarity() {
        let dark = Palette::dark(Rgb::new(0x7C, 0x5C, 0xFF));
        let light = Palette::light(Rgb::new(0x7C, 0x5C, 0xFF));

        assert!(dark.dark_mode);
        assert!(!light.dark_mode);
        assert!(dark.bg_base.luminance() < light.bg_base.luminance());
        assert!(dark.text_primary.luminance() > dark.bg_base.luminance());
        assert!(light.text_primary.luminance() < light.bg_base.luminance());
    }

    #[test]
    fn backgrounds_are_layered_in_order() {
        // Depth without borders only works if each layer is distinguishable.
        let p = Palette::dark(Rgb::new(0x7C, 0x5C, 0xFF));
        assert!(p.bg_base.luminance() < p.bg_surface.luminance());
        assert!(p.bg_surface.luminance() < p.bg_elevated.luminance());
        assert!(p.bg_elevated.luminance() < p.bg_hover.luminance());
        assert!(p.bg_hover.luminance() < p.bg_active.luminance());
    }

    #[test]
    fn body_text_meets_wcag_aa_in_both_modes() {
        for palette in [
            Palette::dark(Rgb::new(0x7C, 0x5C, 0xFF)).enforce_contrast(),
            Palette::light(Rgb::new(0x7C, 0x5C, 0xFF)).enforce_contrast(),
        ] {
            let ratio = palette.text_primary.contrast_ratio(palette.bg_base);
            assert!(
                ratio >= MIN_TEXT_CONTRAST,
                "primary text ratio {ratio} too low for {:?} mode",
                palette.dark_mode
            );
        }
    }

    #[test]
    fn a_dark_album_accent_is_lifted_until_it_is_visible() {
        // The adaptive path can be handed anything, including near-black.
        let theme = Theme::new(
            &appearance(ThemeMode::Adaptive, "#7C5CFF"),
            Some(Rgb::new(0x0A, 0x08, 0x10)),
        );
        let ratio = theme.palette.accent.contrast_ratio(theme.palette.bg_base);
        assert!(ratio >= MIN_ACCENT_CONTRAST, "accent ratio {ratio} too low");
    }

    #[test]
    fn accent_contrast_is_readable_on_the_accent_itself() {
        for accent in ["#FFFFFF", "#000000", "#7C5CFF", "#F5B34A"] {
            let theme = Theme::new(&appearance(ThemeMode::Dark, accent), None);
            let ratio = theme
                .palette
                .accent_contrast
                .contrast_ratio(theme.palette.accent);
            assert!(ratio >= MIN_TEXT_CONTRAST, "{accent} gives ratio {ratio}");
        }
    }

    #[test]
    fn a_malformed_accent_falls_back_instead_of_panicking() {
        let theme = Theme::new(&appearance(ThemeMode::Dark, "not-a-colour"), None);
        assert_eq!(theme.palette.accent, Rgb::new(0x7C, 0x5C, 0xFF));
    }

    #[test]
    fn non_adaptive_modes_ignore_the_album_accent() {
        let art = Some(Rgb::new(0xFF, 0x00, 0x00));
        let dark = Theme::new(&appearance(ThemeMode::Dark, "#7C5CFF"), art);
        assert_eq!(dark.palette.accent, Rgb::new(0x7C, 0x5C, 0xFF));

        let adaptive = Theme::new(&appearance(ThemeMode::Adaptive, "#7C5CFF"), art);
        assert_ne!(adaptive.palette.accent, dark.palette.accent);
    }

    #[test]
    fn palette_lerp_hits_both_endpoints() {
        let a = Palette::dark(Rgb::new(0x7C, 0x5C, 0xFF));
        let b = Palette::dark(Rgb::new(0xFF, 0x00, 0x00));

        assert_eq!(a.lerp(b, 0.0).accent, a.accent);
        assert_eq!(a.lerp(b, 1.0).accent, b.accent);
        // Clamped, not extrapolated.
        assert_eq!(a.lerp(b, -1.0).accent, a.accent);
        assert_eq!(a.lerp(b, 2.0).accent, b.accent);
    }

    #[test]
    fn compact_density_is_tighter_across_the_board() {
        let comfortable = Metrics::for_density(Density::Comfortable);
        let compact = Metrics::for_density(Density::Compact);

        assert!(compact.unit < comfortable.unit);
        assert!(compact.row_height < comfortable.row_height);
        assert!(compact.nav_width < comfortable.nav_width);
        assert!(compact.player_bar_height < comfortable.player_bar_height);
        assert!(compact.thumb_size < comfortable.thumb_size);
    }

    #[test]
    fn spacing_scale_is_proportional() {
        let m = Metrics::for_density(Density::Comfortable);
        assert_eq!(m.space(1.0), m.unit);
        assert_eq!(m.space(2.0), m.unit * 2.0);
        assert_eq!(m.space(0.5), m.unit * 0.5);
    }

    #[test]
    fn style_registers_every_named_text_style() {
        let theme = Theme::new(&Appearance::default(), None);
        let style = theme.style();

        for name in ["title", "subtitle", "nav", "caption"] {
            assert!(
                style
                    .text_styles
                    .contains_key(&TextStyle::Name(name.into())),
                "missing named text style {name}"
            );
        }
        assert!(style.text_styles.contains_key(&TextStyle::Body));
        assert!(style.text_styles.contains_key(&TextStyle::Heading));
    }

    #[test]
    fn style_uses_palette_colours_not_egui_defaults() {
        let theme = Theme::new(&Appearance::default(), None);
        let style = theme.style();

        assert_eq!(style.visuals.panel_fill, col(theme.palette.bg_base));
        assert_eq!(
            style.visuals.override_text_color,
            Some(col(theme.palette.text_primary))
        );
        assert_eq!(style.visuals.hyperlink_color, col(theme.palette.accent));
    }

    #[test]
    fn colour_conversion_preserves_channels() {
        let rgb = Rgb::new(0x7C, 0x5C, 0xFF);
        assert_eq!(col(rgb), Color32::from_rgb(0x7C, 0x5C, 0xFF));
        assert_eq!(col_alpha(rgb, 1.0).a(), 255);
        assert_eq!(col_alpha(rgb, 0.0).a(), 0);
        // Out-of-range alpha is clamped rather than wrapping.
        assert_eq!(col_alpha(rgb, 5.0).a(), 255);
    }
}
