//! The four grouped views — Artists, Albums, Genres, Folders.
//!
//! One file rather than four, because they differ only in what a card says and
//! whether its artwork is round. Sharing the surface means the grid geometry,
//! the virtualisation and the hover behaviour are written once and behave
//! identically everywhere, which is most of what makes a set of views feel like
//! one application rather than four screens.

use egui::{Align, Layout, Rect, RichText, Sense, TextStyle, Ui, Vec2};
use mp_core::library::ArtSize;

use crate::artwork::Artwork;
use crate::theme::{Theme, col, col_alpha};
use crate::widgets::{self, icons::Icon};

/// One entry in a grouped view, flattened to what drawing needs.
pub struct Card<'a> {
    pub title: &'a str,
    /// Second line: artist, track count, running time.
    pub subtitle: String,
    pub art_id: Option<&'a str>,
    /// Artists read as people; a circle says that without a label.
    pub round: bool,
    /// Drawn when there is no cover.
    pub fallback: Icon,
}

/// What the user did in a grouped view.
#[derive(Debug, Default, Clone, Copy)]
pub struct Outcome {
    /// A card was clicked: open this index.
    pub open: Option<usize>,
    /// A card's play affordance was used.
    pub play: Option<usize>,
}

/// Draw a grid of cards, building only the rows on screen.
///
/// `card` is called for the visible range only, so a 5,000-artist library
/// costs the same per frame as a 20-artist one.
pub fn grid<'a>(
    ui: &mut Ui,
    theme: &Theme,
    artwork: &mut Artwork,
    art_cache: &mp_core::library::ArtCache,
    count: usize,
    mut card: impl FnMut(usize) -> Card<'a>,
) -> Outcome {
    let mut outcome = Outcome::default();
    let m = theme.metrics;

    let target = m.space(17.0);
    let gap = m.space(1.5);
    let available = ui.available_width();

    // At least two columns, so a narrow window degrades to a dense list rather
    // than one enormous card per row.
    let columns = (((available + gap) / (target + gap)).floor() as usize).max(2);
    let cell = ((available - gap * (columns - 1) as f32) / columns as f32).max(m.space(8.0));

    // Art, then two lines of text and their padding.
    let row_height = cell + m.space(5.0) + gap;
    let rows = count.div_ceil(columns);

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show_rows(ui, row_height, rows, |ui, range| {
            for row in range {
                ui.horizontal(|ui| {
                    for column in 0..columns {
                        let index = row * columns + column;
                        if index >= count {
                            break;
                        }

                        let data = card(index);
                        let response =
                            draw_card(ui, theme, artwork, art_cache, &data, cell, row_height - gap);

                        if response.clicked() {
                            outcome.open = Some(index);
                        }
                        if response.double_clicked() {
                            outcome.play = Some(index);
                        }

                        if column + 1 < columns {
                            ui.add_space(gap);
                        }
                    }
                });
                ui.add_space(gap);
            }
        });

    outcome
}

/// A single card: artwork, title, subtitle.
fn draw_card(
    ui: &mut Ui,
    theme: &Theme,
    artwork: &mut Artwork,
    art_cache: &mp_core::library::ArtCache,
    card: &Card<'_>,
    width: f32,
    height: f32,
) -> egui::Response {
    let m = theme.metrics;
    let p = theme.palette;

    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click());

    if !ui.is_rect_visible(rect) {
        return response;
    }

    let hovered =
        ui.ctx()
            .animate_bool_with_time(response.id.with("hover"), response.hovered(), 0.12);

    let painter = ui.painter();

    if hovered > 0.0 {
        painter.rect_filled(
            rect.expand(m.space(0.5)),
            egui::CornerRadius::same(m.radius_medium),
            col_alpha(p.bg_hover, hovered * 0.8),
        );
    }

    // Artwork fills the square at the top of the card.
    let art_rect = Rect::from_min_size(rect.min, Vec2::splat(width));
    let radius = if card.round {
        egui::CornerRadius::same(u8::try_from((width / 2.0) as u32).unwrap_or(u8::MAX))
    } else {
        egui::CornerRadius::same(m.radius_medium)
    };

    painter.rect_filled(art_rect, radius, col(p.bg_elevated));

    let texture = card
        .art_id
        .and_then(|id| artwork.get(ui.ctx(), art_cache, id, ArtSize::Card));

    match texture {
        Some(texture) => {
            // `paint_at` respects the corner radius, so round artist cards clip
            // properly instead of showing square corners over the circle.
            egui::Image::from_texture(&texture)
                .corner_radius(radius)
                .paint_at(ui, art_rect);
        }
        None => {
            crate::widgets::icons::draw(
                ui.painter(),
                card.fallback,
                art_rect.shrink(width * 0.32),
                col_alpha(p.text_muted, 0.45),
                1.6,
            );
        }
    }

    // A subtle lift under the art gives the grid depth without a hard border.
    ui.painter().rect_stroke(
        art_rect,
        radius,
        egui::Stroke::new(1.0, col_alpha(p.border, 0.6)),
        egui::StrokeKind::Inside,
    );

    let text_top = art_rect.bottom() + m.space(0.75);
    let title =
        crate::views::songs::truncated(ui, card.title, width, TextStyle::Body, col(p.text_primary));
    let subtitle = crate::views::songs::truncated(
        ui,
        &card.subtitle,
        width,
        TextStyle::Name("caption".into()),
        col(p.text_muted),
    );

    let painter = ui.painter();
    painter.galley(
        egui::Pos2::new(rect.left(), text_top),
        title,
        col(p.text_primary),
    );
    painter.galley(
        egui::Pos2::new(rect.left(), text_top + m.space(2.0)),
        subtitle,
        col(p.text_muted),
    );

    response
}

/// A compact list, for groups where artwork adds nothing.
///
/// Genres are words, not pictures; a grid of near-identical squares labelled
/// "Rock", "Pop", "Trap" is worse than a dense list you can read down.
pub fn list(
    ui: &mut Ui,
    theme: &Theme,
    count: usize,
    mut row: impl FnMut(usize) -> (String, String),
) -> Outcome {
    let mut outcome = Outcome::default();
    let m = theme.metrics;
    let p = theme.palette;
    let height = m.row_height * 0.8;

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show_rows(ui, height, count, |ui, range| {
            for index in range {
                let (title, subtitle) = row(index);

                let (rect, response) =
                    ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::click());
                if !ui.is_rect_visible(rect) {
                    continue;
                }

                let hovered = ui.ctx().animate_bool_with_time(
                    response.id.with("hover"),
                    response.hovered(),
                    0.1,
                );
                if hovered > 0.0 {
                    ui.painter().rect_filled(
                        rect,
                        egui::CornerRadius::same(m.radius_small),
                        col_alpha(p.bg_hover, hovered),
                    );
                }

                let pad = m.space(1.25);

                // The detail is short but not tiny - "289 tracks · 18 h 6 m" -
                // and truncating it to an ellipsis loses the only number on the
                // row worth reading. It gets a third of the width, and the
                // title (which can be arbitrarily long) absorbs the rest.
                let detail_width = (rect.width() * 0.33).clamp(m.space(8.0), m.space(18.0));
                let title_galley = crate::views::songs::truncated(
                    ui,
                    &title,
                    (rect.width() - pad * 2.0 - detail_width).max(0.0),
                    TextStyle::Body,
                    col(p.text_primary),
                );
                let count_galley = crate::views::songs::truncated(
                    ui,
                    &subtitle,
                    detail_width,
                    TextStyle::Name("caption".into()),
                    col(p.text_muted),
                );

                let painter = ui.painter();
                painter.galley(
                    egui::Pos2::new(
                        rect.left() + pad,
                        rect.center().y - title_galley.size().y * 0.5,
                    ),
                    title_galley,
                    col(p.text_primary),
                );
                painter.galley(
                    egui::Pos2::new(
                        rect.right() - pad - count_galley.size().x,
                        rect.center().y - count_galley.size().y * 0.5,
                    ),
                    count_galley,
                    col(p.text_muted),
                );

                if response.clicked() {
                    outcome.open = Some(index);
                }
            }
        });

    outcome
}

/// Heading for a view the user has drilled into, with the way back.
///
/// Returns whether the back control was used.
pub fn focus_header(ui: &mut Ui, theme: &Theme, title: &str, subtitle: Option<&str>) -> bool {
    let m = theme.metrics;
    let p = theme.palette;
    let mut back = false;

    ui.horizontal(|ui| {
        if widgets::icon_button_labelled(ui, theme, Icon::ChevronLeft, m.space(3.0), false, "Back")
            .clicked()
        {
            back = true;
        }

        ui.add_space(m.space(0.5));

        ui.vertical(|ui| {
            ui.label(
                RichText::new(title)
                    .text_style(TextStyle::Heading)
                    .color(col(p.text_primary)),
            );
            if let Some(subtitle) = subtitle {
                ui.label(
                    RichText::new(subtitle)
                        .text_style(TextStyle::Body)
                        .color(col(p.text_secondary)),
                );
            }
        });
    });

    back
}

/// A heading with a count and, on the right, whatever controls belong to it.
pub fn section_header(
    ui: &mut Ui,
    theme: &Theme,
    title: &str,
    detail: &str,
    right: impl FnOnce(&mut Ui),
) {
    let m = theme.metrics;
    let p = theme.palette;

    ui.horizontal(|ui| {
        ui.label(
            RichText::new(title)
                .text_style(TextStyle::Heading)
                .color(col(p.text_primary)),
        );
        ui.add_space(m.space(1.0));
        ui.label(
            RichText::new(detail)
                .text_style(TextStyle::Body)
                .color(col(p.text_muted)),
        );

        ui.with_layout(Layout::right_to_left(Align::Center), right);
    });
}

/// Format a running time as `1 h 12 m` or `4 m`.
///
/// Used on cards, where a `1:12:33` timestamp reads as a position in a track
/// rather than as the length of a collection.
pub fn duration_label(duration: std::time::Duration) -> String {
    let minutes = duration.as_secs() / 60;
    if minutes >= 60 {
        format!("{} h {} m", minutes / 60, minutes % 60)
    } else {
        format!("{minutes} m")
    }
}

/// "1 track" / "12 tracks", because "1 tracks" looks like a bug.
pub fn track_count_label(count: u32) -> String {
    if count == 1 {
        "1 track".to_owned()
    } else {
        format!("{count} tracks")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_single_track_is_not_pluralised() {
        assert_eq!(track_count_label(1), "1 track");
        assert_eq!(track_count_label(0), "0 tracks");
        assert_eq!(track_count_label(12), "12 tracks");
    }

    #[test]
    fn collection_lengths_read_as_durations_not_timestamps() {
        assert_eq!(duration_label(Duration::from_secs(240)), "4 m");
        assert_eq!(duration_label(Duration::from_secs(4353)), "1 h 12 m");
        assert_eq!(duration_label(Duration::from_secs(7200)), "2 h 0 m");
    }
}
