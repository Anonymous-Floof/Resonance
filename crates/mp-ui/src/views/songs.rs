//! The track list.
//!
//! Rows are drawn with `show_rows`, so only what is on screen is built no
//! matter how long the list is. That is not premature optimisation: it is the
//! difference between a list that scrolls smoothly at 20k tracks and one that
//! stalls, and retrofitting it later would mean rewriting this file.

use std::path::Path;

use egui::{Rect, Sense, TextStyle, Ui, Vec2};
use mp_core::library::{ArtSize, Track};

use crate::artwork::Artwork;
use crate::theme::{Theme, col, col_alpha};
use crate::widgets;

/// Shown where a title is cut short.
const ELLIPSIS: char = '…';

/// How the list behaves, as opposed to what is in it.
///
/// Grouped rather than passed as loose booleans: the two already read
/// identically at the call site, and a third would have made transposing them
/// a silent bug.
/// The cover cache and the directory it reads from.
///
/// Always used together, so they travel together rather than as two parameters
/// every list has to remember to keep in step.
pub struct Covers<'a> {
    pub textures: &'a mut Artwork,
    pub cache: &'a mp_core::library::ArtCache,
}

/// How the list behaves, as opposed to what is in it.
///
/// Grouped rather than passed as loose booleans: the two already read
/// identically at the call site, and a third would have made transposing them
/// a silent bug.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Offer "Edit tags…" on the row's context menu.
    pub tag_editing: bool,
    /// One click starts a track, rather than two.
    pub single_click: bool,
}

/// What the user did in the list this frame.
#[derive(Debug, Default, Clone)]
pub struct Outcome {
    /// A row was activated: start playing this index of the visible list.
    pub play: Option<usize>,
    /// Queue this index of the visible list to play after the current track.
    pub play_next: Option<usize>,
    /// Append this index of the visible list to the queue.
    pub enqueue: Option<usize>,
    /// The artist on a row was clicked.
    pub open_artist: Option<i64>,
    /// The album on a row was clicked.
    pub open_album: Option<i64>,
    /// The "add folder" affordance in the empty state was used.
    pub add_folder: bool,
    /// Open the tag editor on this track. Only offered when tag editing is
    /// enabled in Settings, so the menu item cannot appear on a read-only
    /// install and then refuse to work.
    pub edit_tags: Option<i64>,
}

/// Draw the list.
///
/// `current` is matched by path rather than by index because the highlight has
/// to survive re-sorting and filtering — the playing track keeps its highlight
/// when you switch from title order to artist order.
pub fn show(
    ui: &mut Ui,
    theme: &Theme,
    covers: &mut Covers<'_>,
    tracks: &[Track],
    current: Option<&Path>,
    options: Options,
) -> Outcome {
    let mut outcome = Outcome::default();
    let m = theme.metrics;
    let row_height = m.row_height;

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show_rows(ui, row_height, tracks.len(), |ui, range| {
            for index in range {
                let Some(track) = tracks.get(index) else {
                    continue;
                };
                let is_current = current.is_some_and(|path| path == track.path);

                let hit = track_row(ui, theme, covers, track, is_current, row_height, options);

                if widgets::row_activated(&hit.response, options.single_click) {
                    outcome.play = Some(index);
                }
                // Right-click, because queueing and editing are both
                // deliberate acts that do not belong on a row's primary click.
                hit.response.context_menu(|ui| {
                    if ui.button("Play next").clicked() {
                        outcome.play_next = Some(index);
                        ui.close();
                    }
                    if ui.button("Add to queue").clicked() {
                        outcome.enqueue = Some(index);
                        ui.close();
                    }

                    // Only offered when tag editing is on in Settings, so the
                    // item cannot appear on a read-only install and then
                    // refuse to work.
                    if options.tag_editing {
                        ui.separator();
                        if ui.button("Edit tags…").clicked() {
                            outcome.edit_tags = Some(track.id);
                            ui.close();
                        }
                    }
                });

                if hit.artist_clicked {
                    outcome.open_artist = track.artist_id;
                }
                if hit.album_clicked {
                    outcome.open_album = track.album_id;
                }
            }
        });

    outcome
}

/// What a row reported back.
struct RowHit {
    response: egui::Response,
    artist_clicked: bool,
    album_clicked: bool,
}

/// A single row: artwork, title, artist and album, duration.
fn track_row(
    ui: &mut Ui,
    theme: &Theme,
    covers: &mut Covers<'_>,
    track: &Track,
    is_current: bool,
    height: f32,
    options: Options,
) -> RowHit {
    let m = theme.metrics;
    let p = theme.palette;

    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::click());

    let mut hit = RowHit {
        response,
        artist_clicked: false,
        album_clicked: false,
    };

    if !ui.is_rect_visible(rect) {
        return hit;
    }

    let hovered = ui.ctx().animate_bool_with_time(
        hit.response.id.with("hover"),
        hit.response.hovered(),
        0.12,
    );

    let radius = egui::CornerRadius::same(m.radius_small);
    if is_current {
        ui.painter()
            .rect_filled(rect, radius, col_alpha(p.accent, 0.16));
    } else if hovered > 0.0 {
        ui.painter()
            .rect_filled(rect, radius, col_alpha(p.bg_hover, hovered));
    }

    let pad = m.space(1.25);
    let thumb = Vec2::splat(m.thumb_size);
    let thumb_rect = Rect::from_min_size(
        egui::Pos2::new(rect.left() + pad, rect.center().y - thumb.y * 0.5),
        thumb,
    );

    draw_thumbnail(ui, theme, covers, track, is_current, thumb_rect);

    // Duration is right-aligned in its own column so the numbers line up down
    // the list instead of ragging against variable-length titles.
    let duration_width = m.space(5.0);
    let duration = track.duration.map_or_else(
        || "--:--".to_owned(),
        |d| widgets::format_duration(d.as_secs_f64()),
    );

    let text_left = thumb_rect.right() + pad;
    let text_width = (rect.right() - text_left - pad - duration_width).max(0.0);

    let title_color = if is_current {
        col(p.accent)
    } else {
        col(p.text_primary)
    };

    let title = truncated(ui, &track.title, text_width, TextStyle::Body, title_color);
    let title_pos = egui::Pos2::new(
        text_left,
        rect.center().y - m.space(0.9) - title.size().y * 0.5,
    );

    // The subtitle is two separately clickable spans, so an artist or album
    // name in a row is a way to get to that artist or album.
    let caption = TextStyle::Name("caption".into());
    let subtitle_y = rect.center().y + m.space(0.9);

    let artist = truncated(
        ui,
        &track.artist,
        text_width,
        caption.clone(),
        col(p.text_muted),
    );
    let artist_rect = Rect::from_min_size(
        egui::Pos2::new(text_left, subtitle_y - artist.size().y * 0.5),
        artist.size(),
    );
    let artist_hover = ui.rect_contains_pointer(artist_rect) && track.artist_id.is_some();

    let separator_width = m.space(1.0);
    let album_left = artist_rect.right() + separator_width;
    let album_width = (text_left + text_width - album_left).max(0.0);
    let show_album = track.album_id.is_some() && album_width > m.space(4.0);

    let album = show_album.then(|| {
        truncated(
            ui,
            &track.album,
            album_width,
            caption.clone(),
            col(p.text_muted),
        )
    });
    let album_rect = album.as_ref().map(|galley| {
        Rect::from_min_size(
            egui::Pos2::new(album_left, subtitle_y - galley.size().y * 0.5),
            galley.size(),
        )
    });
    let album_hover = album_rect.is_some_and(|rect| ui.rect_contains_pointer(rect));

    let painter = ui.painter();
    painter.galley(title_pos, title, title_color);

    let artist_color = if artist_hover {
        col(p.text_primary)
    } else {
        col(p.text_muted)
    };
    painter.galley(artist_rect.min, artist, artist_color);

    if let (Some(album), Some(album_rect)) = (album, album_rect) {
        painter.text(
            egui::Pos2::new(artist_rect.right() + separator_width * 0.5, subtitle_y),
            egui::Align2::CENTER_CENTER,
            "·",
            caption.resolve(ui.style()),
            col_alpha(p.text_muted, 0.7),
        );
        let album_color = if album_hover {
            col(p.text_primary)
        } else {
            col(p.text_muted)
        };
        painter.galley(album_rect.min, album, album_color);
    }

    painter.text(
        egui::Pos2::new(rect.right() - pad, rect.center().y),
        egui::Align2::RIGHT_CENTER,
        duration,
        caption.resolve(ui.style()),
        col(p.text_muted),
    );

    // A row click that landed on the artist or album span means "go there";
    // anywhere else on the row is a plain selection.
    if hit.response.clicked() {
        if artist_hover {
            hit.artist_clicked = true;
        } else if album_hover {
            hit.album_clicked = true;
        }
    }

    if artist_hover || album_hover {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    hit.response = hit
        .response
        .on_hover_text(widgets::activate_hint(options.single_click));
    hit
}

/// Row artwork, or a placeholder that keeps the row's shape.
fn draw_thumbnail(
    ui: &mut Ui,
    theme: &Theme,
    covers: &mut Covers<'_>,
    track: &Track,
    is_current: bool,
    rect: Rect,
) {
    let m = theme.metrics;
    let p = theme.palette;
    let radius = egui::CornerRadius::same(m.radius_small);

    ui.painter().rect_filled(rect, radius, col(p.bg_elevated));

    let texture = track.art_id.as_deref().and_then(|id| {
        covers
            .textures
            .get(ui.ctx(), covers.cache, id, ArtSize::Thumb)
    });

    if let Some(texture) = texture {
        egui::Image::from_texture(&texture)
            .corner_radius(radius)
            .paint_at(ui, rect);

        // The playing track gets a scrim and a play mark over its cover, so it
        // stays identifiable at a glance in a list of artwork.
        if is_current {
            ui.painter()
                .rect_filled(rect, radius, col_alpha(p.bg_base, 0.55));
            crate::widgets::icons::draw(
                ui.painter(),
                crate::widgets::icons::Icon::Play,
                rect.shrink(m.thumb_size * 0.3),
                col(p.accent),
                1.5,
            );
        }
        return;
    }

    crate::widgets::icons::draw(
        ui.painter(),
        if is_current {
            crate::widgets::icons::Icon::Play
        } else {
            crate::widgets::icons::Icon::Songs
        },
        rect.shrink(m.thumb_size * 0.3),
        if is_current {
            col(p.accent)
        } else {
            col_alpha(p.text_muted, 0.5)
        },
        1.5,
    );
}

/// Lay out one line of text, truncated with an ellipsis if it will not fit.
///
/// Long YouTube-style filenames are the norm in this collection, so rows must
/// clip cleanly rather than spilling past the row.
pub fn truncated(
    ui: &Ui,
    text: &str,
    max_width: f32,
    style: TextStyle,
    color: egui::Color32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::single_section(
        text.to_owned(),
        egui::TextFormat {
            font_id: style.resolve(ui.style()),
            color,
            ..Default::default()
        },
    );

    job.wrap = egui::text::TextWrapping {
        max_width,
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some(ELLIPSIS),
    };

    ui.painter().layout_job(job)
}

/// The line under the "Songs" heading: what is being shown, and any caveats.
pub fn detail_line(shown: usize, total: u32, unplayable: u32, untagged: u32) -> String {
    let mut parts = vec![if shown == 1 {
        "1 track".to_owned()
    } else {
        format!("{shown} tracks")
    }];

    if shown as u32 != total {
        parts.push(format!("of {total}"));
    }
    if untagged > 0 {
        parts.push(format!("{untagged} named from the filename"));
    }
    if unplayable > 0 {
        parts.push(format!("{unplayable} unplayable"));
    }

    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_detail_line_only_mentions_what_applies() {
        assert_eq!(detail_line(12, 12, 0, 0), "12 tracks");
        assert_eq!(detail_line(1, 1, 0, 0), "1 track");
    }

    /// A filtered list has to say so, or the count looks like the library
    /// silently lost tracks.
    #[test]
    fn a_filtered_list_says_what_it_is_filtered_from() {
        assert_eq!(detail_line(3, 485, 0, 0), "3 tracks · of 485");
    }

    #[test]
    fn caveats_are_reported_rather_than_hidden() {
        let line = detail_line(485, 485, 1, 139);
        assert!(line.contains("139 named from the filename"), "{line}");
        assert!(line.contains("1 unplayable"), "{line}");
    }
}
