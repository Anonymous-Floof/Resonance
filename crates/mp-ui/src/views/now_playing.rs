//! The full-screen now-playing view.
//!
//! This is the one screen in the app that is not about finding music — it is
//! about the track you already chose. So it drops every affordance that is not
//! the music: no nav rail, no lists, no player bar. Large artwork, the words if
//! there are any, and enough transport to change your mind.
//!
//! The background is built from the cover's own palette, which is why cover
//! palettes are extracted at scan time. A wash taken from the sleeve makes the
//! screen feel like it belongs to the record; a fixed dark grey makes the
//! artwork look pasted on.

use egui::{Align, Layout, Rect, RichText, Sense, TextStyle, Ui, Vec2};
use mp_core::color::Rgb;
use mp_core::config::{Visualizer as VizSettings, VisualizerKind};
use mp_core::library::{ArtCache, ArtSize, CoverPalette, Lyrics};

use crate::artwork::Artwork;
use crate::immersive::Immersive;
use crate::player::NowPlaying;
use crate::theme::{Theme, col, col_alpha};
use crate::visualizer::{self, Visualizers};
use crate::widgets::{self, icons::Icon};

/// Fraction of the height the visualiser occupies along the bottom.
const VIZ_HEIGHT: f32 = 0.55;

/// How much the scrim knocks the visualiser back.
///
/// It is scenery, not the subject. At full strength it competes with the
/// artwork and makes the text hard to read; gone entirely, the screen is
/// static while music is playing.
const SCRIM: f32 = 0.62;

/// Width the lyrics column takes when it is open.
const LYRICS_SHARE: f32 = 0.42;

/// How much of the artwork's space the "no cover" card takes.
const PLACEHOLDER_SHARE: f32 = 0.55;

/// Height reserved for the titles, the seek bar and the transport.
///
/// The block is anchored to the bottom of the column and the artwork takes
/// what is left above it, rather than the artwork taking a guessed share and
/// the controls flowing after it. Flowing put the transport off the bottom of
/// the window on a maximised display: the cover was sized from a constant that
/// did not actually match what the text below it needed.
const FOOTER_UNITS: f32 = 24.0;

/// Everything the view needs to draw a frame.
///
/// Gathered into one struct because the alternative is a function with
/// fourteen positional parameters, half of which are `Option`s.
pub struct Scene<'a> {
    pub theme: &'a Theme,
    pub now: Option<&'a NowPlaying>,
    pub artwork: &'a mut Artwork,
    pub art_cache: &'a ArtCache,
    /// The cover's colours, for the background wash.
    pub palette: Option<&'a CoverPalette>,
    pub visualizers: &'a mut Visualizers,
    pub viz: &'a VizSettings,

    pub position: f64,
    pub duration: Option<f64>,
    pub progress: f32,
    pub playing: bool,
    /// The fraction being dragged to, while the user is scrubbing.
    pub scrubbing: Option<f32>,
    pub dt: f32,
}

/// What the user asked for this frame.
#[derive(Debug, Default)]
pub struct Outcome {
    pub close: bool,
    pub toggle_lyrics: bool,
    pub toggle_play: bool,
    pub next: bool,
    pub previous: bool,
    pub seek: Option<f32>,
}

pub fn show(ui: &mut Ui, state: &mut Immersive, scene: Scene<'_>) -> Outcome {
    // Taken apart up front so the artwork cache and the visualiser can be held
    // mutably at the same time: they are separate fields, and separate fields
    // borrow independently once they are no longer behind one struct.
    let Scene {
        theme,
        now,
        artwork,
        art_cache,
        palette,
        visualizers,
        viz,
        position,
        duration,
        progress,
        playing,
        scrubbing,
        dt,
    } = scene;

    let mut outcome = Outcome::default();
    let m = theme.metrics;

    let full = ui.max_rect();
    backdrop(ui, theme, palette, visualizers, viz, dt, full);

    // Escape is handled by the shell's shortcut table, which knows what else
    // is open and backs out of one thing at a time. Handling it here as well
    // would close the view *and* clear the search behind it on one press.

    corner_controls(ui, theme, state, full, &mut outcome);

    let content = full.shrink2(Vec2::new(m.space(5.0), m.space(3.0)));
    let show_lyrics = state.shows_lyrics();

    // The artwork column keeps a comfortable width even with the lyrics open;
    // squeezing it below this makes the cover the smaller half of the screen,
    // which inverts what the view is for.
    let art_width = if show_lyrics {
        content.width() * (1.0 - LYRICS_SHARE)
    } else {
        content.width()
    };

    let art_column = Rect::from_min_size(content.min, Vec2::new(art_width, content.height()));

    let mut column = Column {
        theme,
        now,
        artwork,
        art_cache,
        position,
        duration,
        progress,
        playing,
        scrubbing,
    };

    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(art_column)
            .layout(Layout::top_down(Align::Center)),
        |ui| now_playing_column(ui, &mut column, &mut outcome),
    );

    if show_lyrics {
        let pane = Rect::from_min_max(
            egui::Pos2::new(art_column.right() + m.space(3.0), content.top()),
            content.max,
        );

        ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(pane)
                .layout(Layout::top_down(Align::Min)),
            |ui| lyrics_pane(ui, theme, state, position),
        );
    }

    outcome
}

/// The half of the scene that draws the cover, the titles and the transport.
struct Column<'a> {
    theme: &'a Theme,
    now: Option<&'a NowPlaying>,
    artwork: &'a mut Artwork,
    art_cache: &'a ArtCache,
    position: f64,
    duration: Option<f64>,
    progress: f32,
    playing: bool,
    scrubbing: Option<f32>,
}

// ---------------------------------------------------------------------------
// Background
// ---------------------------------------------------------------------------

/// The wash the whole screen sits on: the cover's own colours, then the
/// visualiser, then a scrim to push both behind the content.
#[allow(clippy::too_many_arguments)]
fn backdrop(
    ui: &mut Ui,
    theme: &Theme,
    palette: Option<&CoverPalette>,
    visualizers: &mut Visualizers,
    viz: &VizSettings,
    dt: f32,
    rect: Rect,
) {
    let p = theme.palette;

    let ground = palette
        .and_then(CoverPalette::backdrop)
        .unwrap_or(p.bg_base);

    // Mixed towards the shell background rather than used raw: a saturated
    // sleeve would otherwise give a screen you cannot read white text on.
    let top = ground.mix(p.bg_base, 0.45);
    let bottom = p.bg_base.darken(0.35);

    let painter = ui.painter();
    visualizer::vertical_gradient(painter, rect, col(top), col(bottom));

    if viz.kind != VisualizerKind::None {
        let band = Rect::from_min_max(
            egui::Pos2::new(rect.left(), rect.bottom() - rect.height() * VIZ_HEIGHT),
            rect.max,
        );

        let clipped = painter.with_clip_rect(band);
        visualizers.draw(&clipped, band, theme, viz, dt);
    }

    // Painted over everything so far, and nothing after — the content is drawn
    // by later calls, which land on top.
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::ZERO, col_alpha(bottom, SCRIM));
}

// ---------------------------------------------------------------------------
// Controls in the corner
// ---------------------------------------------------------------------------

fn corner_controls(
    ui: &mut Ui,
    theme: &Theme,
    state: &Immersive,
    full: Rect,
    outcome: &mut Outcome,
) {
    let m = theme.metrics;
    let size = m.space(3.5);

    let strip = Rect::from_min_max(
        egui::Pos2::new(full.right() - m.space(14.0), full.top() + m.space(1.5)),
        egui::Pos2::new(
            full.right() - m.space(1.5),
            full.top() + m.space(1.5) + size,
        ),
    );

    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(strip)
            .layout(Layout::right_to_left(Align::Center)),
        |ui| {
            if widgets::icon_button(ui, theme, Icon::Collapse, size, false).clicked() {
                outcome.close = true;
            }

            // Only offered when there is something to show. A toggle that
            // reveals an empty pane is worse than no toggle.
            if state.has_lyrics() || state.is_awaiting_lyrics() {
                ui.add_space(m.space(0.5));
                if widgets::icon_button(ui, theme, Icon::Lyrics, size, state.shows_lyrics())
                    .clicked()
                {
                    outcome.toggle_lyrics = true;
                }
            }
        },
    );
}

// ---------------------------------------------------------------------------
// Artwork, titles and transport
// ---------------------------------------------------------------------------

fn now_playing_column(ui: &mut Ui, column: &mut Column<'_>, outcome: &mut Outcome) {
    let theme = column.theme;
    let m = theme.metrics;
    let p = theme.palette;

    let Some(now) = column.now else {
        return;
    };

    let area = ui.max_rect();
    let footer_height = m.space(FOOTER_UNITS).min(area.height() * 0.5);

    let footer = Rect::from_min_max(
        egui::Pos2::new(area.left(), area.bottom() - footer_height),
        area.max,
    );
    let above = Rect::from_min_max(
        area.min,
        egui::Pos2::new(area.right(), footer.top() - m.space(1.5)),
    );

    // Square, and as large as the shorter side of what is left allows.
    let art_size = above.height().min(above.width()).max(m.space(8.0));
    let art_rect = Rect::from_center_size(above.center(), Vec2::splat(art_size));

    artwork(ui, column, art_rect);

    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(footer)
            .layout(Layout::top_down(Align::Center)),
        |ui| {
            ui.label(
                RichText::new(&now.title)
                    .text_style(TextStyle::Heading)
                    .color(col(p.text_primary)),
            );

            ui.add_space(m.space(0.25));
            ui.label(
                RichText::new(now.subtitle())
                    .text_style(TextStyle::Name("subtitle".into()))
                    .color(col(p.text_secondary)),
            );

            ui.add_space(m.space(1.5));
            seek_row(ui, column, outcome);

            ui.add_space(m.space(1.0));
            transport(ui, theme, column.playing, outcome);
        },
    );
}

fn artwork(ui: &mut Ui, column: &mut Column<'_>, bounds: Rect) {
    let theme = column.theme;
    let m = theme.metrics;
    let p = theme.palette;

    if !ui.is_rect_visible(bounds) {
        return;
    }

    let texture = column
        .now
        .and_then(|now| now.art_id.as_deref())
        .and_then(|id| {
            column
                .artwork
                .get(ui.ctx(), column.art_cache, id, ArtSize::Full)
        });

    // Fitted to the cover's own proportions rather than forced square. Sleeves
    // are usually square but not always, and stretching a 3:4 cover across a
    // square frame is immediately visible on a screen this size.
    //
    // With no cover at all the card shrinks rather than filling the screen: a
    // half-metre of empty grey with a small note in the middle looks like the
    // view failed to load, where a modest placeholder reads as "this track has
    // no artwork", which is what actually happened.
    let rect = match &texture {
        Some(texture) => fit(bounds, texture.size_vec2()),
        None => Rect::from_center_size(
            bounds.center(),
            Vec2::splat(bounds.width().min(bounds.height()) * PLACEHOLDER_SHARE),
        ),
    };

    let radius = egui::CornerRadius::same(m.radius_large);

    // A soft drop shadow lifts the cover off the wash. Concentric rounded
    // rectangles at low alpha, because egui has no blur.
    for step in 1..=6 {
        let spread = step as f32 * 2.5;
        ui.painter().rect_filled(
            rect.translate(Vec2::new(0.0, spread * 0.6)).expand(spread),
            radius,
            col_alpha(Rgb::BLACK, 0.045),
        );
    }

    ui.painter().rect_filled(rect, radius, col(p.bg_elevated));

    match texture {
        Some(texture) => {
            egui::Image::from_texture(&texture)
                .corner_radius(radius)
                .paint_at(ui, rect);
        }
        None => crate::widgets::icons::draw(
            ui.painter(),
            Icon::Songs,
            rect.shrink(rect.width().min(rect.height()) * 0.34),
            col_alpha(p.text_muted, 0.4),
            2.0,
        ),
    }
}

/// The largest rectangle of `aspect`'s proportions that fits inside `bounds`,
/// centred.
fn fit(bounds: Rect, aspect: Vec2) -> Rect {
    if aspect.x <= 0.0 || aspect.y <= 0.0 {
        return bounds;
    }

    let scale = (bounds.width() / aspect.x).min(bounds.height() / aspect.y);
    Rect::from_center_size(bounds.center(), aspect * scale)
}

fn seek_row(ui: &mut Ui, column: &Column<'_>, outcome: &mut Outcome) {
    let theme = column.theme;
    let m = theme.metrics;
    let p = theme.palette;

    let duration = column.duration.unwrap_or(0.0);

    // While scrubbing, the clock follows the handle rather than the playhead,
    // so the number and the thumb never disagree.
    let elapsed = match column.scrubbing {
        Some(fraction) => f64::from(fraction) * duration,
        None => column.position,
    };

    ui.horizontal(|ui| {
        let label_width = m.space(6.0);
        let bar = (ui.available_width() - label_width * 2.0).max(m.space(10.0));

        ui.allocate_ui_with_layout(
            Vec2::new(label_width, ui.available_height()),
            Layout::right_to_left(Align::Center),
            |ui| {
                ui.label(
                    RichText::new(widgets::format_duration(elapsed))
                        .text_style(TextStyle::Name("caption".into()))
                        .color(col(p.text_secondary)),
                );
            },
        );

        let (_, seek) = widgets::scrubber(
            ui,
            theme,
            column.progress,
            bar,
            column.duration.is_some_and(|d| d > 0.0),
        );
        outcome.seek = seek;

        ui.allocate_ui_with_layout(
            Vec2::new(label_width, ui.available_height()),
            Layout::left_to_right(Align::Center),
            |ui| {
                ui.label(
                    RichText::new(widgets::format_duration(duration))
                        .text_style(TextStyle::Name("caption".into()))
                        .color(col(p.text_muted)),
                );
            },
        );
    });
}

fn transport(ui: &mut Ui, theme: &Theme, playing: bool, outcome: &mut Outcome) {
    // Shuffle and repeat are deliberately absent: this screen is for looking
    // at the record, and those are decisions about the queue behind it. The
    // shared widget is still what draws it, so the three buttons here and the
    // five in the player bar keep the same proportions and the same baseline.
    let hit = widgets::transport(
        ui,
        theme,
        widgets::Transport::minimal(playing, theme.metrics.space(5.5)),
    );

    outcome.previous |= hit.previous;
    outcome.toggle_play |= hit.toggle_play;
    outcome.next |= hit.next;
}

// ---------------------------------------------------------------------------
// Lyrics
// ---------------------------------------------------------------------------

fn lyrics_pane(ui: &mut Ui, theme: &Theme, state: &mut Immersive, position: f64) {
    let m = theme.metrics;
    let p = theme.palette;

    let Some(lyrics) = state.lyrics().cloned() else {
        // The pane is open with nothing in it, which happens only while a
        // lookup is out. Saying so beats an empty column.
        if state.is_awaiting_lyrics() {
            ui.label(
                RichText::new("Looking for lyrics…")
                    .text_style(TextStyle::Name("caption".into()))
                    .color(col(p.text_muted)),
            );
        }
        return;
    };

    let active = if lyrics.is_synced() {
        lyrics.active_at(std::time::Duration::from_secs_f64(position.max(0.0)))
    } else {
        None
    };

    let scroll_now = state.take_scroll(active);

    // Words that came over the network say so. Showing them exactly like the
    // ones found on disk would be the app quietly passing off a lookup as
    // something it already had, and where a lyric came from is precisely the
    // thing a user of this build would want to know.
    if lyrics.source.is_fetched() {
        ui.label(
            RichText::new(lyrics.source.describe())
                .text_style(TextStyle::Name("caption".into()))
                .color(col(p.text_muted)),
        );
        ui.add_space(m.space(0.5));
    }

    if !lyrics.is_synced() {
        ui.label(
            RichText::new("No timings for these words, so they do not follow along.")
                .text_style(TextStyle::Name("caption".into()))
                .color(col(p.text_muted)),
        );
        ui.add_space(m.space(1.0));
    }

    // egui marks a scrollable edge by fading the content into the enclosing
    // background colour. Every other list in the app sits on a panel, so the
    // fade matches what is behind it and reads as depth. This pane sits on
    // nothing — the cover wash and the visualiser show straight through — so
    // the same effect paints an opaque smear over the artwork that belongs to
    // no surface at all, and it is at its worst against a moving background.
    //
    // Traded for a scroll bar that is simply always there. Same information,
    // and it does not have to guess what colour it is sitting on.
    let scroll = &mut ui.spacing_mut().scroll;
    scroll.fade.strength = 0.0;
    scroll.dormant_handle_opacity = scroll.active_handle_opacity;
    // Thicker than egui's dormant default, which is tuned for a bar that only
    // has to be found once the pointer is already near it. This one is the
    // whole indication that there is more to read.
    scroll.floating_width = m.space(0.5);

    egui::ScrollArea::vertical()
        .id_salt("immersive_lyrics")
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            // Half a pane of padding at each end, so the first and last lines
            // can still sit in the middle when centred.
            let padding = ui.available_height() * 0.4;
            ui.add_space(padding);

            for (index, line) in lyrics.lines.iter().enumerate() {
                let response = lyric_line(ui, theme, &lyrics, line, Some(index) == active);

                if scroll_now && Some(index) == active {
                    response.scroll_to_me(Some(Align::Center));
                }
            }

            ui.add_space(padding);
        });
}

fn lyric_line(
    ui: &mut Ui,
    theme: &Theme,
    lyrics: &Lyrics,
    line: &mp_core::library::lyrics::Line,
    active: bool,
) -> egui::Response {
    let m = theme.metrics;
    let p = theme.palette;

    if line.text.trim().is_empty() {
        let (rect, response) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), m.space(1.5)),
            Sense::hover(),
        );
        let _ = rect;
        return response;
    }

    // Unsynced lyrics have no "current" line, so every line reads at the same
    // weight rather than all of them looking disabled.
    let (colour, size) = if !lyrics.is_synced() {
        (p.text_secondary, 16.0)
    } else if active {
        (p.text_primary, 20.0)
    } else {
        (p.text_muted, 16.0)
    };

    let text = RichText::new(&line.text)
        .font(egui::FontId::new(size, egui::FontFamily::Proportional))
        .color(col(colour));

    let response = ui.add(egui::Label::new(text).wrap());
    ui.add_space(m.space(0.75));
    response
}
