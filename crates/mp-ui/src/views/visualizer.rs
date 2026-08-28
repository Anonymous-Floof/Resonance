//! The visualizer view: one large panel plus the controls worth reaching for.
//!
//! The settings panel owns the full set. What lives here is the handful you
//! change *while looking at it* — which visualiser, what colour, how sensitive
//! — because walking to Settings and back to judge a sensitivity slider makes
//! the slider useless.

use egui::{RichText, TextStyle, Ui, Vec2};
use mp_core::config::{Visualizer as VizSettings, VisualizerKind, VizColorMode};

use crate::theme::{Theme, col, col_alpha};
use crate::visualizer::{self, Visualizers};
use crate::widgets;

/// How much of the view height the panel takes, before the controls.
const CONTROLS_HEIGHT_UNITS: f32 = 26.0;

#[derive(Debug, Default)]
pub struct Outcome {
    pub changed: bool,
}

pub fn show(
    ui: &mut Ui,
    theme: &Theme,
    visualizers: &mut Visualizers,
    config: &mut VizSettings,
    playing: bool,
    dt: f32,
) -> Outcome {
    let mut outcome = Outcome::default();
    let m = theme.metrics;

    header(ui, theme, config, &mut outcome);
    ui.add_space(m.space(1.5));

    // The panel takes whatever the controls do not need, so the visualiser is
    // as large as the window allows rather than a fixed strip with dead space
    // beneath it.
    let reserved = m.space(CONTROLS_HEIGHT_UNITS);
    let height = (ui.available_height() - reserved).max(m.space(12.0));

    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), height),
        egui::Sense::hover(),
    );

    ui.painter().rect_filled(
        rect,
        egui::CornerRadius::same(m.radius_large),
        theme.card_fill(),
    );

    let inner = rect.shrink(m.space(1.5));

    if config.kind == VisualizerKind::None {
        centred_note(ui, theme, rect, "Visualizer is off");
    } else {
        // Clipped so a renderer that overshoots its rectangle cannot paint
        // over the rest of the view.
        let painter = ui.painter().with_clip_rect(inner);
        visualizers.draw(&painter, inner, theme, config, dt);

        if !visualizers.is_connected() {
            centred_note(ui, theme, rect, "No audio device");
        } else if !playing {
            centred_note(ui, theme, rect, "Play something");
        }
    }

    ui.add_space(m.space(1.5));

    style_row(ui, theme, config, &mut outcome);

    ui.add_space(m.space(1.25));
    widgets::separator(ui, theme);
    ui.add_space(m.space(1.25));

    tuning_row(ui, theme, config, &mut outcome);

    outcome
}

fn header(ui: &mut Ui, theme: &Theme, config: &mut VizSettings, outcome: &mut Outcome) {
    let m = theme.metrics;
    let p = theme.palette;

    ui.horizontal(|ui| {
        ui.label(
            RichText::new("Visualizer")
                .text_style(TextStyle::Heading)
                .color(col(p.text_primary)),
        );

        ui.add_space(m.space(1.0));

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .checkbox(&mut config.show_peak_caps, "Peak caps")
                .on_hover_text("Markers that hold at each band's recent maximum")
                .changed()
            {
                outcome.changed = true;
            }
        });
    });
}

/// Which visualiser, and what colour it takes.
fn style_row(ui: &mut Ui, theme: &Theme, config: &mut VizSettings, outcome: &mut Outcome) {
    let m = theme.metrics;
    let p = theme.palette;

    ui.horizontal_wrapped(|ui| {
        for kind in visualizer::ALL_KINDS {
            // Anything without a renderer is left out rather than offered and
            // quietly drawn as something else, which would be the interface
            // lying about what it is showing.
            if !visualizer::is_available(kind) {
                continue;
            }

            let selected = config.kind == kind;
            let response = ui
                .selectable_label(
                    selected,
                    RichText::new(visualizer::kind_label(kind)).color(if selected {
                        col(p.accent)
                    } else {
                        col(p.text_secondary)
                    }),
                )
                .on_hover_text(visualizer::kind_description(kind));

            if response.clicked() && !selected {
                config.kind = kind;
                outcome.changed = true;
            }
        }
    });

    ui.add_space(m.space(1.0));

    ui.horizontal(|ui| {
        ui.label(
            RichText::new("Colour")
                .text_style(TextStyle::Name("caption".into()))
                .color(col(p.text_muted)),
        );
        ui.add_space(m.space(0.5));

        for (mode, label) in [
            (VizColorMode::Accent, "Accent"),
            (VizColorMode::Spectrum, "Spectrum"),
            (VizColorMode::AlbumArt, "Album art"),
            (VizColorMode::Custom, "Custom"),
        ] {
            let selected = config.color_mode == mode;
            if ui
                .selectable_label(
                    selected,
                    RichText::new(label).color(if selected {
                        col(p.accent)
                    } else {
                        col(p.text_secondary)
                    }),
                )
                .clicked()
                && !selected
            {
                config.color_mode = mode;
                outcome.changed = true;
            }
        }

        if config.color_mode == VizColorMode::Custom {
            ui.add_space(m.space(0.75));
            let response = ui.add(
                egui::TextEdit::singleline(&mut config.custom_color)
                    .desired_width(m.space(9.0))
                    .hint_text("#7C5CFF"),
            );
            if response.changed() {
                outcome.changed = true;
            }
        }
    });

    // What the mode does is not obvious from its name — in particular that it
    // is a ramp rather than one colour, and that it has an honest fallback.
    if config.color_mode == VizColorMode::AlbumArt {
        ui.add_space(m.space(0.5));
        ui.label(
            RichText::new(
                "Takes a dark-to-light ramp from the current cover, deepest at the bass end.",
            )
            .text_style(TextStyle::Name("caption".into()))
            .color(col(p.text_muted)),
        );
    }
}

/// Sensitivity, smoothing and resolution.
fn tuning_row(ui: &mut Ui, theme: &Theme, config: &mut VizSettings, outcome: &mut Outcome) {
    let m = theme.metrics;
    let p = theme.palette;

    let mut changed = false;

    ui.columns(3, |columns| {
        changed |= labelled_slider(
            &mut columns[0],
            theme,
            "Sensitivity",
            egui::Slider::new(&mut config.sensitivity, 0.1..=4.0).step_by(0.05),
        );
        changed |= labelled_slider(
            &mut columns[1],
            theme,
            "Smoothing",
            egui::Slider::new(&mut config.smoothing, 0.0..=0.95).step_by(0.01),
        );

        let bars_enabled = uses_bars(config.kind);
        columns[2].add_enabled_ui(bars_enabled, |ui| {
            changed |= labelled_slider(
                ui,
                theme,
                "Bands",
                egui::Slider::new(&mut config.bar_count, 8..=256),
            );
        });

        if !bars_enabled {
            columns[2].label(
                RichText::new("Not used by this visualizer")
                    .text_style(TextStyle::Name("caption".into()))
                    .color(col_alpha(p.text_muted, 0.8)),
            );
        }
    });

    ui.add_space(m.space(0.5));

    if changed {
        outcome.changed = true;
    }
}

fn labelled_slider(ui: &mut Ui, theme: &Theme, label: &str, slider: egui::Slider<'_>) -> bool {
    ui.label(
        RichText::new(label)
            .text_style(TextStyle::Name("caption".into()))
            .color(col(theme.palette.text_muted)),
    );
    ui.add(slider).changed()
}

/// Whether the band count means anything for this visualiser.
fn uses_bars(kind: VisualizerKind) -> bool {
    matches!(
        kind,
        VisualizerKind::SpectrumBars
            | VisualizerKind::RadialSpectrum
            | VisualizerKind::AuroraBloom
            | VisualizerKind::ParticleField
    )
}

fn centred_note(ui: &Ui, theme: &Theme, rect: egui::Rect, text: &str) {
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        text,
        TextStyle::Body.resolve(ui.style()),
        col_alpha(theme.palette.text_muted, 0.75),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The band slider is greyed out for visualisers that ignore it, so it has
    /// to agree with what those visualisers actually read.
    #[test]
    fn the_band_slider_is_offered_exactly_where_it_applies() {
        assert!(uses_bars(VisualizerKind::SpectrumBars));
        assert!(uses_bars(VisualizerKind::RadialSpectrum));

        assert!(!uses_bars(VisualizerKind::Oscilloscope));
        assert!(!uses_bars(VisualizerKind::WaveformRibbon));
        assert!(!uses_bars(VisualizerKind::None));
    }

    /// The picker hides what is not built, so at least one thing must be.
    #[test]
    fn the_picker_offers_something() {
        let offered: Vec<_> = visualizer::ALL_KINDS
            .into_iter()
            .filter(|kind| visualizer::is_available(*kind))
            .collect();

        assert!(
            offered.len() >= 4,
            "only {} visualisers on offer",
            offered.len()
        );
        assert!(offered.contains(&VisualizerKind::None));
        assert!(offered.contains(&VisualizerKind::AuroraBloom));
        assert!(offered.contains(&VisualizerKind::ParticleField));
    }
}
