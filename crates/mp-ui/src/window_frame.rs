//! Custom window chrome.
//!
//! The window is created undecorated, so this draws the title bar and provides
//! the behaviour the OS would otherwise give us: drag to move, double-click to
//! maximise, and resize handles on every edge and corner.
//!
//! The reason for taking this on: Windows paints the system title bar from the
//! *system* accent colour when "show accent colour on title bars" is enabled,
//! and on Windows 10 there is no API to override it (`DWMWA_CAPTION_COLOR` is
//! Windows 11 only). A themed shell under a magenta title bar looks broken, and
//! the only reliable fix on every Windows version is to draw our own.
//!
//! Known gap: Windows 11 snap layouts (the flyout when hovering Maximise) need
//! a custom `WM_NCHITTEST` handler returning `HTMAXBUTTON`, which winit does not
//! expose. Dragging to a screen edge still snaps normally, because
//! [`ViewportCommand::StartDrag`] performs a real window drag.

use egui::{
    Align, Align2, CornerRadius, Id, Layout, Rect, Sense, TextStyle, Ui, Vec2, ViewportCommand,
};

use crate::theme::{Theme, col, col_alpha};
use crate::widgets::icons::{self, Icon};

/// Thickness of the invisible grab band along each window edge.
const RESIZE_BAND: f32 = 6.0;
/// Length of the corner zones, which resize on both axes.
const RESIZE_CORNER: f32 = 14.0;

/// Windows caption buttons are 46x32 at 100% scale; matching that keeps the
/// muscle memory intact even though the bar is ours.
const CAPTION_BUTTON: Vec2 = Vec2::new(46.0, 32.0);

/// Height of the title bar.
pub const TITLE_BAR_HEIGHT: f32 = 34.0;

/// Draw the title bar. Returns true if the user asked to close the window.
pub fn title_bar(ui: &mut Ui, theme: &Theme, title: &str) {
    let m = theme.metrics;
    let p = theme.palette;

    egui::Panel::top("title_bar")
        .exact_size(TITLE_BAR_HEIGHT)
        .resizable(false)
        .frame(egui::Frame::new().fill(col(p.bg_surface)))
        .show(ui, |ui| {
            let bar = ui.max_rect();

            // The whole bar is a drag handle except where a button sits, so
            // interact with it first and let the buttons draw on top.
            let drag = ui.interact(bar, Id::new("title_bar_drag"), Sense::click_and_drag());

            if drag.is_pointer_button_down_on() {
                ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
            }
            if drag.double_clicked() {
                toggle_maximised(ui.ctx());
            }

            ui.scope_builder(
                egui::UiBuilder::new()
                    .max_rect(bar)
                    .layout(Layout::left_to_right(Align::Center)),
                |ui| {
                    ui.add_space(m.space(1.75));

                    // Accent dot standing in for a logo.
                    let (dot, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                    ui.painter().circle_filled(dot.center(), 4.0, col(p.accent));

                    ui.add_space(m.space(1.0));
                    ui.painter().text(
                        egui::Pos2::new(ui.cursor().left(), bar.center().y),
                        Align2::LEFT_CENTER,
                        title,
                        TextStyle::Name("nav".into()).resolve(ui.style()),
                        col(p.text_secondary),
                    );

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        caption_buttons(ui, theme);
                    });
                },
            );
        });
}

/// Minimise / maximise / close, in Windows order.
fn caption_buttons(ui: &mut Ui, theme: &Theme) {
    let p = theme.palette;
    let maximised = is_maximised(ui.ctx());

    // Close is rightmost, so in a right-to-left layout it comes first.
    if caption_button(ui, theme, Icon::Close, true).clicked() {
        ui.ctx().send_viewport_cmd(ViewportCommand::Close);
    }

    let restore_icon = if maximised {
        Icon::Restore
    } else {
        Icon::Maximize
    };
    if caption_button(ui, theme, restore_icon, false).clicked() {
        toggle_maximised(ui.ctx());
    }

    if caption_button(ui, theme, Icon::Minimize, false).clicked() {
        ui.ctx().send_viewport_cmd(ViewportCommand::Minimized(true));
    }

    let _ = p;
}

/// One caption button. `danger` turns the hover fill red, as Windows does for
/// Close, so the destructive one is not a surprise.
fn caption_button(ui: &mut Ui, theme: &Theme, icon: Icon, danger: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(CAPTION_BUTTON, Sense::click());

    if ui.is_rect_visible(rect) {
        let hover =
            ui.ctx()
                .animate_bool_with_time(response.id.with("hover"), response.hovered(), 0.08);
        let p = theme.palette;

        if hover > 0.0 {
            let fill = if danger {
                col_alpha(p.error, hover)
            } else {
                col_alpha(p.bg_hover, hover)
            };
            ui.painter().rect_filled(rect, CornerRadius::ZERO, fill);
        }

        // On a red fill the icon must flip to the readable foreground.
        let tint = if danger && hover > 0.5 {
            p.error.readable_foreground()
        } else {
            p.text_secondary.mix(p.text_primary, hover)
        };

        icons::draw(ui.painter(), icon, rect, col(tint), 1.2);
    }

    response.on_hover_text(icon.label())
}

/// Invisible resize zones around the window edge.
///
/// Drawn as foreground areas so they sit above the panels; without this the
/// nav rail and player bar would swallow the edge pixels. Skipped entirely
/// while maximised, where resizing makes no sense.
pub fn resize_handles(ctx: &egui::Context) {
    if is_maximised(ctx) {
        return;
    }

    let screen = ctx.viewport_rect();

    // (id, rect, direction, cursor)
    let zones = [
        (
            "rz_n",
            Rect::from_min_max(
                egui::Pos2::new(screen.left() + RESIZE_CORNER, screen.top()),
                egui::Pos2::new(screen.right() - RESIZE_CORNER, screen.top() + RESIZE_BAND),
            ),
            egui::ResizeDirection::North,
            egui::CursorIcon::ResizeNorth,
        ),
        (
            "rz_s",
            Rect::from_min_max(
                egui::Pos2::new(screen.left() + RESIZE_CORNER, screen.bottom() - RESIZE_BAND),
                egui::Pos2::new(screen.right() - RESIZE_CORNER, screen.bottom()),
            ),
            egui::ResizeDirection::South,
            egui::CursorIcon::ResizeSouth,
        ),
        (
            "rz_w",
            Rect::from_min_max(
                egui::Pos2::new(screen.left(), screen.top() + RESIZE_CORNER),
                egui::Pos2::new(screen.left() + RESIZE_BAND, screen.bottom() - RESIZE_CORNER),
            ),
            egui::ResizeDirection::West,
            egui::CursorIcon::ResizeWest,
        ),
        (
            "rz_e",
            Rect::from_min_max(
                egui::Pos2::new(screen.right() - RESIZE_BAND, screen.top() + RESIZE_CORNER),
                egui::Pos2::new(screen.right(), screen.bottom() - RESIZE_CORNER),
            ),
            egui::ResizeDirection::East,
            egui::CursorIcon::ResizeEast,
        ),
        (
            "rz_nw",
            Rect::from_min_size(screen.left_top(), Vec2::splat(RESIZE_CORNER)),
            egui::ResizeDirection::NorthWest,
            egui::CursorIcon::ResizeNorthWest,
        ),
        (
            "rz_ne",
            Rect::from_min_size(
                egui::Pos2::new(screen.right() - RESIZE_CORNER, screen.top()),
                Vec2::splat(RESIZE_CORNER),
            ),
            egui::ResizeDirection::NorthEast,
            egui::CursorIcon::ResizeNorthEast,
        ),
        (
            "rz_sw",
            Rect::from_min_size(
                egui::Pos2::new(screen.left(), screen.bottom() - RESIZE_CORNER),
                Vec2::splat(RESIZE_CORNER),
            ),
            egui::ResizeDirection::SouthWest,
            egui::CursorIcon::ResizeSouthWest,
        ),
        (
            "rz_se",
            Rect::from_min_size(
                egui::Pos2::new(
                    screen.right() - RESIZE_CORNER,
                    screen.bottom() - RESIZE_CORNER,
                ),
                Vec2::splat(RESIZE_CORNER),
            ),
            egui::ResizeDirection::SouthEast,
            egui::CursorIcon::ResizeSouthEast,
        ),
    ];

    for (id, rect, direction, cursor) in zones {
        egui::Area::new(Id::new(id))
            .order(egui::Order::Foreground)
            .fixed_pos(rect.min)
            // Without this an oversized area is nudged back onto the screen
            // instead of staying where it was put.
            .constrain(false)
            .show(ctx, |ui| {
                // Allocate by *size*, not by absolute rect. The area is already
                // positioned at `rect.min`, so allocating the absolute rect
                // again extends the content a second time from that origin. The
                // resulting oversized area then gets constrained back across the
                // whole window, and - being in the foreground - it swallows the
                // input of every panel underneath. That made the entire UI
                // unclickable except the few widgets it happened not to cover.
                let (_, response) = ui.allocate_exact_size(rect.size(), Sense::drag());

                if response.hovered() || response.is_pointer_button_down_on() {
                    ui.ctx().set_cursor_icon(cursor);
                }
                if response.drag_started() {
                    ui.ctx()
                        .send_viewport_cmd(ViewportCommand::BeginResize(direction));
                }
            });
    }
}

/// A one-pixel outline, which an undecorated window otherwise lacks. Without
/// it the app bleeds into whatever is behind it on a dark desktop.
pub fn window_border(ctx: &egui::Context, theme: &Theme) {
    if is_maximised(ctx) {
        return;
    }

    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        Id::new("window_border"),
    ));

    painter.rect_stroke(
        ctx.viewport_rect().shrink(0.5),
        CornerRadius::ZERO,
        egui::Stroke::new(1.0, col(theme.palette.border_strong)),
        egui::StrokeKind::Inside,
    );
}

fn is_maximised(ctx: &egui::Context) -> bool {
    ctx.input(|i| i.viewport().maximized.unwrap_or(false))
}

fn toggle_maximised(ctx: &egui::Context) {
    let now = is_maximised(ctx);
    ctx.send_viewport_cmd(ViewportCommand::Maximized(!now));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bands must be thin enough not to eat ordinary clicks near an edge,
    /// but thick enough to grab. Checked at compile time so a bad edit fails
    /// the build rather than only the test run.
    #[test]
    fn resize_bands_are_grabbable_but_unobtrusive() {
        const {
            assert!(RESIZE_BAND >= 4.0, "too thin to hit reliably");
            assert!(RESIZE_BAND <= 8.0, "would swallow real clicks");
            assert!(
                RESIZE_CORNER > RESIZE_BAND,
                "corners must be larger than edges to be reachable"
            );
        }
    }

    #[test]
    fn caption_buttons_match_the_windows_metric() {
        // Matching the OS metric is the point; guard against a stray edit.
        assert_eq!(CAPTION_BUTTON, Vec2::new(46.0, 32.0));
        const {
            assert!(
                TITLE_BAR_HEIGHT >= CAPTION_BUTTON.y,
                "title bar must fit its buttons"
            );
        }
    }

    #[test]
    fn edge_zones_do_not_overlap_the_corners() {
        // Corner zones win, so the edge spans must start after them.
        let screen = Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(800.0, 600.0));

        let north = Rect::from_min_max(
            egui::Pos2::new(screen.left() + RESIZE_CORNER, screen.top()),
            egui::Pos2::new(screen.right() - RESIZE_CORNER, screen.top() + RESIZE_BAND),
        );
        let north_west = Rect::from_min_size(screen.left_top(), Vec2::splat(RESIZE_CORNER));

        assert!(
            north.left() >= north_west.right(),
            "north edge overlaps the north-west corner"
        );
        assert!(north.width() > 0.0, "edge zone collapsed");
    }
}
