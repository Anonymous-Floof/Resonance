//! The tag editor dialog.
//!
//! Drawn as a modal window over whatever view is behind it, because editing a
//! file is not a browsing activity and should not be something you wander away
//! from half-finished.

use crate::tag_editor::TagEditor;
use crate::theme::{Theme, col, col_alpha};
use crate::widgets;
use egui::{Align, Layout, RichText, TextStyle, Ui};

/// What the dialog asked for this frame.
#[derive(Debug, Default)]
pub struct Outcome {
    pub close: bool,
    /// Work out what would change and move to the confirmation step.
    pub review: bool,
    /// Write it. Only ever set from the confirmation step.
    pub apply: bool,
    pub back: bool,
    pub reset: bool,
}

pub fn show(ctx: &egui::Context, theme: &Theme, editor: &mut TagEditor) -> Outcome {
    let mut outcome = Outcome::default();

    if !editor.is_open() {
        return outcome;
    }

    let m = theme.metrics;
    let p = theme.palette;

    // A scrim over the app behind, so it is obvious the rest is not live.
    let screen = ctx.viewport_rect();
    egui::Area::new("tag_editor_scrim".into())
        .order(egui::Order::Middle)
        .fixed_pos(screen.min)
        .show(ctx, |ui| {
            ui.painter()
                .rect_filled(screen, egui::CornerRadius::ZERO, col_alpha(p.bg_base, 0.72));
            // Swallow clicks so the list behind cannot be operated through it.
            ui.allocate_rect(screen, egui::Sense::click_and_drag());
        });

    egui::Window::new("Edit tags")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .default_width(m.space(62.0))
        .frame(
            egui::Frame::new()
                .fill(theme.card_fill())
                .stroke(egui::Stroke::new(1.0, col(p.border)))
                .corner_radius(egui::CornerRadius::same(m.radius_large))
                .inner_margin(egui::Margin::same(m.space(2.0) as i8)),
        )
        .show(ctx, |ui| {
            heading(ui, theme, editor);

            ui.add_space(m.space(1.0));
            widgets::separator(ui, theme);
            ui.add_space(m.space(1.25));

            if editor.is_confirming() {
                confirmation(ui, theme, editor, &mut outcome);
            } else {
                form(ui, theme, editor, &mut outcome);
            }
        });

    // Escape backs out of the confirmation first, then closes.
    if ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
        if editor.is_confirming() {
            outcome.back = true;
        } else {
            outcome.close = true;
        }
    }

    outcome
}

fn heading(ui: &mut Ui, theme: &Theme, editor: &TagEditor) {
    let p = theme.palette;

    ui.label(
        RichText::new(editor.title())
            .text_style(TextStyle::Name("title".into()))
            .color(col(p.text_primary)),
    );

    // The full path, because "which file am I about to change" is the single
    // most important thing to be sure of on this screen.
    ui.label(
        RichText::new(editor.path())
            .text_style(TextStyle::Name("caption".into()))
            .color(col(p.text_muted)),
    );
}

fn form(ui: &mut Ui, theme: &Theme, editor: &mut TagEditor, outcome: &mut Outcome) {
    let m = theme.metrics;
    let p = theme.palette;

    if let Some(error) = editor.error() {
        ui.label(
            RichText::new(error)
                .text_style(TextStyle::Name("caption".into()))
                .color(col(p.error)),
        );
        ui.add_space(m.space(1.0));
    }

    let label_width = m.space(15.0);

    egui::Grid::new("tag_fields")
        .num_columns(2)
        // The label column is sized here rather than by allocating a fixed
        // rectangle per row. Allocating one squeezed the labels into whatever
        // egui had already decided the column was, and every one of them came
        // out clipped mid-word — "Album artist" as "Album", "Genre" as "(".
        .min_col_width(label_width)
        .spacing([m.space(1.5), m.space(0.75)])
        .show(ui, |ui| {
            for (field, value) in editor.fields_mut() {
                ui.label(
                    RichText::new(field.label())
                        .text_style(TextStyle::Name("caption".into()))
                        .color(col(p.text_secondary)),
                );

                // Numbers get a short box because a wide one for a four-digit
                // year reads as an invitation to type a sentence. Digits are
                // not enforced as you type — a half-typed number is a normal
                // state, and rejecting keystrokes mid-entry is hostile.
                let width = if field.is_numeric() {
                    m.space(10.0)
                } else {
                    m.space(34.0)
                };

                ui.add(
                    egui::TextEdit::singleline(value)
                        .desired_width(width)
                        .hint_text("—"),
                );

                ui.end_row();
            }
        });

    ui.add_space(m.space(1.5));

    ui.horizontal(|ui| {
        if ui.button("Cancel").clicked() {
            outcome.close = true;
        }

        if editor.is_dirty() {
            ui.add_space(m.space(0.5));
            if ui
                .button("Revert changes")
                .on_hover_text("Put every field back to what the file holds")
                .clicked()
            {
                outcome.reset = true;
            }
        }

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            // Nothing to save means no button to press. An enabled Save on an
            // untouched form invites a pointless write.
            if editor.is_dirty() {
                if widgets::accent_button(ui, theme, "Review changes").clicked() {
                    outcome.review = true;
                }
            } else {
                ui.label(
                    RichText::new("Nothing changed yet")
                        .text_style(TextStyle::Name("caption".into()))
                        .color(col(p.text_muted)),
                );
            }
        });
    });
}

fn confirmation(ui: &mut Ui, theme: &Theme, editor: &TagEditor, outcome: &mut Outcome) {
    let m = theme.metrics;
    let p = theme.palette;

    ui.label(
        RichText::new("This will write to the file on disk:")
            .text_style(TextStyle::Name("caption".into()))
            .color(col(p.text_secondary)),
    );
    ui.add_space(m.space(1.0));

    if editor.pending().is_empty() {
        ui.label(
            RichText::new("Nothing would change after all.")
                .text_style(TextStyle::Name("caption".into()))
                .color(col(p.text_muted)),
        );
    }

    for change in editor.pending() {
        ui.horizontal(|ui| {
            ui.add_sized(
                [m.space(15.0), m.space(2.5)],
                egui::Label::new(
                    RichText::new(change.field.label())
                        .text_style(TextStyle::Name("caption".into()))
                        .color(col(p.text_secondary)),
                )
                .wrap_mode(egui::TextWrapMode::Extend),
            );

            ui.label(
                RichText::new(shown(change.before.as_deref()))
                    .text_style(TextStyle::Name("caption".into()))
                    .color(col(p.text_muted))
                    .strikethrough(),
            );
            ui.label(
                RichText::new("→")
                    .text_style(TextStyle::Name("caption".into()))
                    .color(col(p.text_muted)),
            );
            ui.label(
                RichText::new(shown(change.after.as_deref()))
                    .text_style(TextStyle::Name("caption".into()))
                    .color(col(p.accent)),
            );
        });
    }

    ui.add_space(m.space(1.5));
    ui.label(
        RichText::new(
            "Your music files are modified. This can be undone from the edit \
             history in Settings.",
        )
        .text_style(TextStyle::Name("caption".into()))
        .color(col(p.warning)),
    );

    ui.add_space(m.space(1.5));

    ui.horizontal(|ui| {
        if ui.button("Back").clicked() {
            outcome.back = true;
        }

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if !editor.pending().is_empty()
                && widgets::accent_button(ui, theme, "Write to file").clicked()
            {
                outcome.apply = true;
            }
        });
    });
}

/// How an absent value reads in the diff.
fn shown(value: Option<&str>) -> String {
    match value {
        Some(value) => value.to_owned(),
        None => "(empty)".to_owned(),
    }
}
