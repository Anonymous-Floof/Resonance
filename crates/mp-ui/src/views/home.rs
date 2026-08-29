//! The Home page: what you have listened to, and the quickest way back to it.
//!
//! Every section is drawn only when it has something to say, so the page grows
//! with the history rather than presenting a wall of zeroes on day one. The
//! one thing always shown is the library summary, which is true the moment a
//! folder has been scanned.
//!
//! Nothing here computes a statistic. The numbers arrive already worked out by
//! `mp_core::library::stats`, so what counts as a play is decided in one place
//! and stays testable without a UI.

use egui::{Sense, TextStyle, Ui};
use mp_core::library::Track;
use mp_core::library::stats::{PlayedTrack, Ranked, Totals};

use crate::theme::{Theme, col, col_alpha};
use crate::widgets;
use crate::widgets::icons::Icon;

/// Days of history in the activity chart.
pub const ACTIVITY_DAYS: usize = 30;

/// How many rows each list section shows.
pub const LIST_LIMIT: usize = 5;

/// How many cards a row of artists or albums shows.
pub const CARD_LIMIT: usize = 6;

/// Which list a clicked row came from.
///
/// Carried back rather than resolved here so the caller can queue the whole
/// list behind the row that was clicked — pressing a favourite should start a
/// run through the favourites, not strand you on a queue of one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Favourites,
    Recent,
}

/// A playlist reduced to what a quick-access card needs.
#[derive(Debug, Clone)]
pub struct Shortcut {
    pub id: i64,
    pub name: String,
    pub tracks: usize,
}

/// Everything the page displays.
#[derive(Clone, Copy)]
pub struct Data<'a> {
    pub totals: &'a Totals,
    pub activity: &'a [u32],
    pub favourites: &'a [PlayedTrack],
    pub recent: &'a [Track],
    pub artists: &'a [Ranked],
    pub albums: &'a [Ranked],
    pub playlists: &'a [Shortcut],
}

/// What the user did on the page this frame.
#[derive(Debug, Default, Clone)]
pub struct Outcome {
    /// Start playing row `n` of the given list.
    pub play: Option<(Source, usize)>,
    pub open_artist: Option<i64>,
    pub open_album: Option<i64>,
    pub open_playlist: Option<i64>,
    /// The "Browse songs" affordance was used.
    pub browse: bool,
}

/// Draw the page.
pub fn show(ui: &mut Ui, theme: &Theme, data: Data<'_>) -> Outcome {
    let mut outcome = Outcome::default();
    let m = theme.metrics;

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            summary(ui, theme, data.totals);
            ui.add_space(m.space(2.0));

            if data.totals.has_history() {
                if data.activity.iter().any(|&plays| plays > 0) {
                    section(ui, theme, "Recent activity", |ui| {
                        activity_chart(ui, theme, data.activity);
                    });
                }

                if !data.favourites.is_empty() {
                    section(ui, theme, "Your favourites", |ui| {
                        for (position, favourite) in
                            data.favourites.iter().take(LIST_LIMIT).enumerate()
                        {
                            let plays = format!(
                                "{} {}",
                                favourite.track.play_count,
                                if favourite.track.play_count == 1 {
                                    "play"
                                } else {
                                    "plays"
                                }
                            );

                            if track_row(ui, theme, position, &favourite.track, &plays) {
                                outcome.play = Some((Source::Favourites, position));
                            }
                        }
                    });
                }

                if !data.artists.is_empty() {
                    section(ui, theme, "Most played artists", |ui| {
                        if let Some(id) = cards(ui, theme, data.artists) {
                            outcome.open_artist = Some(id);
                        }
                    });
                }

                if !data.albums.is_empty() {
                    section(ui, theme, "Most played albums", |ui| {
                        if let Some(id) = cards(ui, theme, data.albums) {
                            outcome.open_album = Some(id);
                        }
                    });
                }

                if !data.recent.is_empty() {
                    section(ui, theme, "Jump back in", |ui| {
                        for (position, track) in data.recent.iter().take(LIST_LIMIT).enumerate() {
                            if track_row(ui, theme, position, track, "") {
                                outcome.play = Some((Source::Recent, position));
                            }
                        }
                    });
                }
            } else {
                nothing_yet(ui, theme, data.totals, &mut outcome);
            }

            if !data.playlists.is_empty() {
                section(ui, theme, "Your playlists", |ui| {
                    if let Some(id) = playlist_cards(ui, theme, data.playlists) {
                        outcome.open_playlist = Some(id);
                    }
                });
            }
        });

    outcome
}

// ---------------------------------------------------------------------------
// Sections
// ---------------------------------------------------------------------------

/// The four numbers across the top.
fn summary(ui: &mut Ui, theme: &Theme, totals: &Totals) {
    let m = theme.metrics;

    let explored = format!("{}%", (totals.explored() * 100.0).round() as i64);
    let library = format!(
        "{} {}",
        totals.tracks,
        if totals.tracks == 1 {
            "track"
        } else {
            "tracks"
        }
    );

    let tiles: [(&str, String, String); 4] = [
        (
            "Listening time",
            format_span(totals.listened_secs),
            listening_note(totals),
        ),
        (
            "Plays",
            totals.plays.to_string(),
            format!("across {} days", totals.active_days),
        ),
        (
            "Explored",
            explored,
            format!("{} of {}", totals.tracks_played, library),
        ),
        (
            "In your library",
            library,
            format!(
                "{} · {} albums",
                format_span(totals.library_secs),
                totals.albums
            ),
        ),
    ];

    ui.columns(tiles.len(), |columns| {
        for (column, (label, value, note)) in columns.iter_mut().zip(tiles) {
            egui::Frame::new()
                .fill(theme.card_fill())
                .corner_radius(egui::CornerRadius::same(m.radius_large))
                .inner_margin(egui::Margin::same(m.space(1.75) as i8))
                .show(column, |ui| {
                    ui.set_width(ui.available_width());

                    ui.label(
                        egui::RichText::new(label)
                            .text_style(TextStyle::Name("caption".into()))
                            .color(col(theme.palette.text_muted)),
                    );
                    ui.add_space(m.space(0.25));
                    ui.label(
                        egui::RichText::new(value)
                            .text_style(TextStyle::Name("title".into()))
                            .color(col(theme.palette.text_primary)),
                    );
                    if !note.is_empty() {
                        ui.label(
                            egui::RichText::new(note)
                                .text_style(TextStyle::Name("caption".into()))
                                .color(col_alpha(theme.palette.text_muted, 0.8)),
                        );
                    }
                });
        }
    });
}

/// The caveat under the listening-time tile, when there is one.
///
/// Plays recorded before listening was measured have no duration attached, and
/// silently leaving them out would make the headline number quietly wrong. The
/// honest thing is to say the total is a floor and how much is missing.
fn listening_note(totals: &Totals) -> String {
    if totals.unmeasured_plays == 0 {
        return if totals.plays == 0 {
            "nothing played yet".to_owned()
        } else {
            "measured".to_owned()
        };
    }

    format!(
        "at least — {} earlier {} not timed",
        totals.unmeasured_plays,
        if totals.unmeasured_plays == 1 {
            "play"
        } else {
            "plays"
        }
    )
}

/// Plays per day, oldest on the left.
fn activity_chart(ui: &mut Ui, theme: &Theme, activity: &[u32]) {
    let m = theme.metrics;
    let p = &theme.palette;

    let height = m.space(8.0);
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), Sense::hover());

    let peak = activity.iter().copied().max().unwrap_or(0).max(1) as f32;
    let count = activity.len().max(1);
    let slot = rect.width() / count as f32;
    let bar_width = (slot * 0.62).max(2.0);

    for (index, &plays) in activity.iter().enumerate() {
        let centre = rect.left() + slot * (index as f32 + 0.5);

        // Every day gets a visible foot, so a gap reads as "nothing here"
        // rather than as the chart having stopped.
        let filled = (plays as f32 / peak) * (rect.height() - 2.0);
        let bar = egui::Rect::from_min_max(
            egui::pos2(centre - bar_width * 0.5, rect.bottom() - filled.max(2.0)),
            egui::pos2(centre + bar_width * 0.5, rect.bottom()),
        );

        let colour = if plays == 0 {
            col_alpha(p.border, 0.7)
        } else {
            col(p.accent)
        };

        ui.painter()
            .rect_filled(bar, egui::CornerRadius::same(2), colour);
    }

    ui.add_space(m.space(0.5));
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(format!("{} days ago", activity.len()))
                .text_style(TextStyle::Name("caption".into()))
                .color(col(p.text_muted)),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                egui::RichText::new("today")
                    .text_style(TextStyle::Name("caption".into()))
                    .color(col(p.text_muted)),
            );
        });
    });
}

/// The state before anything has been played.
fn nothing_yet(ui: &mut Ui, theme: &Theme, totals: &Totals, outcome: &mut Outcome) {
    let m = theme.metrics;

    let (title, body) = if totals.tracks == 0 {
        (
            "Nothing here yet",
            "Add a music folder in Settings and Resonance will index it.",
        )
    } else {
        (
            "No listening history yet",
            "Play something and this page fills in: what you return to, who you \
             listen to most, and how much you have actually heard.",
        )
    };

    widgets::empty_state(ui, theme, Icon::Home, title, body);

    if totals.tracks > 0 {
        ui.vertical_centered(|ui| {
            ui.add_space(m.space(2.0));
            if widgets::accent_button(ui, theme, "Browse songs").clicked() {
                outcome.browse = true;
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Pieces
// ---------------------------------------------------------------------------

/// A titled group on a card.
fn section(ui: &mut Ui, theme: &Theme, title: &str, contents: impl FnOnce(&mut Ui)) {
    let m = theme.metrics;

    ui.label(
        egui::RichText::new(title)
            .text_style(TextStyle::Name("title".into()))
            .color(col(theme.palette.text_primary)),
    );
    ui.add_space(m.space(1.0));

    egui::Frame::new()
        .fill(theme.card_fill())
        .corner_radius(egui::CornerRadius::same(m.radius_large))
        .inner_margin(egui::Margin::same(m.space(1.5) as i8))
        .show(ui, contents);

    ui.add_space(m.space(2.5));
}

/// One clickable track line. Returns whether it was activated.
///
/// Always a single click, unlike the library list: a row here is a shortcut
/// the user came to the page to press, not one of thousands they are scrolling
/// past and might select by accident.
fn track_row(ui: &mut Ui, theme: &Theme, position: usize, track: &Track, trailing: &str) -> bool {
    let m = theme.metrics;
    let p = &theme.palette;

    let height = m.row_height * 0.78;
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), height), Sense::click());

    if response.hovered() {
        ui.painter().rect_filled(
            rect,
            egui::CornerRadius::same(m.radius_small),
            col(p.bg_hover),
        );
    }

    let pad = m.space(1.0);
    let caption = font(ui, "caption", 11.0);
    let body = font(ui, "body", 14.0);

    ui.painter().text(
        egui::pos2(rect.left() + pad + m.space(0.75), rect.center().y),
        egui::Align2::CENTER_CENTER,
        format!("{}", position + 1),
        caption.clone(),
        col_alpha(p.text_muted, 0.8),
    );

    let x = rect.left() + pad + m.space(2.5);

    // The trailing label claims its width first and the title takes what is
    // left. Without this a long title simply runs underneath it.
    let trailing_width = if trailing.is_empty() {
        0.0
    } else {
        ui.painter()
            .layout_no_wrap(trailing.to_owned(), caption.clone(), egui::Color32::WHITE)
            .rect
            .width()
            + m.space(1.5)
    };
    let text_width = (rect.right() - pad - trailing_width - x).max(0.0);

    let (title_y, subtitle_y) = widgets::stacked_lines(rect, &body, &caption, m.space(0.25));

    ui.painter().text(
        egui::pos2(x, title_y),
        egui::Align2::LEFT_CENTER,
        widgets::elide(ui, &track.title, &body, text_width),
        body.clone(),
        col(p.text_primary),
    );
    ui.painter().text(
        egui::pos2(x, subtitle_y),
        egui::Align2::LEFT_CENTER,
        widgets::elide(ui, &track.subtitle(), &caption, text_width),
        caption.clone(),
        col(p.text_muted),
    );

    if !trailing.is_empty() {
        ui.painter().text(
            egui::pos2(rect.right() - pad, rect.center().y),
            egui::Align2::RIGHT_CENTER,
            trailing,
            caption,
            col(p.text_muted),
        );
    }

    response.clicked()
}

/// A row of ranked artist or album cards. Returns the id of any that was hit.
fn cards(ui: &mut Ui, theme: &Theme, ranked: &[Ranked]) -> Option<i64> {
    let mut hit = None;

    ui.horizontal_wrapped(|ui| {
        for entry in ranked.iter().take(CARD_LIMIT) {
            let plays = format!(
                "{} {}",
                entry.plays,
                if entry.plays == 1 { "play" } else { "plays" }
            );

            if chip(ui, theme, &entry.name, &plays) {
                hit = Some(entry.id);
            }
        }
    });

    hit
}

/// A row of playlist cards. Returns the id of any that was hit.
fn playlist_cards(ui: &mut Ui, theme: &Theme, playlists: &[Shortcut]) -> Option<i64> {
    let mut hit = None;

    ui.horizontal_wrapped(|ui| {
        for playlist in playlists.iter().take(CARD_LIMIT) {
            let detail = format!(
                "{} {}",
                playlist.tracks,
                if playlist.tracks == 1 {
                    "track"
                } else {
                    "tracks"
                }
            );

            if chip(ui, theme, &playlist.name, &detail) {
                hit = Some(playlist.id);
            }
        }
    });

    hit
}

/// A small two-line clickable card.
fn chip(ui: &mut Ui, theme: &Theme, title: &str, detail: &str) -> bool {
    let m = theme.metrics;
    let p = &theme.palette;

    let size = egui::vec2(m.space(17.0), m.space(6.0));
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());

    let fill = if response.hovered() {
        col(p.bg_hover)
    } else {
        col(p.bg_elevated)
    };
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::same(m.radius_medium), fill);

    let body = font(ui, "body", 14.0);
    let caption = font(ui, "caption", 11.0);
    let pad = m.space(1.25);
    let width = rect.width() - pad * 2.0;

    let (title_y, detail_y) = widgets::stacked_lines(rect, &body, &caption, m.space(0.25));

    ui.painter().text(
        egui::pos2(rect.left() + pad, title_y),
        egui::Align2::LEFT_CENTER,
        widgets::elide(ui, title, &body, width),
        body.clone(),
        col(p.text_primary),
    );
    ui.painter().text(
        egui::pos2(rect.left() + pad, detail_y),
        egui::Align2::LEFT_CENTER,
        widgets::elide(ui, detail, &caption, width),
        caption,
        col(p.text_muted),
    );

    response.clicked()
}

/// Look up a named text style, falling back to a sensible size.
fn font(ui: &Ui, name: &str, fallback: f32) -> egui::FontId {
    let style = if name == "body" {
        TextStyle::Body
    } else {
        TextStyle::Name(name.into())
    };

    ui.style()
        .text_styles
        .get(&style)
        .cloned()
        .unwrap_or_else(|| egui::FontId::proportional(fallback))
}

/// A span of listening time, in the largest units that stay meaningful.
///
/// Deliberately not `format_duration`, which renders `mm:ss` for a seek bar.
/// A listening total is read as a quantity, not as a position in a track, and
/// `41:07:33` is not a number anyone parses at a glance.
pub fn format_span(seconds: f64) -> String {
    if !seconds.is_finite() || seconds < 1.0 {
        return "none".to_owned();
    }

    let total = seconds.round() as u64;
    let days = total / 86_400;
    let hours = (total % 86_400) / 3_600;
    let minutes = (total % 3_600) / 60;

    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{total}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_read_as_quantities() {
        assert_eq!(format_span(0.0), "none");
        assert_eq!(format_span(0.4), "none");
        assert_eq!(format_span(9.0), "9s");
        assert_eq!(format_span(90.0), "1m");
        assert_eq!(format_span(3_600.0), "1h 0m");
        assert_eq!(format_span(3_661.0), "1h 1m");
        assert_eq!(format_span(90_000.0), "1d 1h");
    }

    #[test]
    fn a_span_never_renders_a_clock_time() {
        // The bug this guards: reaching for `format_duration` and showing a
        // listening total of forty-one hours as "41:07:33".
        let span = format_span(147_000.0);
        assert!(!span.contains(':'), "{span}");
        assert_eq!(span, "1d 16h");
    }

    #[test]
    fn a_nonsense_total_does_not_panic() {
        assert_eq!(format_span(f64::NAN), "none");
        assert_eq!(format_span(f64::INFINITY), "none");
        assert_eq!(format_span(-5.0), "none");
    }

    #[test]
    fn the_listening_note_admits_what_it_cannot_count() {
        let mut totals = Totals {
            plays: 10,
            unmeasured_plays: 4,
            ..Totals::default()
        };
        let note = listening_note(&totals);
        assert!(note.starts_with("at least"), "{note}");
        assert!(note.contains('4'), "{note}");

        totals.unmeasured_plays = 0;
        assert_eq!(listening_note(&totals), "measured");

        totals.plays = 0;
        assert_eq!(listening_note(&totals), "nothing played yet");
    }

    #[test]
    fn one_unmeasured_play_is_singular() {
        let totals = Totals {
            plays: 3,
            unmeasured_plays: 1,
            ..Totals::default()
        };
        let note = listening_note(&totals);
        assert!(note.ends_with("1 earlier play not timed"), "{note}");
    }
}
