//! The first thing a new user sees.
//!
//! Shown once, on the first run, until there is a library to look at. It has
//! two jobs and deliberately no more.
//!
//! The first is to get the user to the one action that makes the app useful:
//! pointing it at a folder. Everything else can be discovered later; without a
//! library there is nothing to discover.
//!
//! The second is to state the two promises the whole design rests on — that
//! the app does not touch the user's files, and does not touch the network.
//! Both are unusual enough to be worth saying out loud, and both are the kind
//! of thing someone wants to know *before* they point a new program at their
//! music collection rather than after.

use egui::{Align, Layout, RichText, TextStyle, Ui, Vec2};

use crate::theme::{Theme, col, col_alpha};
use crate::widgets::{self, icons::Icon};

/// What the welcome screen is asking the shell to do.
#[derive(Debug, Default, Clone, Copy)]
pub struct Outcome {
    /// Open the folder picker.
    pub add_folder: bool,
    /// Put the welcome away without adding anything.
    pub dismiss: bool,
}

/// One reassurance, with the detail underneath it.
const PROMISES: &[(Icon, &str, &str)] = &[
    (
        Icon::Songs,
        "Your files are left alone",
        "Resonance reads your music and never renames, moves or rewrites it. \
         Tag editing is off until you turn it on, and every edit can be undone.",
    ),
    (
        Icon::Search,
        "Everything works offline",
        "The library, artwork and suggestions are all built on your own machine. \
         Nothing is uploaded, and no lookups leave your computer.",
    ),
];

pub fn show(ui: &mut Ui, theme: &Theme, scanning: bool) -> Outcome {
    let mut outcome = Outcome::default();
    let m = theme.metrics;
    let p = theme.palette;

    ui.vertical_centered(|ui| {
        // Held to a readable measure. Full-width prose across a maximised
        // window is a wall of text nobody reads.
        let width = ui.available_width().min(m.space(58.0));

        ui.allocate_ui_with_layout(
            Vec2::new(width, ui.available_height()),
            Layout::top_down(Align::Center),
            |ui| {
                ui.add_space(m.space(5.0));

                ui.label(
                    RichText::new("Welcome to Resonance")
                        .text_style(TextStyle::Heading)
                        .color(col(p.text_primary)),
                );

                ui.add_space(m.space(1.0));
                ui.label(
                    RichText::new("A music player for the collection you already have.")
                        .text_style(TextStyle::Body)
                        .color(col(p.text_secondary)),
                );

                ui.add_space(m.space(4.0));

                for (icon, title, body) in PROMISES {
                    promise(ui, theme, *icon, title, body);
                    ui.add_space(m.space(2.0));
                }

                ui.add_space(m.space(2.0));

                if scanning {
                    // The button would do nothing useful mid-scan, and the
                    // folder is already chosen.
                    ui.label(
                        RichText::new("Looking through your folder…")
                            .text_style(TextStyle::Body)
                            .color(col(p.text_secondary)),
                    );
                } else {
                    if widgets::accent_button(ui, theme, "Choose your music folder").clicked() {
                        outcome.add_folder = true;
                    }

                    ui.add_space(m.space(1.0));
                    if ui
                        .button(
                            RichText::new("I'll do it later")
                                .text_style(TextStyle::Name("caption".into()))
                                .color(col(p.text_muted)),
                        )
                        .clicked()
                    {
                        outcome.dismiss = true;
                    }
                }
            },
        );
    });

    outcome
}

/// One icon-and-text row.
fn promise(ui: &mut Ui, theme: &Theme, icon: Icon, title: &str, body: &str) {
    let m = theme.metrics;
    let p = theme.palette;

    ui.horizontal_top(|ui| {
        let size = m.space(3.0);
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), egui::Sense::hover());

        if ui.is_rect_visible(rect) {
            ui.painter()
                .circle_filled(rect.center(), size * 0.5, col_alpha(p.accent, 0.16));
            widgets::icons::draw(
                ui.painter(),
                icon,
                rect.shrink(size * 0.28),
                col(p.accent),
                1.5,
            );
        }

        ui.add_space(m.space(1.25));

        ui.vertical(|ui| {
            ui.label(
                RichText::new(title)
                    .text_style(TextStyle::Body)
                    .color(col(p.text_primary)),
            );
            ui.add_space(m.space(0.25));
            ui.label(
                RichText::new(body)
                    .text_style(TextStyle::Name("caption".into()))
                    .color(col(p.text_muted)),
            );
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every promise has to be a promise the app actually keeps, and has to
    /// read as one — an empty title or body would render as a stray icon.
    #[test]
    fn every_promise_is_filled_in() {
        assert!(
            !PROMISES.is_empty(),
            "a welcome with no content is a blank page"
        );

        for (_, title, body) in PROMISES {
            assert!(!title.is_empty());
            assert!(!body.is_empty());
            assert!(
                body.len() > title.len(),
                "{title:?} has a body shorter than its own heading"
            );
        }
    }

    /// The two claims made here are the app's standing design decisions. If
    /// either stops being true the welcome becomes a lie, so they are pinned.
    #[test]
    fn the_promises_are_the_ones_the_design_actually_makes() {
        let text: String = PROMISES
            .iter()
            .map(|(_, title, body)| format!("{title} {body}"))
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();

        assert!(text.contains("never renames, moves or rewrites"));
        assert!(text.contains("tag editing is off"));
        assert!(text.contains("undone"));
        assert!(text.contains("offline"));
        assert!(text.contains("nothing is uploaded"));
    }
}
