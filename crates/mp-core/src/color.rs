//! A tiny sRGB colour type.
//!
//! Config stores colours as hex strings and the theme needs to tint, mix and
//! contrast-check them. Blending happens in linear light rather than directly
//! on sRGB bytes, because naive byte averaging produces the muddy midpoints
//! that make gradients and hover states look dirty.
//!
//! This is deliberately dependency-free so `mp-core` stays testable on its own;
//! `mp-ui` converts to `egui::Color32` at the boundary.

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const BLACK: Self = Self::new(0, 0, 0);
    pub const WHITE: Self = Self::new(255, 255, 255);

    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Parse `#RRGGBB`, `#RGB`, or either without the leading `#`.
    ///
    /// Returns `None` rather than panicking: these come from a hand-editable
    /// config file, so a typo must degrade to "use the default", not a crash.
    pub fn parse_hex(text: &str) -> Option<Self> {
        let hex = text.trim().trim_start_matches('#');

        let (r, g, b) = match hex.len() {
            3 => {
                let expand = |c: u8| c * 17; // 0xF -> 0xFF
                (
                    expand(u8::from_str_radix(&hex[0..1], 16).ok()?),
                    expand(u8::from_str_radix(&hex[1..2], 16).ok()?),
                    expand(u8::from_str_radix(&hex[2..3], 16).ok()?),
                )
            }
            6 => (
                u8::from_str_radix(&hex[0..2], 16).ok()?,
                u8::from_str_radix(&hex[2..4], 16).ok()?,
                u8::from_str_radix(&hex[4..6], 16).ok()?,
            ),
            _ => return None,
        };

        Some(Self::new(r, g, b))
    }

    /// Parse, falling back to `fallback` on anything malformed.
    pub fn parse_hex_or(text: &str, fallback: Self) -> Self {
        Self::parse_hex(text).unwrap_or(fallback)
    }

    pub fn to_hex(self) -> String {
        format!("#{:02X}{:02X}{:02X}", self.r, self.g, self.b)
    }

    pub fn to_array(self) -> [u8; 3] {
        [self.r, self.g, self.b]
    }

    /// Linear-light components in 0.0..=1.0.
    pub fn to_linear(self) -> [f32; 3] {
        [
            srgb_to_linear(self.r),
            srgb_to_linear(self.g),
            srgb_to_linear(self.b),
        ]
    }

    pub fn from_linear(linear: [f32; 3]) -> Self {
        Self::new(
            linear_to_srgb(linear[0]),
            linear_to_srgb(linear[1]),
            linear_to_srgb(linear[2]),
        )
    }

    /// Blend towards `other`. `t` of 0.0 is `self`, 1.0 is `other`.
    pub fn mix(self, other: Self, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        let a = self.to_linear();
        let b = other.to_linear();
        Self::from_linear([
            a[0] + (b[0] - a[0]) * t,
            a[1] + (b[1] - a[1]) * t,
            a[2] + (b[2] - a[2]) * t,
        ])
    }

    pub fn lighten(self, amount: f32) -> Self {
        self.mix(Self::WHITE, amount)
    }

    pub fn darken(self, amount: f32) -> Self {
        self.mix(Self::BLACK, amount)
    }

    /// WCAG relative luminance, 0.0 (black) ..= 1.0 (white).
    pub fn luminance(self) -> f32 {
        let [r, g, b] = self.to_linear();
        0.2126 * r + 0.7152 * g + 0.0722 * b
    }

    /// WCAG contrast ratio, 1.0 (identical) ..= 21.0 (black on white).
    pub fn contrast_ratio(self, other: Self) -> f32 {
        let a = self.luminance();
        let b = other.luminance();
        let (hi, lo) = if a > b { (a, b) } else { (b, a) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// Black or white, whichever stays readable on top of `self`.
    ///
    /// Used by the Adaptive theme, where the accent comes from album art and
    /// could be anything from near-black to near-white.
    pub fn readable_foreground(self) -> Self {
        if self.contrast_ratio(Self::BLACK) >= self.contrast_ratio(Self::WHITE) {
            Self::BLACK
        } else {
            Self::WHITE
        }
    }

    /// Nudge a colour until it reads clearly against `background`.
    ///
    /// Album art yields accents that are often too dark or too washed out to
    /// use as-is for text and focus rings. This walks the colour towards
    /// white or black (whichever direction helps) until it clears
    /// `min_ratio`, preserving hue rather than falling back to grey.
    pub fn ensure_contrast(self, background: Self, min_ratio: f32) -> Self {
        if self.contrast_ratio(background) >= min_ratio {
            return self;
        }

        let target = background.readable_foreground();
        let mut best = self;

        // 20 steps is finer than the eye resolves and keeps this cheap enough
        // to run per frame during the adaptive-accent crossfade.
        for step in 1..=20 {
            let candidate = self.mix(target, step as f32 / 20.0);
            best = candidate;
            if candidate.contrast_ratio(background) >= min_ratio {
                break;
            }
        }

        best
    }
}

impl fmt::Display for Rgb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

fn srgb_to_linear(component: u8) -> f32 {
    let c = component as f32 / 255.0;
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(linear: f32) -> u8 {
    let c = linear.clamp(0.0, 1.0);
    let s = if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    };
    (s * 255.0).round().clamp(0.0, 255.0) as u8
}

/// A colour in Oklab: lightness, and two opponent axes.
///
/// sRGB is a terrible space to reason about colour in. "Is this too dark to
/// use as an accent?" and "are these two pixels the same colour?" both have
/// obvious answers in Oklab and misleading ones in RGB, where a saturated blue
/// and a mid grey can share a byte average. Everything that judges or clusters
/// colours — picking an accent out of album art, above all — works here.
///
/// Oklab rather than CIELAB because it is perceptually better behaved through
/// the blues, costs three cube roots, and needs no white-point bookkeeping.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Oklab {
    /// Perceptual lightness. 0.0 is black, roughly 1.0 is white.
    pub l: f32,
    /// Green (negative) to red (positive).
    pub a: f32,
    /// Blue (negative) to yellow (positive).
    pub b: f32,
}

impl Oklab {
    pub const fn new(l: f32, a: f32, b: f32) -> Self {
        Self { l, a, b }
    }

    /// Distance from the neutral axis: how colourful this is, regardless of
    /// how light it is. Greys sit at ~0.0; a vivid red reaches about 0.26.
    pub fn chroma(self) -> f32 {
        self.a.hypot(self.b)
    }

    /// Hue angle in radians. Meaningless for near-greys.
    pub fn hue(self) -> f32 {
        self.b.atan2(self.a)
    }

    /// The same hue and colourfulness at a different lightness.
    ///
    /// Scaling `a` and `b` along with `l` keeps the *relative* colourfulness
    /// steady; without it, lightening a colour in Oklab washes it out.
    pub fn with_lightness(self, l: f32) -> Self {
        let l = l.max(0.0);
        let scale = if self.l > 0.001 { l / self.l } else { 1.0 };
        // Only ever partially applied: a full rescale overshoots the sRGB gamut
        // badly at high lightness and the result clips to a flat primary.
        let scale = scale.clamp(0.35, 2.2).sqrt();
        Self::new(l, self.a * scale, self.b * scale)
    }

    /// The same hue and lightness at a given chroma.
    pub fn with_chroma(self, chroma: f32) -> Self {
        let current = self.chroma();
        if current < 1e-4 {
            // A true grey has no hue to preserve, so there is nothing to
            // saturate towards. Inventing one would be a lie about the source.
            return self;
        }
        let scale = chroma / current;
        Self::new(self.l, self.a * scale, self.b * scale)
    }

    /// Straight-line distance in Oklab, which is close enough to perceptual
    /// difference for clustering.
    pub fn distance(self, other: Self) -> f32 {
        let dl = self.l - other.l;
        let da = self.a - other.a;
        let db = self.b - other.b;
        (dl * dl + da * da + db * db).sqrt()
    }

    pub fn to_rgb(self) -> Rgb {
        let l_ = self.l + 0.396_337_78 * self.a + 0.215_803_76 * self.b;
        let m_ = self.l - 0.105_561_346 * self.a - 0.063_854_17 * self.b;
        let s_ = self.l - 0.089_484_18 * self.a - 1.291_485_5 * self.b;

        let l = l_ * l_ * l_;
        let m = m_ * m_ * m_;
        let s = s_ * s_ * s_;

        Rgb::from_linear([
            4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_94 * s,
            -1.268_438 * l + 2.609_757_4 * m - 0.341_319_38 * s,
            -0.004_196_086_3 * l - 0.703_418_6 * m + 1.707_614_7 * s,
        ])
    }
}

impl Rgb {
    pub fn to_oklab(self) -> Oklab {
        let [r, g, b] = self.to_linear();

        let l = 0.412_221_46 * r + 0.536_332_55 * g + 0.051_445_995 * b;
        let m = 0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b;
        let s = 0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b;

        let l_ = l.cbrt();
        let m_ = m.cbrt();
        let s_ = s.cbrt();

        Oklab::new(
            0.210_454_26 * l_ + 0.793_617_8 * m_ - 0.004_072_047 * s_,
            1.977_998_5 * l_ - 2.428_592_2 * m_ + 0.450_593_7 * s_,
            0.025_904_037 * l_ + 0.782_771_77 * m_ - 0.808_675_77 * s_,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_six_digit_hex_with_and_without_hash() {
        let expected = Rgb::new(0x7C, 0x5C, 0xFF);
        assert_eq!(Rgb::parse_hex("#7C5CFF"), Some(expected));
        assert_eq!(Rgb::parse_hex("7c5cff"), Some(expected));
        assert_eq!(Rgb::parse_hex("  #7C5CFF  "), Some(expected));
    }

    #[test]
    fn parses_three_digit_shorthand() {
        assert_eq!(Rgb::parse_hex("#F0A"), Some(Rgb::new(0xFF, 0x00, 0xAA)));
        assert_eq!(Rgb::parse_hex("fff"), Some(Rgb::WHITE));
    }

    #[test]
    fn rejects_malformed_input_instead_of_panicking() {
        for bad in [
            "",
            "#",
            "#12",
            "#12345",
            "#GGGGGG",
            "not a colour",
            "#1234567",
        ] {
            assert_eq!(Rgb::parse_hex(bad), None, "should reject {bad:?}");
        }
        // The fallback path is what the config actually relies on.
        assert_eq!(Rgb::parse_hex_or("nonsense", Rgb::WHITE), Rgb::WHITE);
    }

    #[test]
    fn hex_round_trips() {
        let c = Rgb::new(0x12, 0xAB, 0x7F);
        assert_eq!(Rgb::parse_hex(&c.to_hex()), Some(c));
    }

    #[test]
    fn mix_endpoints_are_exact() {
        let a = Rgb::new(10, 20, 30);
        let b = Rgb::new(200, 100, 50);
        assert_eq!(a.mix(b, 0.0), a);
        assert_eq!(a.mix(b, 1.0), b);
        // Out-of-range t is clamped rather than extrapolated.
        assert_eq!(a.mix(b, -3.0), a);
        assert_eq!(a.mix(b, 9.0), b);
    }

    #[test]
    fn mixing_black_and_white_is_perceptually_mid_grey() {
        // A linear-light midpoint lands near #BC, not the #80 that naive byte
        // averaging would give. This is the whole reason for the conversion.
        let mid = Rgb::BLACK.mix(Rgb::WHITE, 0.5);
        assert!(
            (183..=195).contains(&mid.r),
            "expected a linear-light midpoint, got {mid}"
        );
        assert_eq!(mid.r, mid.g);
        assert_eq!(mid.g, mid.b);
    }

    #[test]
    fn luminance_ordering_matches_intuition() {
        assert!(Rgb::WHITE.luminance() > Rgb::new(128, 128, 128).luminance());
        assert!(Rgb::new(128, 128, 128).luminance() > Rgb::BLACK.luminance());
        assert!((Rgb::WHITE.luminance() - 1.0).abs() < 1e-4);
        assert!(Rgb::BLACK.luminance().abs() < 1e-6);
    }

    #[test]
    fn contrast_ratio_matches_wcag_extremes() {
        let ratio = Rgb::BLACK.contrast_ratio(Rgb::WHITE);
        assert!((ratio - 21.0).abs() < 0.01, "got {ratio}");
        assert!((Rgb::WHITE.contrast_ratio(Rgb::WHITE) - 1.0).abs() < 1e-6);
        // Symmetric regardless of argument order.
        assert_eq!(
            Rgb::BLACK.contrast_ratio(Rgb::WHITE),
            Rgb::WHITE.contrast_ratio(Rgb::BLACK)
        );
    }

    #[test]
    fn readable_foreground_flips_at_the_right_end() {
        assert_eq!(Rgb::WHITE.readable_foreground(), Rgb::BLACK);
        assert_eq!(Rgb::BLACK.readable_foreground(), Rgb::WHITE);
        assert_eq!(Rgb::new(0x11, 0x11, 0x14).readable_foreground(), Rgb::WHITE);
    }

    #[test]
    fn ensure_contrast_lifts_a_too_dark_accent() {
        let background = Rgb::new(0x0E, 0x0E, 0x12); // near-black shell
        let muddy = Rgb::new(0x20, 0x18, 0x30); // an accent from dark album art

        assert!(muddy.contrast_ratio(background) < 4.5);
        let fixed = muddy.ensure_contrast(background, 4.5);
        assert!(
            fixed.contrast_ratio(background) >= 4.5,
            "{fixed} still fails against {background}"
        );
    }

    #[test]
    fn ensure_contrast_leaves_good_colours_untouched() {
        let background = Rgb::new(0x0E, 0x0E, 0x12);
        let accent = Rgb::new(0x7C, 0x5C, 0xFF);
        assert_eq!(accent.ensure_contrast(background, 3.0), accent);
    }

    #[test]
    fn linear_conversion_round_trips_every_byte() {
        for v in 0u8..=255 {
            let back = linear_to_srgb(srgb_to_linear(v));
            assert_eq!(back, v, "round trip failed for {v}");
        }
    }
}
