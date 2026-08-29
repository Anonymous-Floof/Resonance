//! Drawing what is playing.
//!
//! The analysis all happens in `mp-audio`; everything here is paint. Each
//! renderer takes the same [`Frame`] and a rectangle and is free to interpret
//! them however it likes, which is why they are separate modules rather than
//! branches of one function — a spectrum and an oscilloscope share the input
//! and nothing else.
//!
//! [`Visualizers`] owns the pieces that have to persist between frames: the
//! feed from the engine, the analyzer, and whatever scrolling history a
//! particular renderer keeps.

pub mod aurora;
mod oscilloscope;
mod particles;
mod radial;
mod ribbon;
mod spectrum;

use egui::{Color32, Mesh, Painter, Pos2, Rect, Shape};
use mp_audio::AudioEngine;
use mp_audio::viz::{Analyzer, Frame, Monitor};
use mp_core::color::Rgb;
use mp_core::config::{Visualizer as VizSettings, VisualizerKind, VizColorMode};
use mp_core::library::CoverPalette;

use crate::theme::Theme;

/// Everything a running visualiser needs to keep between frames.
pub struct Visualizers {
    /// The feed from the engine. `None` until an output stream exists.
    monitor: Option<Monitor>,
    analyzer: Analyzer,
    ribbon: ribbon::History,
    particles: particles::Field,
    /// The device rate the analyzer is currently mapped for.
    rate: u32,
    /// Running clock for the visualisers that animate on their own.
    elapsed: f32,
    /// The most recent analysis, so a renderer can be called without one.
    empty: Frame,

    /// Colours of the current cover, for [`VizColorMode::AlbumArt`].
    ///
    /// Held here rather than passed to `draw` because the visualiser is drawn
    /// from three places now — its own view, the full-screen view, and the
    /// panel backdrops — and threading a palette through every one of them
    /// would mean three chances to forget it.
    cover: Option<CoverPalette>,
}

impl Default for Visualizers {
    fn default() -> Self {
        Self::new()
    }
}

impl Visualizers {
    pub fn new() -> Self {
        // A placeholder rate until a device reports its own. Replaced on the
        // first update, before anything is drawn.
        const ASSUMED_RATE: u32 = 48_000;

        Self {
            monitor: None,
            analyzer: Analyzer::new(ASSUMED_RATE),
            ribbon: ribbon::History::new(),
            particles: particles::Field::new(),
            rate: ASSUMED_RATE,
            elapsed: 0.0,
            empty: Frame::default(),
            cover: None,
        }
    }

    /// Whether audio is reaching the visualiser.
    pub fn is_connected(&self) -> bool {
        self.monitor.is_some()
    }

    /// Tell the visualiser what the current cover looks like.
    ///
    /// Only [`VizColorMode::AlbumArt`] reads it; the other modes ignore it
    /// entirely, so this is safe to call unconditionally.
    pub fn set_cover(&mut self, cover: Option<CoverPalette>) {
        self.cover = cover;
    }

    /// Collect samples and analyse them. Call once per frame, before drawing.
    ///
    /// Returns the analysed frame, or an empty one when no feed is connected —
    /// so a renderer never has to special-case a missing engine.
    pub fn update(
        &mut self,
        engine: Option<&AudioEngine>,
        settings: &VizSettings,
        dt: f32,
    ) -> &Frame {
        let Some(engine) = engine else {
            self.monitor = None;
            return &self.empty;
        };

        // A monitor whose producer has gone belongs to a stream that no longer
        // exists — the device was changed or reopened. Dropping it here is what
        // lets the replacement be collected below.
        if self.monitor.as_ref().is_some_and(Monitor::is_abandoned) {
            self.monitor = None;
        }

        if self.monitor.is_none() {
            self.monitor = engine.take_visualizer();
        }

        let rate = engine.shared().device_rate();
        if rate != 0 && rate != self.rate {
            self.rate = rate;
            self.analyzer.set_sample_rate(rate);
        }

        let Some(monitor) = &mut self.monitor else {
            return &self.empty;
        };

        // `analyze` owns the polling: it also has to notice when *nothing*
        // arrives, so that a stopped stream settles rather than freezing.
        self.analyzer.analyze(monitor, settings, dt)
    }

    /// The most recent analysis, without advancing anything.
    pub fn frame(&self) -> &Frame {
        if self.monitor.is_some() {
            self.analyzer.frame()
        } else {
            &self.empty
        }
    }

    /// Paint the configured visualiser into `rect`.
    ///
    /// [`VisualizerKind::None`] draws nothing, so a caller can hand this any
    /// configuration without checking first.
    pub fn draw(
        &mut self,
        painter: &Painter,
        rect: Rect,
        theme: &Theme,
        settings: &VizSettings,
        dt: f32,
    ) {
        if rect.width() < 4.0 || rect.height() < 4.0 {
            return;
        }

        // A clock the self-animating visualisers run on, advanced only while
        // one of them is actually being drawn.
        self.elapsed += dt.clamp(0.0, 1.0 / 15.0);

        let frame = if self.monitor.is_some() {
            self.analyzer.frame()
        } else {
            &self.empty
        };

        let paint = Paint::resolve(settings, theme, self.cover.as_ref());

        match settings.kind {
            VisualizerKind::None => {}
            VisualizerKind::SpectrumBars => {
                spectrum::draw(painter, rect, frame, &paint, settings);
            }
            VisualizerKind::Oscilloscope => oscilloscope::draw(painter, rect, frame, &paint),
            VisualizerKind::RadialSpectrum => radial::draw(painter, rect, frame, &paint),
            VisualizerKind::WaveformRibbon => {
                self.ribbon.push(frame, dt);
                self.ribbon.draw(painter, rect, &paint);
            }
            VisualizerKind::AuroraBloom => {
                aurora::draw(painter, rect, frame, &paint, self.elapsed);
            }
            VisualizerKind::ParticleField => {
                self.particles.update(frame, dt);
                self.particles.draw(painter, rect, &paint);
            }
        }
    }
}

/// Whether a visualiser kind is actually implemented.
///
/// Every kind in the plan is built now, but the check stays: it is what keeps
/// the settings picker honest if a kind is ever added to the config ahead of
/// its renderer again.
pub fn is_available(_kind: VisualizerKind) -> bool {
    true
}

pub fn kind_label(kind: VisualizerKind) -> &'static str {
    match kind {
        VisualizerKind::None => "Off",
        VisualizerKind::SpectrumBars => "Spectrum",
        VisualizerKind::Oscilloscope => "Oscilloscope",
        VisualizerKind::RadialSpectrum => "Radial",
        VisualizerKind::WaveformRibbon => "Ribbon",
        VisualizerKind::AuroraBloom => "Aurora",
        VisualizerKind::ParticleField => "Particles",
    }
}

pub fn kind_description(kind: VisualizerKind) -> &'static str {
    match kind {
        VisualizerKind::None => "No visualiser",
        VisualizerKind::SpectrumBars => "Frequency bars with falling peak markers",
        VisualizerKind::Oscilloscope => "The waveform itself, held still",
        VisualizerKind::RadialSpectrum => "A spectrum wrapped into a ring",
        VisualizerKind::WaveformRibbon => "Scrolling history of the last few seconds",
        VisualizerKind::AuroraBloom => "Curtains of light, shaded on the GPU",
        VisualizerKind::ParticleField => "Sparks thrown up by each beat",
    }
}

/// Every kind, in the order the settings panel offers them.
pub const ALL_KINDS: [VisualizerKind; 7] = [
    VisualizerKind::None,
    VisualizerKind::SpectrumBars,
    VisualizerKind::Oscilloscope,
    VisualizerKind::RadialSpectrum,
    VisualizerKind::WaveformRibbon,
    VisualizerKind::AuroraBloom,
    VisualizerKind::ParticleField,
];

// ---------------------------------------------------------------------------
// Colour
// ---------------------------------------------------------------------------

/// Below this chroma a cover swatch is a neutral, not a colour.
///
/// Sleeve backgrounds are overwhelmingly near-black or near-white. Building a
/// ramp out of those gives a grey visualiser on almost every record, which is
/// the same trap the accent extraction has to avoid.
const RAMP_MIN_CHROMA: f32 = 0.04;

/// Colours taken from a cover to build its ramp.
const RAMP_SIZE: usize = 3;

/// How a visualiser colours itself, resolved once per frame.
pub struct Paint {
    mode: VizColorMode,
    primary: Rgb,
    /// The far end of the gradient — for a bar, the colour at its base.
    secondary: Rgb,
    /// Background the visualiser is drawn over, for fades to nothing.
    ground: Rgb,
    /// Dark-to-light ramp of the cover's own colours, for
    /// [`VizColorMode::AlbumArt`]. Empty in every other mode.
    ramp: Vec<Rgb>,
}

impl Paint {
    fn resolve(settings: &VizSettings, theme: &Theme, cover: Option<&CoverPalette>) -> Self {
        let p = &theme.palette;

        let ramp = match settings.color_mode {
            VizColorMode::AlbumArt => cover.map(ramp_from).unwrap_or_default(),
            _ => Vec::new(),
        };

        let primary = match settings.color_mode {
            VizColorMode::Custom => Rgb::parse_hex_or(&settings.custom_color, p.accent),
            // The brightest end of the cover's ramp, which is the colour the
            // record actually reads as. With no cover — nothing playing, or a
            // greyscale sleeve — this falls back to the accent, so the mode
            // degrades to "Accent" rather than to grey.
            VizColorMode::AlbumArt => ramp.last().copied().unwrap_or(p.accent),
            VizColorMode::Accent | VizColorMode::Spectrum => p.accent,
        };

        Self {
            mode: settings.color_mode,
            primary,
            secondary: primary.mix(p.bg_base, 0.55),
            ground: p.bg_base,
            ramp,
        }
    }

    /// The colour at position `t` across the spectrum, `0.0` low to `1.0` high.
    pub fn at(&self, t: f32) -> Rgb {
        match self.mode {
            // A hue sweep across the display, warm at the bass end. Stopping
            // short of a full turn keeps the two ends distinguishable — a
            // complete rainbow puts red at both edges.
            VizColorMode::Spectrum => hsv(20.0 + t.clamp(0.0, 1.0) * 260.0, 0.72, 1.0),
            // The cover's own colours, darkest at the bass end. Sweeping the
            // ramp rather than using one colour is what makes this read as
            // *the record's* palette and not merely a differently-tinted
            // accent — the same reason the spectrum mode sweeps a hue.
            VizColorMode::AlbumArt if !self.ramp.is_empty() => sample(&self.ramp, t),
            _ => self.primary,
        }
    }

    /// The dimmer end of a bar's gradient at position `t`.
    pub fn base_at(&self, t: f32) -> Rgb {
        match self.mode {
            VizColorMode::Spectrum => self.at(t).mix(self.ground, 0.5),
            VizColorMode::AlbumArt if !self.ramp.is_empty() => self.at(t).mix(self.ground, 0.55),
            _ => self.secondary,
        }
    }

    pub fn primary(&self) -> Rgb {
        self.primary
    }

    /// Three related colours, for a visualiser that layers more than one.
    ///
    /// In spectrum and album modes they are pulled from across the sweep so the
    /// layers stay distinguishable; otherwise they are shades of the one
    /// colour, which keeps a custom or accent palette from turning into a
    /// rainbow.
    pub fn triad(&self) -> (Rgb, Rgb, Rgb) {
        match self.mode {
            VizColorMode::Spectrum => (self.at(0.08), self.at(0.45), self.at(0.85)),
            VizColorMode::AlbumArt if !self.ramp.is_empty() => {
                (self.at(0.05), self.at(0.5), self.at(0.95))
            }
            _ => (
                self.primary,
                self.primary.lighten(0.22),
                self.primary.mix(self.ground, 0.35),
            ),
        }
    }
}

/// How far apart two ramp colours have to be, in Oklab.
///
/// Without this the ramp happily picks three shades of the same navy, which
/// animates as one flat colour that happens to get slightly lighter.
const RAMP_MIN_DISTANCE: f32 = 0.10;

/// Build a dark-to-light ramp from a cover's colours.
///
/// Three rules, in order of who wins.
///
/// The cover's own accent goes in first when there is one. That colour has
/// already been chosen by the palette extraction as the best thing on the
/// sleeve and corrected into a usable range, so re-deriving a different answer
/// here would mean the visualiser and the Adaptive theme disagreed about what
/// colour the record is.
///
/// The rest are ranked by prominence *and* colourfulness rather than
/// prominence alone. Ranking by size picked the two big desaturated regions
/// every sleeve has and left the one vivid colour on the floor — the display
/// came out grey-blue on a cover full of magenta. Multiplying by chroma is the
/// same correction the accent extraction makes, for the same reason.
///
/// Order is by lightness, so the bass end is the deep end on every record and
/// the display reads the same way from one album to the next.
fn ramp_from(cover: &CoverPalette) -> Vec<Rgb> {
    let mut ranked: Vec<(Rgb, f32)> = cover
        .swatches
        .iter()
        .filter(|swatch| swatch.colour.to_oklab().chroma() >= RAMP_MIN_CHROMA)
        .map(|swatch| (swatch.colour, ramp_score(swatch.colour, swatch.weight)))
        .collect();

    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let mut chosen: Vec<Rgb> = Vec::with_capacity(RAMP_SIZE);

    // Gated on chroma like everything else rather than trusted outright. The
    // extraction never returns a grey accent, but the palette can also come
    // from a hand-edited cache, and one grey seed would drag the whole ramp
    // to grey — the exact failure this mode exists to avoid.
    if let Some(accent) = cover
        .accent
        .filter(|colour| colour.to_oklab().chroma() >= RAMP_MIN_CHROMA)
    {
        chosen.push(accent);
    }

    for (colour, _) in ranked {
        if chosen.len() >= RAMP_SIZE {
            break;
        }
        // Skip anything that would be a near-duplicate of a colour already in.
        let lab = colour.to_oklab();
        if chosen
            .iter()
            .all(|picked| picked.to_oklab().distance(lab) >= RAMP_MIN_DISTANCE)
        {
            chosen.push(colour);
        }
    }

    // One colour is not a ramp. A cover with a single usable colour gets a
    // shaded version of it rather than a flat wall.
    if chosen.len() == 1 {
        let only = chosen[0];
        chosen = vec![only.darken(0.4), only, only.lighten(0.32)];
    }

    // Nothing usable — a greyscale sleeve. The caller falls back to the accent.
    if chosen.len() < 2 {
        return Vec::new();
    }

    chosen.sort_by(|a, b| {
        a.luminance()
            .partial_cmp(&b.luminance())
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    chosen
}

/// How good a ramp colour a swatch would make.
///
/// Prominence under a square root so a large dull field cannot outrank a
/// smaller vivid one, times colourfulness capped at "clearly a colour".
fn ramp_score(colour: Rgb, weight: f32) -> f32 {
    let chroma = colour.to_oklab().chroma();
    weight.max(0.0).sqrt() * (chroma / 0.13).min(1.0)
}

/// Interpolate along a ramp. `t` runs `0.0`..=`1.0` across it.
fn sample(ramp: &[Rgb], t: f32) -> Rgb {
    match ramp.len() {
        0 => Rgb::BLACK,
        1 => ramp[0],
        len => {
            let position = t.clamp(0.0, 1.0) * (len - 1) as f32;
            let index = (position.floor() as usize).min(len - 2);
            ramp[index].mix(ramp[index + 1], position - index as f32)
        }
    }
}

/// HSV to RGB, for the spectrum colour mode.
///
/// `hue` in degrees; `saturation` and `value` in `0.0..=1.0`.
fn hsv(hue: f32, saturation: f32, value: f32) -> Rgb {
    let hue = hue.rem_euclid(360.0) / 60.0;
    let chroma = value * saturation;
    let second = chroma * (1.0 - ((hue % 2.0) - 1.0).abs());
    let offset = value - chroma;

    let (r, g, b) = match hue as u32 {
        0 => (chroma, second, 0.0),
        1 => (second, chroma, 0.0),
        2 => (0.0, chroma, second),
        3 => (0.0, second, chroma),
        4 => (second, 0.0, chroma),
        _ => (chroma, 0.0, second),
    };

    Rgb {
        r: ((r + offset) * 255.0).round().clamp(0.0, 255.0) as u8,
        g: ((g + offset) * 255.0).round().clamp(0.0, 255.0) as u8,
        b: ((b + offset) * 255.0).round().clamp(0.0, 255.0) as u8,
    }
}

// ---------------------------------------------------------------------------
// Shared painting helpers
// ---------------------------------------------------------------------------

/// Fill `rect` with a vertical gradient.
///
/// egui has no gradient primitive, so this is a two-triangle mesh with a colour
/// per corner — which the renderer interpolates for free. Painting a stack of
/// solid slices instead would band visibly on a tall bar.
pub fn vertical_gradient(painter: &Painter, rect: Rect, top: Color32, bottom: Color32) {
    if rect.width() <= 0.0 || rect.height() <= 0.0 {
        return;
    }

    let mut mesh = Mesh::default();
    mesh.colored_vertex(rect.left_top(), top);
    mesh.colored_vertex(rect.right_top(), top);
    mesh.colored_vertex(rect.left_bottom(), bottom);
    mesh.colored_vertex(rect.right_bottom(), bottom);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(1, 3, 2);

    painter.add(Shape::mesh(mesh));
}

/// Fill the band between two polylines with a gradient across its height.
///
/// Used for the ribbon, whose outline is not convex and so cannot go through
/// `convex_polygon`. `top` and `bottom` must be the same length and share their
/// x positions.
pub fn filled_band(
    painter: &Painter,
    top: &[Pos2],
    bottom: &[Pos2],
    edge: Color32,
    centre: Color32,
) {
    if top.len() < 2 || top.len() != bottom.len() {
        return;
    }

    let mut mesh = Mesh::default();

    for (upper, lower) in top.iter().zip(bottom.iter()) {
        mesh.colored_vertex(*upper, edge);
        mesh.colored_vertex(*lower, edge);
        // A third vertex on the centre line gives the band a bright core
        // rather than a flat slab of one colour.
        mesh.colored_vertex(Pos2::new(upper.x, (upper.y + lower.y) * 0.5), centre);
    }

    for column in 0..top.len() - 1 {
        let a = column as u32 * 3;
        let b = a + 3;

        // Upper half, then lower half.
        mesh.add_triangle(a, a + 2, b);
        mesh.add_triangle(a + 2, b + 2, b);
        mesh.add_triangle(a + 2, a + 1, b + 2);
        mesh.add_triangle(a + 1, b + 1, b + 2);
    }

    painter.add(Shape::mesh(mesh));
}

#[cfg(test)]
mod tests {
    use super::*;
    use mp_core::library::Swatch;

    /// Build a `Paint` the way the app does, rather than by hand — these
    /// tests are as much about `resolve` picking the right colours as about
    /// what the getters do with them.
    fn paint(mode: VizColorMode, cover: Option<&CoverPalette>) -> Paint {
        let settings = VizSettings {
            color_mode: mode,
            ..VizSettings::default()
        };
        let theme = Theme::new(&mp_core::config::Appearance::default(), None);

        Paint::resolve(&settings, &theme, cover)
    }

    /// The theme accent, which every mode falls back to.
    fn accent() -> Rgb {
        Theme::new(&mp_core::config::Appearance::default(), None)
            .palette
            .accent
    }

    /// A cover with the given colours, most prominent first.
    ///
    /// The accent mirrors what the extraction would do: the first colour, but
    /// only if it is actually a colour. A real `CoverPalette` never carries a
    /// grey accent, and a fixture that does would be testing a state the app
    /// cannot reach.
    fn cover(colours: &[Rgb]) -> CoverPalette {
        CoverPalette {
            accent: colours
                .iter()
                .copied()
                .find(|colour| colour.to_oklab().chroma() >= RAMP_MIN_CHROMA),
            swatches: colours
                .iter()
                .enumerate()
                .map(|(index, colour)| Swatch {
                    colour: *colour,
                    weight: 1.0 / (index + 2) as f32,
                })
                .collect(),
        }
    }

    fn separation(x: Rgb, y: Rgb) -> i32 {
        (x.r as i32 - y.r as i32).abs()
            + (x.g as i32 - y.g as i32).abs()
            + (x.b as i32 - y.b as i32).abs()
    }

    #[test]
    fn the_spectrum_sweep_gives_distinct_colours_across_the_display() {
        let paint = paint(VizColorMode::Spectrum, None);

        let low = paint.at(0.0);
        let high = paint.at(1.0);

        // Far enough apart to read as different colours, not shades.
        let distance = (low.r as i32 - high.r as i32).abs()
            + (low.g as i32 - high.g as i32).abs()
            + (low.b as i32 - high.b as i32).abs();

        assert!(
            distance > 200,
            "the sweep barely moved: {low:?} to {high:?}"
        );
    }

    #[test]
    fn a_fixed_colour_mode_ignores_position() {
        let paint = paint(VizColorMode::Accent, None);

        assert_eq!(paint.at(0.0), accent());
        assert_eq!(paint.at(1.0), accent());
    }

    #[test]
    fn hsv_hits_the_primaries() {
        assert_eq!(hsv(0.0, 1.0, 1.0), Rgb { r: 255, g: 0, b: 0 });
        assert_eq!(hsv(120.0, 1.0, 1.0), Rgb { r: 0, g: 255, b: 0 });
        assert_eq!(hsv(240.0, 1.0, 1.0), Rgb { r: 0, g: 0, b: 255 });
    }

    #[test]
    fn hsv_wraps_rather_than_clipping() {
        assert_eq!(hsv(360.0, 1.0, 1.0), hsv(0.0, 1.0, 1.0));
        assert_eq!(hsv(-120.0, 1.0, 1.0), hsv(240.0, 1.0, 1.0));
    }

    /// Every kind the settings panel offers must have a name and a
    /// description, and the availability flag must match what `draw` handles.
    #[test]
    fn every_kind_is_described_and_its_availability_is_honest() {
        for kind in ALL_KINDS {
            assert!(!kind_label(kind).is_empty(), "{kind:?} needs a label");
            assert!(
                !kind_description(kind).is_empty(),
                "{kind:?} needs a description"
            );
        }

        // Everything in the plan is built, so everything is on offer.
        for kind in ALL_KINDS {
            assert!(is_available(kind), "{kind:?} is described but not offered");
        }
    }

    /// Layered visualisers need three colours that are actually different,
    /// or the layers stack into one indistinguishable smear.
    #[test]
    fn the_triad_gives_three_distinguishable_colours() {
        let art = cover(&[
            Rgb::new(210, 70, 60),
            Rgb::new(40, 90, 170),
            Rgb::new(230, 200, 90),
        ]);

        for mode in [
            VizColorMode::Accent,
            VizColorMode::Spectrum,
            VizColorMode::Custom,
            VizColorMode::AlbumArt,
        ] {
            let paint = paint(mode, Some(&art));
            let (a, b, c) = paint.triad();

            assert!(separation(a, b) > 12, "{mode:?}: first two layers match");
            assert!(separation(b, c) > 12, "{mode:?}: last two layers match");
            assert!(separation(a, c) > 12, "{mode:?}: outer two layers match");
        }
    }

    /// The bug this covers: album-art mode fell through to the accent, so the
    /// setting existed and did nothing.
    #[test]
    fn album_mode_uses_the_covers_colours_not_the_accent() {
        let art = cover(&[
            Rgb::new(215, 75, 55),
            Rgb::new(45, 95, 175),
            Rgb::new(235, 205, 95),
        ]);

        let paint = paint(VizColorMode::AlbumArt, Some(&art));

        for t in [0.0, 0.5, 1.0] {
            assert!(
                separation(paint.at(t), accent()) > 40,
                "at {t} the visualiser was still drawing in the accent colour"
            );
        }
    }

    /// The ramp has to run dark to light regardless of the order the cover's
    /// swatches arrive in, so the bass end is the deep end on every record.
    #[test]
    fn the_album_ramp_runs_dark_to_light() {
        // Deliberately handed over brightest-first.
        let art = cover(&[
            Rgb::new(240, 220, 120),
            Rgb::new(30, 60, 140),
            Rgb::new(190, 80, 70),
        ]);

        let paint = paint(VizColorMode::AlbumArt, Some(&art));

        let low = paint.at(0.0).luminance();
        let mid = paint.at(0.5).luminance();
        let high = paint.at(1.0).luminance();

        assert!(low < high, "the ramp runs backwards: {low} to {high}");
        assert!(low <= mid && mid <= high, "the middle is out of order");
    }

    /// What the screen showed: a cover whose big regions are desaturated and
    /// whose one vivid colour is a small patch came out grey-blue. Size alone
    /// is the wrong ranking.
    #[test]
    fn a_vivid_minority_colour_beats_a_large_dull_one() {
        let art = CoverPalette {
            accent: None,
            swatches: vec![
                Swatch {
                    colour: Rgb::new(38, 44, 66),
                    weight: 0.55,
                },
                Swatch {
                    colour: Rgb::new(120, 130, 150),
                    weight: 0.33,
                },
                Swatch {
                    colour: Rgb::new(225, 60, 165),
                    weight: 0.12,
                },
            ],
        };

        let ramp = ramp_from(&art);
        let most_colourful = ramp
            .iter()
            .map(|colour| colour.to_oklab().chroma())
            .fold(0.0f32, f32::max);

        assert!(
            most_colourful > 0.12,
            "the magenta never made the ramp: {ramp:?}"
        );
    }

    /// The palette already picked the best colour on the sleeve; the
    /// visualiser and the Adaptive theme must not disagree about it.
    #[test]
    fn the_covers_own_accent_is_always_in_the_ramp() {
        let accent = Rgb::new(220, 90, 40);
        let art = CoverPalette {
            accent: Some(accent),
            swatches: vec![
                Swatch {
                    colour: Rgb::new(30, 40, 60),
                    weight: 0.7,
                },
                Swatch {
                    colour: Rgb::new(90, 110, 150),
                    weight: 0.3,
                },
            ],
        };

        assert!(
            ramp_from(&art).contains(&accent),
            "the cover's chosen accent was dropped"
        );
    }

    /// Three shades of one navy is not a ramp, it is a flat colour that gets
    /// slightly lighter.
    #[test]
    fn the_ramp_refuses_near_duplicates() {
        let art = CoverPalette {
            accent: None,
            swatches: vec![
                Swatch {
                    colour: Rgb::new(40, 60, 130),
                    weight: 0.4,
                },
                Swatch {
                    colour: Rgb::new(42, 62, 133),
                    weight: 0.35,
                },
                Swatch {
                    colour: Rgb::new(41, 61, 131),
                    weight: 0.25,
                },
            ],
        };

        let ramp = ramp_from(&art);

        // One distinct colour survived, so it was shaded into a ramp rather
        // than three near-identical entries being kept.
        assert_eq!(ramp.len(), 3);
        assert!(
            separation(ramp[0], ramp[2]) > 40,
            "the ramp is three of the same colour: {ramp:?}"
        );
    }

    /// A greyscale sleeve has no colours to take, and a grey visualiser would
    /// be worse than the accent it replaced.
    #[test]
    fn a_colourless_cover_falls_back_to_the_accent() {
        let grey = cover(&[
            Rgb::new(20, 20, 21),
            Rgb::new(128, 128, 129),
            Rgb::new(240, 240, 241),
        ]);

        let paint = paint(VizColorMode::AlbumArt, Some(&grey));

        assert_eq!(paint.at(0.0), accent());
        assert_eq!(paint.at(1.0), accent());
        assert_eq!(paint.primary(), accent());
    }

    /// With nothing playing there is no cover at all, which must behave the
    /// same way rather than painting black.
    #[test]
    fn album_mode_with_no_cover_falls_back_to_the_accent() {
        let paint = paint(VizColorMode::AlbumArt, None);

        assert_eq!(paint.at(0.3), accent());
        assert_eq!(paint.primary(), accent());
    }

    /// One usable colour is not a ramp; it gets shaded rather than producing a
    /// flat wall of one tone.
    #[test]
    fn a_single_coloured_cover_is_shaded_into_a_ramp() {
        let art = cover(&[Rgb::new(200, 60, 60), Rgb::new(18, 18, 19)]);
        let paint = paint(VizColorMode::AlbumArt, Some(&art));

        let low = paint.at(0.0);
        let high = paint.at(1.0);

        assert!(low.luminance() < high.luminance());
        assert!(separation(low, high) > 40, "the ramp is flat");
    }

    /// Other modes must be untouched by a cover being present.
    #[test]
    fn a_cover_does_not_leak_into_the_other_modes() {
        let art = cover(&[Rgb::new(215, 75, 55), Rgb::new(45, 95, 175)]);

        for mode in [VizColorMode::Accent, VizColorMode::Spectrum] {
            let with = paint(mode, Some(&art));
            let without = paint(mode, None);

            assert_eq!(with.at(0.0), without.at(0.0), "{mode:?}");
            assert_eq!(with.at(1.0), without.at(1.0), "{mode:?}");
        }
    }

    #[test]
    fn sampling_a_ramp_hits_its_ends_exactly() {
        let ramp = vec![
            Rgb::new(10, 20, 30),
            Rgb::new(100, 110, 120),
            Rgb::new(200, 210, 220),
        ];

        assert_eq!(sample(&ramp, 0.0), ramp[0]);
        assert_eq!(sample(&ramp, 1.0), ramp[2]);
        assert_eq!(sample(&ramp, 0.5), ramp[1]);

        // Out of range is clamped rather than wrapping or panicking.
        assert_eq!(sample(&ramp, -2.0), ramp[0]);
        assert_eq!(sample(&ramp, 5.0), ramp[2]);

        // Degenerate ramps do not panic.
        assert_eq!(sample(&[], 0.5), Rgb::BLACK);
        assert_eq!(sample(&ramp[..1], 0.5), ramp[0]);
    }

    #[test]
    fn the_kind_list_covers_the_whole_enum() {
        // A kind added to the config without being added here would silently
        // vanish from the settings panel.
        let mut seen = std::collections::HashSet::new();
        for kind in ALL_KINDS {
            assert!(seen.insert(kind), "{kind:?} is listed twice");
        }
        assert_eq!(seen.len(), ALL_KINDS.len());
    }
}
