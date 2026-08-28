//! The equalizer.
//!
//! Ten sliders over a live composite response curve. The curve is the point:
//! ten disconnected handles tell you what you set, but not what it *does* —
//! neighbouring bands overlap, so +6 dB on two adjacent sliders is nearly +9 dB
//! between them. Drawing the sum makes that visible.
//!
//! The curve is sampled from [`Bank::response_db`], the same function the audio
//! thread's coefficients come from, so what is drawn cannot drift from what is
//! heard.

use egui::{Align, Layout, Pos2, Rect, RichText, Sense, Stroke, TextStyle, Ui, Vec2};
use mp_audio::dsp::eq::{BAND_COUNT, BAND_FREQUENCIES, Bank, band_label};
use mp_audio::dsp::presets;
use mp_core::config::Equalizer;

use crate::theme::{Theme, col, col_alpha};
use crate::widgets;

/// Lowest and highest frequency drawn on the curve.
const VIEW_RANGE_HZ: (f32, f32) = (20.0, 20_000.0);

/// Vertical extent of the curve, in decibels either side of flat.
const VIEW_RANGE_DB: f32 = 15.0;

/// Vertical space the sliders, preamp row and presets need below the curve,
/// in spacing units. The curve gets everything else.
const CONTROLS_HEIGHT_UNITS: f32 = 42.0;

/// How many points the curve is sampled at.
///
/// The response is smooth, so this is about looking right rather than about
/// accuracy; 160 points is under a millisecond and indistinguishable from more.
const CURVE_POINTS: usize = 160;

/// What the user did.
#[derive(Debug, Default, Clone)]
pub struct Outcome {
    /// Any setting changed, so the engine and the config both need updating.
    pub changed: bool,
    /// A preset was chosen.
    pub preset: Option<&'static str>,
}

pub fn show(ui: &mut Ui, theme: &Theme, config: &mut Equalizer, limiting: bool) -> Outcome {
    let mut outcome = Outcome::default();
    let m = theme.metrics;
    let p = theme.palette;

    // Built from the live settings each frame. Ten biquads is a few microseconds
    // and it keeps the curve honest: it is derived, never cached.
    let sample_rate = 48_000.0;
    let bank = Bank::new(
        &config.gains_db,
        config.preamp_db,
        sample_rate,
        config.enabled,
    );

    header(ui, theme, config, limiting, &mut outcome);
    ui.add_space(m.space(1.5));

    curve(ui, theme, &bank, config);
    ui.add_space(m.space(1.0));

    if sliders(ui, theme, config) {
        outcome.changed = true;
        // Hand-editing a curve means it is no longer the preset it came from.
        config.preset = presets::matching(&config.gains_db, config.preamp_db)
            .map(|preset| preset.name.to_owned());
    }

    ui.add_space(m.space(1.5));
    widgets::separator(ui, theme);
    ui.add_space(m.space(1.5));

    preamp_row(ui, theme, &bank, config, &mut outcome);

    ui.add_space(m.space(1.5));
    ui.label(
        RichText::new("Presets")
            .text_style(TextStyle::Name("caption".into()))
            .color(col(p.text_muted)),
    );
    ui.add_space(m.space(0.75));
    preset_row(ui, theme, config, &mut outcome);

    outcome
}

/// Title, enable toggle, limiter toggle and the clip indicator.
fn header(
    ui: &mut Ui,
    theme: &Theme,
    config: &mut Equalizer,
    limiting: bool,
    outcome: &mut Outcome,
) {
    let m = theme.metrics;
    let p = theme.palette;

    ui.horizontal(|ui| {
        ui.label(
            RichText::new("Equalizer")
                .text_style(TextStyle::Heading)
                .color(col(p.text_primary)),
        );

        ui.add_space(m.space(1.0));

        // A/B: the only honest way to judge an equalizer is to hear it off.
        if ui.checkbox(&mut config.enabled, "On").changed() {
            outcome.changed = true;
        }

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            // The clip indicator earns its place: without it, a curve that is
            // quietly being limited just sounds slightly wrong.
            if limiting {
                ui.label(
                    RichText::new("● limiting")
                        .text_style(TextStyle::Name("caption".into()))
                        .color(col(p.warning)),
                )
                .on_hover_text(
                    "The limiter is holding the output back. Lower the preamp \
                     to give the curve more room.",
                );
                ui.add_space(m.space(1.0));
            }

            if ui
                .checkbox(&mut config.limiter, "Limiter")
                .on_hover_text("Prevents boosted bands from clipping")
                .changed()
            {
                outcome.changed = true;
            }
        });
    });
}

/// The composite response curve.
fn curve(ui: &mut Ui, theme: &Theme, bank: &Bank, config: &Equalizer) {
    let m = theme.metrics;
    let p = theme.palette;

    // The plot takes whatever the controls below it do not need. A ±15 dB
    // range crammed into a short strip is unreadable exactly when it matters:
    // a preset with a -12 dB preamp puts its whole curve near the floor, and at
    // four pixels per decibel the shape disappears.
    let reserved = m.space(CONTROLS_HEIGHT_UNITS);
    let height = (ui.available_height() - reserved).clamp(m.space(16.0), m.space(40.0));

    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());

    if !ui.is_rect_visible(rect) {
        return;
    }

    let painter = ui.painter();
    painter.rect_filled(
        rect,
        egui::CornerRadius::same(m.radius_medium),
        theme.card_fill(),
    );

    // The decibel scale gets its own gutter on the left and the frequency
    // labels theirs at the bottom, so neither is drawn over the curve.
    let gutter = m.space(3.0);
    let plot = Rect::from_min_max(
        Pos2::new(rect.left() + gutter, rect.top() + m.space(1.0)),
        Pos2::new(rect.right() - m.space(1.0), rect.bottom() - m.space(2.0)),
    );

    // Horizontal grid: 0 dB emphasised, because that is the line the curve is
    // read against.
    for db in [-12.0, -6.0, 0.0, 6.0, 12.0] {
        let y = db_to_y(db, plot);
        let is_zero = db == 0.0;
        painter.line_segment(
            [Pos2::new(plot.left(), y), Pos2::new(plot.right(), y)],
            Stroke::new(
                if is_zero { 1.2 } else { 1.0 },
                col_alpha(p.border, if is_zero { 0.9 } else { 0.35 }),
            ),
        );

        painter.text(
            Pos2::new(plot.left() - 4.0, y),
            egui::Align2::RIGHT_CENTER,
            if is_zero {
                "0".to_owned()
            } else {
                format!("{db:+.0}")
            },
            TextStyle::Name("caption".into()).resolve(ui.style()),
            col_alpha(p.text_muted, if is_zero { 0.9 } else { 0.7 }),
        );
    }

    // Vertical grid at the band centres, so a slider lines up with its effect.
    for freq in BAND_FREQUENCIES {
        let x = freq_to_x(freq, plot);
        painter.line_segment(
            [Pos2::new(x, plot.top()), Pos2::new(x, plot.bottom())],
            Stroke::new(1.0, col_alpha(p.border, 0.25)),
        );
    }

    // The curve itself.
    let points: Vec<Pos2> = (0..CURVE_POINTS)
        .map(|index| {
            let t = index as f32 / (CURVE_POINTS - 1) as f32;
            let freq = VIEW_RANGE_HZ.0 * (VIEW_RANGE_HZ.1 / VIEW_RANGE_HZ.0).powf(t);
            Pos2::new(freq_to_x(freq, plot), db_to_y(bank.response_db(freq), plot))
        })
        .collect();

    // A filled band between the curve and 0 dB reads as "how much" at a glance,
    // which a bare line does not.
    let zero_y = db_to_y(0.0, plot);
    let accent = if config.enabled {
        p.accent
    } else {
        p.text_muted
    };

    for pair in points.windows(2) {
        let quad = vec![
            pair[0],
            pair[1],
            Pos2::new(pair[1].x, zero_y),
            Pos2::new(pair[0].x, zero_y),
        ];
        painter.add(egui::Shape::convex_polygon(
            quad,
            col_alpha(accent, 0.14),
            Stroke::NONE,
        ));
    }

    painter.add(egui::Shape::line(
        points,
        Stroke::new(
            2.0,
            col_alpha(accent, if config.enabled { 1.0 } else { 0.4 }),
        ),
    ));

    // Frequency labels below the plot, in their own strip.
    for (index, freq) in BAND_FREQUENCIES.iter().enumerate() {
        painter.text(
            Pos2::new(freq_to_x(*freq, plot), plot.bottom() + 3.0),
            egui::Align2::CENTER_TOP,
            band_label(index),
            TextStyle::Name("caption".into()).resolve(ui.style()),
            col_alpha(p.text_muted, 0.8),
        );
    }

    // A disabled equalizer draws a flat line along zero, which is honest but
    // reads as a missing feature. Say so instead.
    if !config.enabled {
        painter.text(
            plot.center(),
            egui::Align2::CENTER_CENTER,
            "Equalizer is off",
            TextStyle::Body.resolve(ui.style()),
            col_alpha(p.text_muted, 0.75),
        );
    }
}

/// The ten band sliders. Returns whether any moved.
///
/// Laid out with `columns` rather than hand-allocated cells inside a
/// `horizontal`: a horizontal layout advances its cursor by each item's own
/// height, so ten sliders of slightly different heights come out visibly
/// staggered down the screen, and the last one runs off the right edge.
/// `columns` gives ten equal, independently top-aligned strips instead.
fn sliders(ui: &mut Ui, theme: &Theme, config: &mut Equalizer) -> bool {
    let m = theme.metrics;
    let p = theme.palette;
    let mut changed = false;

    // Settings written by an older build can be short; pad rather than panic.
    if config.gains_db.len() < BAND_COUNT {
        config.gains_db.resize(BAND_COUNT, 0.0);
    }

    let track_height = m.space(14.0);

    ui.columns(BAND_COUNT, |columns| {
        for (index, column) in columns.iter_mut().enumerate() {
            column.vertical_centered(|ui| {
                let gain = &mut config.gains_db[index];
                let before = *gain;

                // egui sizes a vertical slider from `slider_width`, so it is set
                // here rather than left to whatever the surrounding style says.
                ui.spacing_mut().slider_width = track_height;

                let response = ui.add(
                    egui::Slider::new(gain, -Equalizer::MAX_GAIN_DB..=Equalizer::MAX_GAIN_DB)
                        .vertical()
                        .show_value(false),
                );

                // Double-click returns one band to flat, which is much quicker
                // than nudging a slider back to exactly zero.
                if response.double_clicked() {
                    *gain = 0.0;
                }

                if *gain != before {
                    changed = true;
                }

                let value = *gain;
                ui.add_space(m.space(0.5));
                ui.label(
                    // A whole number reads better without a trailing ".0", but
                    // rounding 3.5 dB to "+4" tells the user something untrue
                    // about their own setting.
                    RichText::new(if (value - value.round()).abs() < 0.05 {
                        format!("{:+.0}", value.round())
                    } else {
                        format!("{value:+.1}")
                    })
                    .text_style(TextStyle::Name("caption".into()))
                    .color(if value.abs() < 0.05 {
                        col(p.text_muted)
                    } else {
                        col(p.accent)
                    }),
                );
                ui.label(
                    RichText::new(band_label(index))
                        .text_style(TextStyle::Name("caption".into()))
                        .color(col(p.text_muted)),
                );
            });
        }
    });

    changed
}

/// Preamp slider, plus the one-click fix for a clipping curve.
fn preamp_row(
    ui: &mut Ui,
    theme: &Theme,
    bank: &Bank,
    config: &mut Equalizer,
    outcome: &mut Outcome,
) {
    let m = theme.metrics;
    let p = theme.palette;

    ui.horizontal(|ui| {
        ui.label(
            RichText::new("Preamp")
                .text_style(TextStyle::Body)
                .color(col(p.text_secondary)),
        );
        ui.add_space(m.space(1.0));

        if ui
            .add(
                egui::Slider::new(
                    &mut config.preamp_db,
                    -Equalizer::MAX_PREAMP_DB..=Equalizer::MAX_PREAMP_DB,
                )
                .suffix(" dB")
                .fixed_decimals(1),
            )
            .changed()
        {
            outcome.changed = true;
        }

        // Only offered when it would actually do something.
        let headroom = bank.peak_gain_db();
        if headroom > 0.1 {
            ui.add_space(m.space(1.0));
            if widgets::accent_button(ui, theme, "Fix clipping")
                .on_hover_text(format!(
                    "This curve peaks at {headroom:+.1} dB. Lower the preamp to match."
                ))
                .clicked()
            {
                config.preamp_db = (config.preamp_db + bank.suggested_preamp_db())
                    .clamp(-Equalizer::MAX_PREAMP_DB, Equalizer::MAX_PREAMP_DB);
                outcome.changed = true;
            }
        }

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if widgets::accent_button(ui, theme, "Reset").clicked() {
                config.gains_db = vec![0.0; BAND_COUNT];
                config.preamp_db = 0.0;
                config.preset = Some(presets::FLAT.name.to_owned());
                outcome.changed = true;
            }
        });
    });
}

/// Preset buttons.
fn preset_row(ui: &mut Ui, theme: &Theme, config: &mut Equalizer, outcome: &mut Outcome) {
    let p = theme.palette;

    // Derived from the curve itself rather than from the remembered name.
    //
    // `preset` cannot survive a round-trip through TOML: `None` has no
    // representation, so serde drops the key and the struct default puts
    // "Flat" back on load. A hand-edited curve then reopened claiming to be
    // Flat, which is the interface asserting something untrue about the user's
    // own settings. Recomputing costs ten biquad comparisons and cannot go
    // stale.
    let selected = presets::matching(&config.gains_db, config.preamp_db).map(|p| p.name);

    ui.horizontal_wrapped(|ui| {
        for preset in presets::ALL {
            let is_selected = selected == Some(preset.name);

            let response = ui
                .selectable_label(
                    is_selected,
                    RichText::new(preset.name).color(if is_selected {
                        col(p.accent)
                    } else {
                        col(p.text_secondary)
                    }),
                )
                .on_hover_text(preset.description);

            if response.clicked() {
                config.gains_db = preset.gains();
                config.preamp_db = preset.preamp_db;
                config.preset = Some(preset.name.to_owned());
                outcome.changed = true;
                outcome.preset = Some(preset.name);
            }
        }
    });
}

/// Map a frequency to an x position. Logarithmic, the way hearing works.
fn freq_to_x(freq: f32, plot: Rect) -> f32 {
    let (low, high) = VIEW_RANGE_HZ;
    let t = (freq.max(low) / low).log10() / (high / low).log10();
    plot.left() + t.clamp(0.0, 1.0) * plot.width()
}

/// Map a decibel value to a y position, clamped to the visible range.
fn db_to_y(db: f32, plot: Rect) -> f32 {
    let t = (db.clamp(-VIEW_RANGE_DB, VIEW_RANGE_DB) + VIEW_RANGE_DB) / (2.0 * VIEW_RANGE_DB);
    plot.bottom() - t * plot.height()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plot() -> Rect {
        Rect::from_min_size(Pos2::new(10.0, 20.0), Vec2::new(400.0, 200.0))
    }

    /// A logarithmic axis is what makes the curve match how the bands are
    /// spaced: each octave has to take the same width.
    #[test]
    fn the_frequency_axis_is_logarithmic() {
        let plot = plot();

        let octaves = [100.0, 200.0, 400.0, 800.0];
        let positions: Vec<f32> = octaves.iter().map(|f| freq_to_x(*f, plot)).collect();

        let first = positions[1] - positions[0];
        for pair in positions.windows(2) {
            let width = pair[1] - pair[0];
            assert!(
                (width - first).abs() < 0.5,
                "octaves are unevenly spaced: {width} vs {first}"
            );
        }
    }

    #[test]
    fn the_axes_span_the_plot() {
        let plot = plot();
        assert!((freq_to_x(20.0, plot) - plot.left()).abs() < 0.01);
        assert!((freq_to_x(20_000.0, plot) - plot.right()).abs() < 0.01);
        assert!((db_to_y(0.0, plot) - plot.center().y).abs() < 0.01);
    }

    /// Louder must be higher up, or the curve reads upside down.
    #[test]
    fn positive_gain_draws_above_the_zero_line() {
        let plot = plot();
        assert!(db_to_y(6.0, plot) < db_to_y(0.0, plot));
        assert!(db_to_y(-6.0, plot) > db_to_y(0.0, plot));
    }

    /// A curve beyond the drawn range must stay inside the box rather than
    /// painting over the rest of the view.
    #[test]
    fn out_of_range_values_are_clamped_into_the_plot() {
        let plot = plot();
        for db in [-100.0, -30.0, 30.0, 100.0] {
            let y = db_to_y(db, plot);
            assert!(
                y >= plot.top() && y <= plot.bottom(),
                "{db} dB drew at {y}, outside {plot:?}"
            );
        }
        assert!(freq_to_x(1.0, plot) >= plot.left());
        assert!(freq_to_x(100_000.0, plot) <= plot.right());
    }

    /// Every band has to be visible on the curve, or a slider would appear to
    /// do nothing.
    #[test]
    fn every_band_falls_inside_the_drawn_range() {
        let plot = plot();
        for freq in BAND_FREQUENCIES {
            let x = freq_to_x(freq, plot);
            assert!(
                x > plot.left() - 0.01 && x < plot.right() + 0.01,
                "{freq} Hz drew at {x}"
            );
        }
    }
}
