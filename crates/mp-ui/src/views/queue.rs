//! The play queue.
//!
//! Shows what is coming next *in the order it will actually play*. That is the
//! whole reason the engine publishes its play order rather than the panel
//! deriving one: with shuffle on, the order is a permutation the engine owns,
//! and it rebuilds itself whenever a shuffled queue wraps.
//!
//! Rows are drawn with `show_rows`, so a queue of twenty thousand costs the
//! same to display as a queue of twenty.

use std::time::Duration;

use egui::{Sense, TextStyle, Ui};
use mp_core::library::model::{AlbumId, ArtistId};

use crate::theme::{Theme, col, col_alpha};
use crate::widgets;
use crate::widgets::icons::Icon;

/// How much of the panel a single row takes, relative to a library row.
///
/// Slightly tighter than the main list: this is a glanceable running order,
/// not somewhere you browse, so more of it on screen is worth more than the
/// breathing room.
const ROW_SCALE: f32 = 0.82;

/// One queue entry, already resolved for display.
///
/// Resolved by the caller rather than here because turning a path into a
/// library track is a database lookup, and the panel is redrawn far more often
/// than the queue actually changes.
#[derive(Debug, Clone)]
pub struct Row {
    /// The engine-side index, which is what jumping and removing take.
    pub index: usize,
    pub title: String,
    /// Kept apart rather than pre-joined into one subtitle: each half is its
    /// own navigation target, so each needs its own rectangle to be hit.
    pub artist: String,
    pub album: Option<String>,
    pub artist_id: Option<ArtistId>,
    pub album_id: Option<AlbumId>,
    pub duration: Option<Duration>,
}

/// The separator between the two halves of the second line.
const SEPARATOR: &str = " — ";

impl Row {
    /// Whether there is a second line to draw at all.
    fn has_subtitle(&self) -> bool {
        !self.artist.is_empty() || self.album.is_some()
    }
}

/// Where the in-progress drag is remembered between frames.
///
/// egui memory rather than a field on the caller: the panel is drawn from a
/// plain function, and a drag is transient interaction state that means
/// nothing once the pointer is released.
fn drag_id() -> egui::Id {
    egui::Id::new("queue-drag-from")
}

fn dragging_from(ctx: &egui::Context) -> Option<usize> {
    ctx.data(|d| d.get_temp::<usize>(drag_id()))
}

fn set_dragging(ctx: &egui::Context, from: Option<usize>) {
    ctx.data_mut(|d| match from {
        Some(position) => {
            d.insert_temp(drag_id(), position);
        }
        None => {
            d.remove_temp::<usize>(drag_id());
        }
    });
}

/// Which half of the second line the pointer is over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Link {
    Artist,
    Album,
}

/// What the user did in the panel this frame.
#[derive(Debug, Default, Clone)]
pub struct Outcome {
    /// Jump to this engine-side index.
    pub jump: Option<usize>,
    /// Drop this engine-side index from the queue.
    pub remove: Option<usize>,
    /// Empty the queue.
    pub clear: bool,
    /// Close the panel.
    pub close: bool,
    /// Open this artist in the library.
    pub open_artist: Option<(ArtistId, String)>,
    /// Open this album in the library, as `(id, title, artist)`.
    pub open_album: Option<(AlbumId, String, String)>,
    /// Move an entry within the play order, as `(from, to)`.
    ///
    /// Positions within the displayed order, not engine indices: reordering is
    /// done to the list on screen, and the engine owns the permutation that
    /// list came from.
    pub reorder: Option<(usize, usize)>,
}

/// Draw the panel.
///
/// `cursor` is the position *within `rows`* of the track playing now, not an
/// engine index: the panel renders a list, and what it needs to know is which
/// line of that list to mark.
pub fn show(
    ui: &mut Ui,
    theme: &Theme,
    rows: &[Row],
    cursor: Option<usize>,
    single_click: bool,
) -> Outcome {
    let mut outcome = Outcome::default();
    let m = theme.metrics;

    header(ui, theme, rows, cursor, &mut outcome);
    widgets::separator(ui, theme);

    if rows.is_empty() {
        empty(ui, theme);
        return outcome;
    }

    let row_height = m.row_height * ROW_SCALE;
    let dragging = dragging_from(ui.ctx());

    // Read once, before the rows: while a drag is active egui gives the
    // pointer to the dragged widget, so no other row will report a hover and
    // the drop target has to be worked out from the raw position.
    let pointer = ui.input(|i| i.pointer.interact_pos());
    let released = ui.input(|i| i.pointer.any_released());
    let mut drop_slot: Option<usize> = None;

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show_rows(ui, row_height, rows.len(), |ui, range| {
            for position in range {
                let Some(row) = rows.get(position) else {
                    continue;
                };

                let played = cursor.is_some_and(|cursor| position < cursor);
                let is_current = cursor == Some(position);

                let hit = entry_row(
                    ui,
                    theme,
                    row,
                    position,
                    is_current,
                    played,
                    row_height,
                    dragging == Some(position),
                );

                if hit.response.drag_started() {
                    set_dragging(ui.ctx(), Some(position));
                }

                if dragging.is_some()
                    && let Some(at) = pointer
                    && hit.rect.y_range().contains(at.y)
                {
                    // Top half drops above this row, bottom half below it.
                    let slot = if at.y < hit.rect.center().y {
                        position
                    } else {
                        position + 1
                    };
                    drop_slot = Some(slot);

                    let y = if slot == position {
                        hit.rect.top()
                    } else {
                        hit.rect.bottom()
                    };
                    ui.painter().hline(
                        hit.rect.x_range(),
                        y,
                        egui::Stroke::new(2.0, col(theme.palette.accent)),
                    );
                }

                // A drag is not a click, and while one is running the row is
                // a drop target rather than a link or a track to play.
                if dragging.is_some() {
                    continue;
                }

                match hit.link {
                    // A click on a name goes to that name, on the first click
                    // whatever the activation setting is: following a link is
                    // not the same gesture as choosing a track.
                    Some(link) if hit.response.clicked() => match link {
                        Link::Artist => {
                            if let Some(id) = row.artist_id {
                                outcome.open_artist = Some((id, row.artist.clone()));
                            }
                        }
                        Link::Album => {
                            if let (Some(id), Some(album)) = (row.album_id, row.album.as_ref()) {
                                outcome.open_album = Some((id, album.clone(), row.artist.clone()));
                            }
                        }
                    },
                    // Anywhere else on the row is still "play this".
                    None if widgets::row_activated(&hit.response, single_click) => {
                        outcome.jump = Some(row.index);
                    }
                    _ => {}
                }

                hit.response.context_menu(|ui| {
                    if ui.button("Play this next").clicked() {
                        outcome.jump = Some(row.index);
                        ui.close();
                    }

                    // Removing what is playing is refused by the engine, so it
                    // is not offered here either — an item that does nothing
                    // teaches the user the menu is unreliable.
                    if !is_current && ui.button("Remove from queue").clicked() {
                        outcome.remove = Some(row.index);
                        ui.close();
                    }
                });
            }
        });

    if let Some(from) = dragging {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);

        if released {
            // `drop_slot` counts insertion points, so a slot below the lifted
            // row is one place further along than the index it will occupy
            // once that row is out of the list.
            if let Some(slot) = drop_slot {
                let to = if from < slot {
                    slot.saturating_sub(1)
                } else {
                    slot
                };
                if to != from {
                    outcome.reorder = Some((from, to));
                }
            }
            set_dragging(ui.ctx(), None);
        }

        // The insertion line has to follow the pointer between rows.
        ui.ctx().request_repaint();
    }

    outcome
}

/// Title, what is left to play, and the controls for the panel itself.
fn header(ui: &mut Ui, theme: &Theme, rows: &[Row], cursor: Option<usize>, outcome: &mut Outcome) {
    let m = theme.metrics;
    let p = &theme.palette;

    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("Queue")
                .text_style(TextStyle::Name("title".into()))
                .color(col(p.text_primary)),
        );

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if widgets::icon_button(ui, theme, Icon::Collapse, m.space(2.5), false)
                .on_hover_text("Hide the queue")
                .clicked()
            {
                outcome.close = true;
            }
        });
    });

    ui.add_space(m.space(0.5));

    // What is *left*, not the whole queue: once something is playing, the
    // length of the part already behind you is not a useful number.
    let upcoming = cursor.map_or(rows.len(), |cursor| rows.len().saturating_sub(cursor + 1));
    let remaining: f64 = rows
        .iter()
        .skip(cursor.map_or(0, |cursor| cursor + 1))
        .filter_map(|row| row.duration)
        .map(|d| d.as_secs_f64())
        .sum();

    let summary = if rows.is_empty() {
        "Nothing queued".to_owned()
    } else if remaining > 0.0 {
        format!(
            "{upcoming} {} · {} left",
            plural(upcoming),
            widgets::format_duration(remaining)
        )
    } else {
        format!("{upcoming} {}", plural(upcoming))
    };

    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(summary)
                .text_style(TextStyle::Name("caption".into()))
                .color(col(p.text_muted)),
        );

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if !rows.is_empty() && ui.button("Clear").clicked() {
                outcome.clear = true;
            }
        });
    });

    ui.add_space(m.space(1.0));
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "track" } else { "tracks" }
}

/// Shown when nothing is queued at all.
fn empty(ui: &mut Ui, theme: &Theme) {
    let m = theme.metrics;
    ui.add_space(m.space(3.0));
    ui.vertical_centered(|ui| {
        ui.label(
            egui::RichText::new("Play something and it will show up here.")
                .text_style(TextStyle::Name("caption".into()))
                .color(col(theme.palette.text_muted)),
        );
    });
}

/// The response for one row, so the caller can hang a menu off it.
struct Hit {
    response: egui::Response,
    rect: egui::Rect,
    /// The half of the second line under the pointer, if any.
    ///
    /// Returned rather than acted on here so the caller keeps every decision
    /// about what a click means in one place.
    link: Option<Link>,
}

#[allow(clippy::too_many_arguments)]
fn entry_row(
    ui: &mut Ui,
    theme: &Theme,
    row: &Row,
    position: usize,
    is_current: bool,
    played: bool,
    height: f32,
    lifted: bool,
) -> Hit {
    let m = theme.metrics;
    let p = &theme.palette;

    let width = ui.available_width();
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(width, height), Sense::click_and_drag());

    // The row being carried is dimmed and left in place, so the list does not
    // reflow under the pointer while the drop target is still being chosen.
    if lifted {
        ui.painter().rect_filled(
            rect,
            egui::CornerRadius::same(m.radius_small),
            col_alpha(p.accent, 0.10),
        );
    }

    let hovered = response.hovered();

    if is_current {
        ui.painter().rect_filled(
            rect,
            egui::CornerRadius::same(m.radius_small),
            col(p.bg_active),
        );
    } else if hovered {
        ui.painter().rect_filled(
            rect,
            egui::CornerRadius::same(m.radius_small),
            col(p.bg_hover),
        );
    }

    let pad = m.space(1.0);
    let mut x = rect.left() + pad;

    // The position marker. A number for everything still to come, and the
    // accent bar for whatever is playing, so the eye finds "you are here"
    // without reading a single digit.
    let marker_width = m.space(3.0);
    if is_current {
        let bar = egui::Rect::from_min_size(
            egui::pos2(x + marker_width * 0.35, rect.center().y - height * 0.22),
            egui::vec2(3.0, height * 0.44),
        );
        ui.painter()
            .rect_filled(bar, egui::CornerRadius::same(2), col(p.accent));
    } else {
        ui.painter().text(
            egui::pos2(x + marker_width * 0.5, rect.center().y),
            egui::Align2::CENTER_CENTER,
            format!("{}", position + 1),
            ui.style()
                .text_styles
                .get(&TextStyle::Name("caption".into()))
                .cloned()
                .unwrap_or(egui::FontId::proportional(11.0)),
            col(p.text_muted),
        );
    }
    x += marker_width + m.space(0.5);

    // A track already behind the cursor is dimmed rather than hidden: the
    // queue is also a record of where this listen has been.
    let title_colour = if is_current {
        col(p.accent)
    } else if played {
        col_alpha(p.text_secondary, 0.55)
    } else {
        col(p.text_primary)
    };

    let duration_width = m.space(5.0);
    let text_width = (rect.right() - pad - duration_width - x).max(0.0);

    let title_font = ui
        .style()
        .text_styles
        .get(&TextStyle::Body)
        .cloned()
        .unwrap_or(egui::FontId::proportional(14.0));
    let caption_font = ui
        .style()
        .text_styles
        .get(&TextStyle::Name("caption".into()))
        .cloned()
        .unwrap_or(egui::FontId::proportional(11.0));

    let (title_y, subtitle_y) =
        widgets::stacked_lines(rect, &title_font, &caption_font, m.space(0.25));

    // A row with nothing on its second line centres the first, rather than
    // sitting it high above empty space.
    let title_y = if row.has_subtitle() {
        title_y
    } else {
        rect.center().y
    };

    ui.painter().text(
        egui::pos2(x, title_y),
        egui::Align2::LEFT_CENTER,
        widgets::elide(ui, &row.title, &title_font, text_width),
        title_font,
        title_colour,
    );

    let mut link = None;

    if row.has_subtitle() {
        // Each half is painted separately so it can own a rectangle. Laying
        // them out by measuring as we go is what makes the hit target line up
        // with the glyphs rather than the whole row.
        let dim = if played { 0.55 } else { 1.0 };
        let pointer = response.hover_pos();
        let mut cursor_x = x;
        let limit = x + text_width;

        let mut segment = |ui: &Ui, text: &str, target: Option<Link>, painter: &egui::Painter| {
            if text.is_empty() || cursor_x >= limit {
                return;
            }

            let shown = widgets::elide(ui, text, &caption_font, limit - cursor_x);
            let galley = painter.layout_no_wrap(shown, caption_font.clone(), col(p.text_muted));
            let size = galley.size();
            let rect =
                egui::Rect::from_min_size(egui::pos2(cursor_x, subtitle_y - size.y / 2.0), size);

            let over = target.is_some() && pointer.is_some_and(|at| rect.contains(at));
            if over {
                link = target;
            }

            let colour = if over {
                col(p.text_primary)
            } else {
                col_alpha(p.text_muted, dim)
            };
            painter.galley(rect.min, galley, colour);

            if over {
                // An underline rather than a colour change alone: on a muted
                // caption the colour shift is easy to miss, and the underline
                // is what says "this goes somewhere".
                painter.hline(
                    rect.x_range(),
                    rect.bottom() - 1.0,
                    egui::Stroke::new(1.0, colour),
                );
            }

            cursor_x += size.x;
        };

        let painter = ui.painter().clone();
        segment(
            ui,
            &row.artist,
            row.artist_id.map(|_| Link::Artist),
            &painter,
        );
        if let Some(album) = &row.album {
            segment(ui, SEPARATOR, None, &painter);
            segment(ui, album, row.album_id.map(|_| Link::Album), &painter);
        }
    }

    if let Some(duration) = row.duration {
        ui.painter().text(
            egui::pos2(rect.right() - pad, rect.center().y),
            egui::Align2::RIGHT_CENTER,
            widgets::format_duration(duration.as_secs_f64()),
            caption_font,
            col_alpha(p.text_muted, if played { 0.55 } else { 1.0 }),
        );
    }

    if link.is_some() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    Hit {
        response,
        rect,
        link,
    }
}
