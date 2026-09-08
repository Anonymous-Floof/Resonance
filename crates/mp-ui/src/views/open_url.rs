//! Paste a link, and play what is behind it.
//!
//! A modal for the same reason the tag editor is one: it is a single errand
//! with an answer, not somewhere to browse, and while it is working there is
//! exactly one thing worth looking at.
//!
//! The dialog never decides anything. It collects a string, says what the
//! worker is doing, and reports back — whether the link is one this build can
//! use is settled in `mp-net`, and whether the feature exists at all is
//! settled by the setting.

use egui::{RichText, TextStyle, Ui};

use crate::theme::{Theme, col, col_alpha};
use crate::widgets;

/// What the user has typed, and what came of it.
#[derive(Debug, Default)]
pub struct OpenUrl {
    open: bool,
    text: String,
    /// Why the last attempt came to nothing, kept on screen until the next.
    problem: Option<String>,
}

impl OpenUrl {
    pub fn open(&mut self) {
        self.open = true;
        self.problem = None;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.text.clear();
        self.problem = None;
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn text(&self) -> &str {
        self.text.trim()
    }

    /// Report why nothing played, and leave the link in the box so it can be
    /// looked at rather than retyped.
    pub fn set_problem(&mut self, why: String) {
        self.problem = Some(why);
    }
}

/// What the dialog asked for this frame.
#[derive(Debug, Default)]
pub struct Outcome {
    pub close: bool,
    /// Try the link that is in the box.
    pub play: bool,
}

/// Draw it.
///
/// `working` is the worker's own description of what it is doing, or `None`
/// when it is idle. Passed in rather than read here so this file has no
/// opinion about how the work is done.
pub fn show(
    ctx: &egui::Context,
    theme: &Theme,
    state: &mut OpenUrl,
    working: Option<&str>,
) -> Outcome {
    let mut outcome = Outcome::default();

    if !state.open {
        return outcome;
    }

    let m = theme.metrics;
    let p = theme.palette;

    // A scrim over the app behind, so it is obvious the rest is not live.
    let screen = ctx.viewport_rect();
    egui::Area::new("open_url_scrim".into())
        .order(egui::Order::Middle)
        .fixed_pos(screen.min)
        .show(ctx, |ui| {
            ui.painter()
                .rect_filled(screen, egui::CornerRadius::ZERO, col_alpha(p.bg_base, 0.72));
            ui.allocate_rect(screen, egui::Sense::click_and_drag());
        });

    egui::Window::new("Play from a link")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .default_width(m.space(56.0))
        .frame(
            egui::Frame::new()
                .fill(theme.card_fill())
                .stroke(egui::Stroke::new(1.0, col(p.border)))
                .corner_radius(egui::CornerRadius::same(m.radius_large))
                .inner_margin(egui::Margin::same(m.space(2.0) as i8)),
        )
        .show(ctx, |ui| {
            body(ui, theme, state, working, &mut outcome);
        });

    if ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
        outcome.close = true;
    }

    outcome
}

fn body(
    ui: &mut Ui,
    theme: &Theme,
    state: &mut OpenUrl,
    working: Option<&str>,
    outcome: &mut Outcome,
) {
    let m = theme.metrics;
    let p = theme.palette;

    ui.label(
        RichText::new("Play from a link")
            .text_style(TextStyle::Name("title".into()))
            .color(col(p.text_primary)),
    );
    ui.label(
        RichText::new("A YouTube or YouTube Music link. The audio is fetched to this machine and played from there; nothing is added to your library and nothing is written to your music folders.")
            .text_style(TextStyle::Name("caption".into()))
            .color(col(p.text_muted)),
    );

    ui.add_space(m.space(1.0));
    widgets::separator(ui, theme);
    ui.add_space(m.space(1.25));

    let busy = working.is_some();

    let field = ui.add_enabled(
        !busy,
        egui::TextEdit::singleline(&mut state.text)
            .hint_text("https://www.youtube.com/watch?v=...")
            .desired_width(f32::INFINITY),
    );

    // The box takes focus on the frame it opens, so a paste can go straight in
    // without reaching for the mouse first.
    if field.has_focus() {
        // Already there.
    } else if !busy && state.problem.is_none() {
        field.request_focus();
    }

    let submitted =
        field.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)) && !busy;

    if let Some(problem) = &state.problem {
        ui.add_space(m.space(1.0));
        ui.label(
            RichText::new(problem)
                .text_style(TextStyle::Name("caption".into()))
                .color(col(p.error)),
        );
    }

    if let Some(working) = working {
        ui.add_space(m.space(1.0));
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(
                RichText::new(format!("{working}\u{2026}"))
                    .text_style(TextStyle::Name("caption".into()))
                    .color(col(p.text_secondary)),
            );
        });
    }

    ui.add_space(m.space(1.5));

    ui.horizontal(|ui| {
        let ready = !busy && !state.text.trim().is_empty();

        if ui
            .add_enabled_ui(ready, |ui| widgets::accent_button(ui, theme, "Play"))
            .inner
            .clicked()
        {
            outcome.play = true;
        }

        if ui.button("Cancel").clicked() {
            outcome.close = true;
        }
    });

    if submitted && !state.text.trim().is_empty() {
        outcome.play = true;
    }
}
