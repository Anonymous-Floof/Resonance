//! The application shell: nav rail, content area and player bar.
//!
//! This file is about layout and routing only. What is *in* the library lives
//! in [`crate::library::LibraryState`], what is playing lives in
//! [`crate::player::Player`], and how anything looks lives in the theme — so a
//! change to any one of those does not come back here.

use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::Frame;
use egui::{Align, Layout, Rect, RichText, TextStyle, Ui, Vec2};
use mp_core::color::Rgb;
use mp_core::config::{SurfaceStyle, ThemeMode, VizColorMode};
use mp_core::library::{ArtSize, Library, Order};
use mp_core::{AppPaths, Config};
use mp_net::Activity;

use crate::adaptive::Adaptive;
use crate::analysis_job::AnalysisJob;
use crate::artwork::Artwork;
use crate::fonts::{self, FontReport};
use crate::immersive::Immersive;
use crate::library::{Emptiness, Focus, LibraryState};
use crate::lyrics_job::LyricsJob;
use crate::platform::{MediaCommand, MediaControls, NowPlayingInfo, PlaybackState};
use crate::player::Player;
use crate::playlists::PlaylistState;
use crate::shortcuts::{self, Action};
use crate::surface;
use crate::tag_editor::TagEditor;
use crate::theme::{Theme, col, col_alpha};
use crate::views::{self, View, browse};
use crate::visualizer::Visualizers;
use crate::widgets::{self, icons::Icon};

/// Settings are saved this long after the last edit, so dragging a slider
/// writes the file once rather than sixty times a second.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(800);

pub struct ResonanceApp {
    paths: AppPaths,
    config: Config,
    theme: Theme,
    fonts: FontReport,

    view: View,
    nav_collapsed: bool,
    /// Live text in the search box, mirrored into the library on change.
    search: String,

    /// Set when the config changes; cleared once written to disk.
    dirty: bool,
    dirty_since: Option<Instant>,

    library: LibraryState,
    artwork: Artwork,
    player: Player,

    /// Wall-clock of the previous frame, for animating notices.
    last_frame: Instant,

    /// The background audio-analysis pass, when one is running.
    analysis: Option<AnalysisJob>,

    playlists: PlaylistState,
    /// Scratch state for the playlist view (in-place renaming).
    playlist_editing: views::playlists::Editing,

    /// The cover-derived accent, and the fade between covers.
    adaptive: Adaptive,

    /// The full-screen now-playing view and its lyrics.
    immersive: Immersive,

    /// The record of every request this build has made.
    ///
    /// Held even when nothing can make a request, so the settings page can
    /// show the file and its history whether or not fetching is switched on
    /// right now. Opening it writes nothing until something is recorded.
    activity: Arc<Activity>,

    /// The lyrics fetcher, present only while online lyrics are switched on.
    ///
    /// Started and stopped by [`Self::tend_lyrics`] as the setting changes, so
    /// there is no worker thread and no possibility of a request while the
    /// feature is off.
    lyrics_job: Option<LyricsJob>,

    /// Whether the first-run welcome is still showing.
    ///
    /// Set only when the app started with no config file at all, so it appears
    /// exactly once per installation and never after an upgrade.
    welcome: bool,

    /// Set by the search shortcut; consumed by the search box next frame.
    ///
    /// A flag rather than a direct focus call because the box does not exist
    /// yet when the key is read — the shell handles input before it lays
    /// anything out.
    focus_search: bool,

    /// The Windows media session: the media flyout, and the media keys.
    media: MediaControls,

    /// The tag editor, when one is open.
    tag_editor: TagEditor,

    /// The visualiser feed and its analysis, kept across frames.
    visualizers: Visualizers,
    /// Seconds since the visualiser last ran, so a view that has been off
    /// screen does not resume with a stale delta.
    viz_dt: f32,

    /// The Home page's figures, and the library epoch they were read at.
    ///
    /// Cached because every one of them is a query, and Home is redrawn on
    /// every frame that anything animates.
    home: HomeData,
    home_epoch: Option<u64>,

    /// The queue panel's rows, and the queue revision they were built from.
    ///
    /// Resolving a queue entry to a library track is a lookup per entry, so it
    /// is done when the engine republishes the queue rather than per frame.
    queue_rows: Vec<views::queue::Row>,
    queue_revision: Option<u64>,
}

/// Everything the Home page shows, read in one pass.
#[derive(Default)]
struct HomeData {
    totals: mp_core::library::stats::Totals,
    activity: Vec<u32>,
    favourites: Vec<mp_core::library::stats::PlayedTrack>,
    recent: Vec<mp_core::library::Track>,
    artists: Vec<mp_core::library::stats::Ranked>,
    albums: Vec<mp_core::library::stats::Ranked>,
}

/// The smallest the window can be dragged to.
///
/// Exported so the viewport builder and this file cannot disagree: the check
/// that tells a real size from the nonsense a minimised window reports is only
/// correct if it matches the limit actually enforced.
pub const MIN_WINDOW_SIZE: [f32; 2] = [880.0, 560.0];

/// How many past tag edits the history panel shows.
///
/// Enough to cover a session's worth of fixing up an album, short of turning
/// Settings into a log viewer.
const TAG_HISTORY_SHOWN: usize = 50;

impl ResonanceApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        paths: AppPaths,
        config: Config,
        first_run: bool,
    ) -> Self {
        let fonts = fonts::install(&cc.egui_ctx, &config.appearance.font_candidates);
        let theme = Theme::new(&config.appearance, None);

        theme.apply(&cc.egui_ctx);
        cc.egui_ctx.set_zoom_factor(config.appearance.ui_scale);

        // Windows would otherwise paint the title bar from the system accent
        // colour, which has no relationship to our palette.
        // The aurora visualiser renders through its own wgpu pipeline, which
        // has to be built here: this is the only place the surface's texture
        // format is available.
        crate::visualizer::aurora::install(cc);

        crate::platform::apply_window_chrome(
            cc,
            theme.palette.dark_mode,
            config.appearance.mica_backdrop,
        );

        // Bound to the window, which is how the shell knows whose media
        // session this is. Silently absent if it cannot be created.
        let media = MediaControls::new(cc);
        if media.is_active() {
            tracing::info!("system media controls attached");
        }

        // The disclosure log lives in the data directory rather than the
        // cache, because a record the application may clear whenever it likes
        // is a weaker promise than one it may not. Failing to open it costs
        // the file and nothing else — the entries are still kept in memory and
        // still shown in Settings.
        let activity = Arc::new(
            Activity::open(paths.data_dir().join(mp_net::LOG_FILE_NAME)).unwrap_or_else(|err| {
                tracing::warn!("network activity will not be written to disk: {err:#}");
                Activity::in_memory()
            }),
        );

        let library = match Library::open(&paths) {
            Ok(library) => library,
            Err(err) => {
                // Losing the index is recoverable; refusing to start is not.
                tracing::error!("library unavailable, running without an index: {err:#}");
                Library::in_memory().expect("an in-memory library cannot fail to open")
            }
        };

        let mut library = LibraryState::new(library, &config);
        if config.library.scan_on_startup {
            library.start_scan(&config);
        }

        let player = Player::new(&config);

        Self {
            nav_collapsed: config.window.nav_collapsed,
            // Whichever section the user asked to open on. With a library
            // present it is immediately useful, and without one it explains
            // how to add a folder.
            view: View::for_grouping(config.library.default_grouping),
            search: String::new(),
            paths,
            config,
            theme,
            fonts,
            dirty: false,
            dirty_since: None,
            library,
            artwork: Artwork::new(),
            player,
            last_frame: Instant::now(),
            analysis: None,
            playlists: PlaylistState::new(),
            playlist_editing: views::playlists::Editing::default(),
            adaptive: Adaptive::new(),
            immersive: Immersive::new(),
            activity,
            lyrics_job: None,
            welcome: first_run,
            focus_search: false,
            media,
            tag_editor: TagEditor::default(),
            visualizers: Visualizers::new(),
            viz_dt: 0.0,
            home: HomeData::default(),
            home_epoch: None,
            queue_rows: Vec::new(),
            queue_revision: None,
        }
    }

    /// Mark the config as needing a save, restarting the debounce window.
    fn touch(&mut self) {
        self.dirty = true;
        self.dirty_since = Some(Instant::now());
    }

    /// Rebuild the style after a theme-affecting setting changed.
    ///
    /// The cover accent is passed through rather than dropped: switching
    /// density or font would otherwise reset the interface to the configured
    /// colour until the next track change.
    fn restyle(&mut self, ctx: &egui::Context) {
        self.config.validate();
        self.theme = Theme::new(&self.config.appearance, self.art_accent());
        self.theme.apply(ctx);
        ctx.set_zoom_factor(self.config.appearance.ui_scale);
    }

    /// The accent the current cover is contributing, if the theme wants one.
    ///
    /// Resolved against the configured colour here rather than in `Adaptive`,
    /// because only this layer knows what the user picked in settings.
    fn art_accent(&self) -> Option<Rgb> {
        if self.config.appearance.theme != ThemeMode::Adaptive {
            return None;
        }

        let configured =
            Rgb::parse_hex_or(&self.config.appearance.accent, Rgb::new(0x7C, 0x5C, 0xFF));

        Some(self.adaptive.accent(configured))
    }

    /// Whether anything on screen is currently driven by the cover's colours.
    ///
    /// Reading a palette touches the disk the first time it is asked for, so
    /// it is only done when something will actually use the answer. The
    /// visualiser's album-art colour mode counts as much as the Adaptive
    /// theme does — it was the case this check originally missed, which left
    /// that mode quietly drawing in the accent colour.
    fn wants_cover_colours(&self) -> bool {
        self.config.appearance.theme == ThemeMode::Adaptive
            || self.config.visualizer.color_mode == VizColorMode::AlbumArt
    }

    /// Follow the current cover's colour, restyling while the fade runs.
    ///
    /// The palette is tracked for any feature that wants it; only the Adaptive
    /// theme restyles from it. In the fixed themes the palette is a promise
    /// that the interface holds still, so the accent is left alone even while
    /// the visualiser is colouring itself from the same cover.
    fn follow_artwork(&mut self, ctx: &egui::Context, dt: f32) {
        if !self.wants_cover_colours() {
            // Anything held from a moment when it was wanted is now stale.
            self.visualizers.set_cover(None);
            return;
        }

        let art_id = self
            .player
            .now_playing
            .as_ref()
            .and_then(|now| now.art_id.clone());

        let started = self.adaptive.observe(&self.library, art_id.as_deref());
        let moved = self.adaptive.advance(dt);

        if started {
            self.visualizers.set_cover(self.adaptive.palette().cloned());
        }

        if self.config.appearance.theme == ThemeMode::Adaptive && (started || moved) {
            self.theme = Theme::new(&self.config.appearance, self.art_accent());
            self.theme.apply(ctx);

            if self.adaptive.is_animating() {
                ctx.request_repaint();
            }
        }
    }

    /// Write the config if the debounce window has elapsed.
    fn maybe_save(&mut self) {
        let due = self
            .dirty_since
            .is_some_and(|since| since.elapsed() >= SAVE_DEBOUNCE);

        if self.dirty && due {
            self.save_config();
        }
    }

    /// Write the config now, whatever the debounce says.
    fn save_config(&mut self) {
        if !self.dirty {
            return;
        }

        self.config.validate();
        if let Err(err) = self.config.save(&self.paths) {
            tracing::error!("could not save settings: {err:#}");
        }
        self.dirty = false;
        self.dirty_since = None;
    }

    /// Start playing the list currently on screen, from `index`.
    /// The path of one row of the visible list.
    fn visible_path(&self, index: usize) -> Option<std::path::PathBuf> {
        self.library.tracks().get(index).map(|t| t.path.clone())
    }

    fn play_visible(&mut self, index: usize) {
        let paths = self.library.visible_paths();
        self.player.play(paths, index);
    }

    // -----------------------------------------------------------------------
    // Nav rail
    // -----------------------------------------------------------------------

    fn nav_rail(&mut self, ui: &mut Ui) {
        let m = self.theme.metrics;
        let width = if self.nav_collapsed {
            m.nav_collapsed_width
        } else {
            m.nav_width
        };

        egui::Panel::left("nav")
            .exact_size(width)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(col(self.theme.palette.bg_surface))
                    .inner_margin(egui::Margin::symmetric(
                        m.space(1.0) as i8,
                        m.space(1.5) as i8,
                    )),
            )
            .show(ui, |ui| {
                self.nav_header(ui);
                ui.add_space(m.space(2.0));

                for view in [View::Home, View::Playlists] {
                    if widgets::nav_item(
                        ui,
                        &self.theme,
                        view.icon(),
                        view.label(),
                        self.view == view,
                        self.nav_collapsed,
                    )
                    .clicked()
                    {
                        self.go_to(view);
                    }
                }

                if self.nav_collapsed {
                    ui.add_space(m.space(1.5));
                } else {
                    widgets::nav_section_label(ui, &self.theme, "Library");
                }

                for view in View::LIBRARY {
                    if widgets::nav_item(
                        ui,
                        &self.theme,
                        view.icon(),
                        view.label(),
                        self.view == view,
                        self.nav_collapsed,
                    )
                    .clicked()
                    {
                        self.go_to(view);
                    }
                }

                // Settings pinned to the bottom, away from the content views.
                ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                    if widgets::nav_item(
                        ui,
                        &self.theme,
                        View::Settings.icon(),
                        View::Settings.label(),
                        self.view == View::Settings,
                        self.nav_collapsed,
                    )
                    .clicked()
                    {
                        self.view = View::Settings;
                    }

                    if !self.nav_collapsed {
                        self.scan_status(ui);
                    }
                });
            });
    }

    /// Switch sections, leaving any drill-down behind.
    ///
    /// Keeping the focus would mean clicking "Albums" from inside an artist
    /// showed that artist's albums under a heading that says otherwise.
    fn go_to(&mut self, view: View) {
        if self.view != view {
            self.library.close_focus();
        }
        self.view = view;
    }

    /// App name plus the collapse toggle.
    fn nav_header(&mut self, ui: &mut Ui) {
        let m = self.theme.metrics;

        ui.horizontal(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let icon = if self.nav_collapsed {
                    Icon::ChevronRight
                } else {
                    Icon::ChevronLeft
                };
                if widgets::icon_button(ui, &self.theme, icon, m.space(3.0), false).clicked() {
                    self.nav_collapsed = !self.nav_collapsed;
                    self.config.window.nav_collapsed = self.nav_collapsed;
                    self.touch();
                }
            });
        });
    }

    /// A live line at the foot of the rail while a scan is running.
    fn scan_status(&mut self, ui: &mut Ui) {
        let Some(progress) = self.library.scan_progress() else {
            return;
        };

        let m = self.theme.metrics;
        let p = self.theme.palette;

        let phase = progress.phase();
        let detail = match progress.fraction() {
            Some(fraction) => format!("{}%", (fraction * 100.0).round() as u32),
            None => format!("{} found", progress.found()),
        };

        ui.add_space(m.space(1.0));
        ui.label(
            RichText::new(phase.label())
                .text_style(TextStyle::Name("caption".into()))
                .color(col(p.text_secondary)),
        );
        ui.label(
            RichText::new(detail)
                .text_style(TextStyle::Name("caption".into()))
                .color(col(p.text_muted)),
        );
    }

    // -----------------------------------------------------------------------
    // Player bar
    // -----------------------------------------------------------------------

    fn player_bar(&mut self, ui: &mut Ui) {
        let m = self.theme.metrics;
        let p = self.theme.palette;

        let margin = Vec2::new(m.space(2.0), m.space(1.0));

        egui::Panel::bottom("player")
            .exact_size(m.player_bar_height)
            .frame(
                // Transparent for the same reason as the content panel: the
                // backdrop below paints the fill.
                egui::Frame::new()
                    .fill(egui::Color32::TRANSPARENT)
                    .inner_margin(egui::Margin::symmetric(margin.x as i8, margin.y as i8)),
            )
            .show(ui, |ui| {
                let bar = Self::panel_rect(ui, margin);
                self.paint_backdrop(
                    ui,
                    bar,
                    self.config.appearance.player_background,
                    p.bg_surface,
                );

                // A hairline separates the bar from the content above it.
                ui.painter().rect_filled(
                    Rect::from_min_size(bar.min, Vec2::new(bar.width(), 1.0)),
                    egui::CornerRadius::ZERO,
                    col(p.border),
                );

                ui.horizontal_centered(|ui| {
                    self.now_playing_block(ui);
                    self.transport_block(ui);
                    self.player_bar_right(ui);
                });
            });
    }

    /// Artwork thumbnail, title and artist.
    fn now_playing_block(&mut self, ui: &mut Ui) {
        let m = self.theme.metrics;
        let p = self.theme.palette;

        ui.allocate_ui_with_layout(
            Vec2::new(m.space(28.0), ui.available_height()),
            Layout::left_to_right(Align::Center),
            |ui| {
                let size = m.space(6.0);
                let (rect, response) =
                    ui.allocate_exact_size(Vec2::splat(size), egui::Sense::click());

                if ui.is_rect_visible(rect) {
                    let radius = egui::CornerRadius::same(m.radius_medium);
                    ui.painter().rect_filled(rect, radius, col(p.bg_elevated));

                    let texture = self
                        .player
                        .now_playing
                        .as_ref()
                        .and_then(|now| now.art_id.as_deref())
                        .and_then(|id| {
                            self.artwork
                                .get(ui.ctx(), self.library.art(), id, ArtSize::Card)
                        });

                    match texture {
                        Some(texture) => {
                            egui::Image::from_texture(&texture)
                                .corner_radius(radius)
                                .paint_at(ui, rect);
                        }
                        None => crate::widgets::icons::draw(
                            ui.painter(),
                            Icon::Songs,
                            rect.shrink(size * 0.28),
                            col_alpha(p.text_muted, 0.5),
                            1.6,
                        ),
                    }
                }

                // Clicking the cover jumps the list to the album it came from.
                if response.clicked() {
                    self.reveal_now_playing();
                }

                ui.add_space(m.space(1.25));

                // Two lines: what is playing, and where it came from. Falls back
                // to a prompt when nothing has been started.
                let stats = self.library.stats();
                let (title, subtitle) = match &self.player.now_playing {
                    Some(now) => (now.title.clone(), now.subtitle()),
                    None if self.player.engine_error.is_some() => (
                        "No audio output".to_owned(),
                        "Check your sound device".to_owned(),
                    ),
                    None if stats.tracks == 0 => (
                        "Nothing playing".to_owned(),
                        "Add a folder to get started".to_owned(),
                    ),
                    None => (
                        "Nothing playing".to_owned(),
                        format!("{} tracks ready", stats.tracks),
                    ),
                };

                let available = ui.available_width();

                // Copied out before the closure: the names are drawn from
                // `self.player`, and following one of them needs `&mut self`.
                let names = self.player.now_playing.as_ref().map(|now| {
                    (
                        now.artist.clone(),
                        now.album.clone(),
                        now.artist_id,
                        now.album_id,
                    )
                });
                let theme = &self.theme;
                let caption = ui
                    .style()
                    .text_styles
                    .get(&TextStyle::Name("caption".into()))
                    .cloned()
                    .unwrap_or(egui::FontId::proportional(11.0));

                let mut open_artist = None;
                let mut open_album = None;

                ui.vertical(|ui| {
                    ui.add_space(m.space(0.75));
                    ui.set_max_width(available);
                    ui.label(
                        RichText::new(title)
                            .text_style(TextStyle::Body)
                            .color(col(p.text_primary)),
                    );

                    match &names {
                        Some((artist, album, artist_id, album_id)) => {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 0.0;
                                let mut left = available;

                                let plain = |ui: &mut Ui, text: &str, left: &mut f32| {
                                    let shown = widgets::elide(ui, text, &caption, *left);
                                    let response = ui.label(
                                        RichText::new(shown)
                                            .text_style(TextStyle::Name("caption".into()))
                                            .color(col(p.text_muted)),
                                    );
                                    *left -= response.rect.width();
                                };

                                match artist_id {
                                    Some(id) => {
                                        let hit =
                                            widgets::link_text(ui, theme, artist, &caption, left);
                                        left -= hit.rect.width();
                                        if hit.clicked() {
                                            open_artist = Some((*id, artist.clone()));
                                        }
                                    }
                                    // An artist the index does not know is not
                                    // a link. A control that goes nowhere is
                                    // worse than plain text.
                                    None => plain(ui, artist, &mut left),
                                }

                                if let Some(album) = album {
                                    plain(ui, " — ", &mut left);
                                    match album_id {
                                        Some(id) => {
                                            let hit = widgets::link_text(
                                                ui, theme, album, &caption, left,
                                            );
                                            left -= hit.rect.width();
                                            if hit.clicked() {
                                                open_album =
                                                    Some((*id, album.clone(), artist.clone()));
                                            }
                                        }
                                        None => plain(ui, album, &mut left),
                                    }
                                }

                                let _ = left;
                            });
                        }
                        // Nothing playing: the second line is a prompt, not a
                        // pair of names.
                        None => {
                            ui.label(
                                RichText::new(subtitle)
                                    .text_style(TextStyle::Name("caption".into()))
                                    .color(col(p.text_muted)),
                            );
                        }
                    }
                });

                if let Some((id, name)) = open_artist {
                    self.open_artist(id, name);
                }
                if let Some((id, title, artist)) = open_album {
                    self.open_album(id, title, artist);
                }
            },
        );
    }

    /// Show an artist's page, from wherever the name was clicked.
    ///
    /// The nav rail moves with it: arriving inside "Artists → Someone" while
    /// the rail still says Queue would leave Back with nowhere sensible to go.
    fn open_artist(&mut self, id: mp_core::library::model::ArtistId, name: String) {
        self.library.open(Focus::Artist { id, name });
        self.view = View::Artists;
    }

    /// Show an album's page, from wherever the name was clicked.
    fn open_album(&mut self, id: mp_core::library::model::AlbumId, title: String, artist: String) {
        self.library.open(Focus::Album { id, title, artist });
        self.view = View::Albums;
    }

    /// Jump the browser to the album (or artist) of the playing track.
    fn reveal_now_playing(&mut self) {
        let Some(now) = &self.player.now_playing else {
            return;
        };
        let Some(track) = self.library.track_at_path(&now.path) else {
            return;
        };

        if let Some(id) = track.album_id {
            let (title, artist) = (track.album.clone(), track.artist.clone());
            self.open_album(id, title, artist);
        } else if let Some(id) = track.artist_id {
            let name = track.artist.clone();
            self.open_artist(id, name);
        }
    }

    /// Transport buttons and the seek bar.
    fn transport_block(&mut self, ui: &mut Ui) {
        let m = self.theme.metrics;
        let p = self.theme.palette;
        let available = ui.available_width();
        // Leave room for the right-hand controls.
        let block_width = (available - m.space(24.0)).max(m.space(30.0));

        ui.allocate_ui_with_layout(
            Vec2::new(block_width, ui.available_height()),
            Layout::top_down(Align::Center),
            |ui| {
                ui.add_space(m.space(0.5));

                let hit = widgets::transport(
                    ui,
                    &self.theme,
                    widgets::Transport {
                        playing: self.player.is_playing(),
                        size: m.space(4.5),
                        shuffle: Some(
                            self.config.playback.shuffle != mp_core::config::ShuffleMode::Off,
                        ),
                        repeat: Some((
                            match self.config.playback.repeat {
                                mp_core::config::RepeatMode::One => Icon::RepeatOne,
                                _ => Icon::Repeat,
                            },
                            self.config.playback.repeat != mp_core::config::RepeatMode::Off,
                        )),
                    },
                );

                if hit.shuffle {
                    self.cycle_shuffle();
                }
                if hit.previous {
                    self.player.previous();
                }
                if hit.toggle_play {
                    self.toggle_play_pause();
                }
                if hit.next {
                    self.player.next();
                }
                if hit.repeat {
                    self.cycle_repeat();
                }

                ui.add_space(m.space(0.25));

                ui.horizontal(|ui| {
                    let label_width = m.space(5.0);
                    let bar_width = (ui.available_width() - label_width * 2.0).max(m.space(10.0));

                    // While scrubbing, show the drag target rather than the
                    // playhead, so the numbers agree with the handle.
                    let elapsed = match self.player.scrubbing {
                        Some(fraction) => {
                            f64::from(fraction) * self.player.duration_secs().unwrap_or(0.0)
                        }
                        None => self.player.position_secs(),
                    };

                    ui.allocate_ui_with_layout(
                        Vec2::new(label_width, m.space(2.0)),
                        Layout::right_to_left(Align::Center),
                        |ui| {
                            ui.label(
                                RichText::new(widgets::format_duration(elapsed))
                                    .text_style(TextStyle::Name("caption".into()))
                                    .color(col(p.text_muted)),
                            );
                        },
                    );

                    ui.add_space(m.space(0.75));

                    // Seeking needs a known length; a stream with no duration
                    // still plays, it just cannot be scrubbed.
                    let seekable = self.player.duration_secs().is_some_and(|d| d > 0.0);
                    let (response, seek) = widgets::scrubber(
                        ui,
                        &self.theme,
                        self.player.progress(),
                        bar_width - m.space(1.5),
                        seekable,
                    );

                    if let Some(fraction) = seek {
                        // Track the handle live, but only commit the seek when
                        // the drag ends: seeking on every frame of a drag would
                        // thrash the decoder.
                        self.player.scrubbing = Some(fraction);
                    }
                    if (response.drag_stopped() || response.clicked())
                        && let Some(fraction) = self.player.scrubbing.take()
                    {
                        self.player.seek_fraction(fraction);
                    }

                    ui.add_space(m.space(0.75));

                    let total = self
                        .player
                        .duration_secs()
                        .map_or_else(|| "--:--".to_owned(), widgets::format_duration);

                    ui.label(
                        RichText::new(total)
                            .text_style(TextStyle::Name("caption".into()))
                            .color(col(p.text_muted)),
                    );
                });
            },
        );
    }

    /// Play/pause, starting the visible list when nothing is loaded yet.
    fn toggle_play_pause(&mut self) {
        if self.player.has_queue() {
            self.player.toggle_play_pause();
            return;
        }
        if !self.library.tracks().is_empty() {
            self.play_visible(0);
        }
    }

    /// Volume, equalizer, visualizer and queue toggles.
    fn player_bar_right(&mut self, ui: &mut Ui) {
        let m = self.theme.metrics;

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let queue_open = self.config.window.queue_panel_open;
            if widgets::icon_button(ui, &self.theme, Icon::Queue, m.space(3.0), queue_open)
                .clicked()
            {
                self.config.window.queue_panel_open = !queue_open;
                self.touch();
            }

            // Full screen sits at the far right, next to the queue: both are
            // about how the player is laid out rather than what it is doing.
            let playing = self.player.current_path().map(std::path::Path::to_path_buf);
            let enabled = playing.is_some();
            let response = widgets::icon_button_labelled(
                ui,
                &self.theme,
                Icon::Expand,
                m.space(3.0),
                false,
                if enabled {
                    "Full screen"
                } else {
                    "Full screen (nothing playing)"
                },
            );
            if response.clicked() {
                self.immersive.open(playing.as_deref());
            }

            // Opens the visualiser rather than jumping to Settings: the
            // controls that matter while watching it are on the view itself.
            let viz_on = self.config.visualizer.kind != mp_core::config::VisualizerKind::None;
            if widgets::icon_button_labelled(
                ui,
                &self.theme,
                Icon::Visualizer,
                m.space(3.0),
                viz_on,
                if viz_on {
                    "Visualizer"
                } else {
                    "Visualizer (off)"
                },
            )
            .clicked()
            {
                self.go_to(View::Visualizer);
            }

            // Opens the equalizer rather than toggling it: a blind on/off from
            // the player bar changes the sound with nothing on screen to say
            // what it changed it to.
            let eq_on = self.config.equalizer.enabled;
            if widgets::icon_button_labelled(
                ui,
                &self.theme,
                Icon::Equalizer,
                m.space(3.0),
                eq_on,
                if eq_on { "Equalizer (on)" } else { "Equalizer" },
            )
            .clicked()
            {
                self.go_to(View::Equalizer);
            }

            ui.add_space(m.space(0.5));

            let volume = self.config.playback.volume;
            let (_, changed) = widgets::scrubber(ui, &self.theme, volume, m.space(10.0), true);
            if let Some(v) = changed {
                self.config.playback.volume = v;
                self.player.set_volume(v);
                self.touch();
            }

            let icon = match (self.config.playback.muted, self.config.playback.volume) {
                (true, _) => Icon::VolumeMute,
                (_, v) if v <= 0.001 => Icon::VolumeMute,
                (_, v) if v < 0.5 => Icon::VolumeLow,
                _ => Icon::VolumeHigh,
            };
            if widgets::icon_button(ui, &self.theme, icon, m.space(3.0), false).clicked() {
                let muted = !self.config.playback.muted;
                self.config.playback.muted = muted;
                self.player.set_muted(muted);
                self.touch();
            }
        });
    }

    fn cycle_shuffle(&mut self) {
        use mp_core::config::ShuffleMode::{Off, Random, Smart};
        self.config.playback.shuffle = match self.config.playback.shuffle {
            Off => Smart,
            Smart => Random,
            Random => Off,
        };
        self.player.set_shuffle(self.config.playback.shuffle);
        self.touch();
    }

    fn cycle_repeat(&mut self) {
        use mp_core::config::RepeatMode::{All, Off, One};
        self.config.playback.repeat = match self.config.playback.repeat {
            Off => All,
            All => One,
            One => Off,
        };
        self.player.set_repeat(self.config.playback.repeat);
        self.touch();
    }

    // -----------------------------------------------------------------------
    // Content
    // -----------------------------------------------------------------------

    fn content(&mut self, ui: &mut Ui) {
        let m = self.theme.metrics;

        let margin = Vec2::new(m.space(3.0), m.space(2.0));

        egui::CentralPanel::default()
            .frame(
                // Transparent, because the backdrop below paints the fill. A
                // frame colour here would sit on top of it.
                egui::Frame::new()
                    .fill(egui::Color32::TRANSPARENT)
                    .inner_margin(egui::Margin::symmetric(margin.x as i8, margin.y as i8)),
            )
            .show(ui, |ui| {
                let rect = Self::panel_rect(ui, margin);
                self.paint_backdrop(
                    ui,
                    rect,
                    self.config.appearance.content_background,
                    self.theme.palette.bg_base,
                );

                if self.welcome {
                    self.welcome_view(ui);
                    return;
                }

                match self.view {
                    View::Settings => self.settings_view(ui),
                    View::Equalizer => self.equalizer_view(ui),
                    View::Visualizer => self.visualizer_view(ui),
                    View::Playlists => self.playlists_view(ui),
                    View::Home => self.home_view(ui),
                    _ => self.library_view(ui),
                }
            });
    }

    // -----------------------------------------------------------------------
    // Home
    // -----------------------------------------------------------------------

    /// Re-read the statistics if anything behind them has changed.
    fn refresh_home(&mut self) {
        let epoch = self.library.stats_epoch();
        if self.home_epoch == Some(epoch) {
            return;
        }
        self.home_epoch = Some(epoch);

        let library = self.library.library();
        let mut home = HomeData::default();

        // A statistic that cannot be read is not worth taking the page down
        // for: each one falls back to empty and the rest still render.
        match library.totals() {
            Ok(totals) => home.totals = totals,
            Err(err) => tracing::warn!("could not read listening totals: {err:#}"),
        }
        home.activity = library
            .activity(views::home::ACTIVITY_DAYS)
            .unwrap_or_default();
        home.favourites = library
            .top_tracks(views::home::LIST_LIMIT)
            .unwrap_or_default();
        home.recent = library
            .recently_played_tracks(views::home::LIST_LIMIT)
            .unwrap_or_default();
        home.artists = library
            .top_artists(views::home::CARD_LIMIT)
            .unwrap_or_default();
        home.albums = library
            .top_albums(views::home::CARD_LIMIT)
            .unwrap_or_default();

        self.home = home;
    }

    fn home_view(&mut self, ui: &mut Ui) {
        self.refresh_home();

        let shortcuts: Vec<views::home::Shortcut> = self
            .playlists
            .playlists()
            .iter()
            .take(views::home::CARD_LIMIT)
            .map(|playlist| views::home::Shortcut {
                id: playlist.id,
                name: playlist.name.clone(),
                tracks: playlist.track_count as usize,
            })
            .collect();

        let outcome = views::home::show(
            ui,
            &self.theme,
            views::home::Data {
                totals: &self.home.totals,
                activity: &self.home.activity,
                favourites: &self.home.favourites,
                recent: &self.home.recent,
                artists: &self.home.artists,
                albums: &self.home.albums,
                playlists: &shortcuts,
            },
        );

        if let Some((source, index)) = outcome.play {
            // The whole list is queued behind the row, so pressing a favourite
            // starts a run through the favourites rather than stranding you on
            // a queue of one.
            let paths: Vec<std::path::PathBuf> = match source {
                views::home::Source::Favourites => self
                    .home
                    .favourites
                    .iter()
                    .map(|played| played.track.path.clone())
                    .collect(),
                views::home::Source::Recent => {
                    self.home.recent.iter().map(|t| t.path.clone()).collect()
                }
            };
            self.player.play(paths, index);
        }

        if let Some(id) = outcome.open_artist
            && let Some(entry) = self.home.artists.iter().find(|a| a.id == id)
        {
            let name = entry.name.clone();
            self.library.open(Focus::Artist { id, name });
            self.view = View::Artists;
        }

        if let Some(id) = outcome.open_album
            && let Some(entry) = self.home.albums.iter().find(|a| a.id == id)
        {
            let focus = Focus::Album {
                id,
                title: entry.name.clone(),
                artist: entry.detail.clone(),
            };
            self.library.open(focus);
            self.view = View::Albums;
        }

        if let Some(id) = outcome.open_playlist {
            self.playlists.open(self.library.library(), id);
            self.view = View::Playlists;
        }

        if outcome.browse {
            self.go_to(View::Songs);
        }
    }

    // -----------------------------------------------------------------------
    // Queue
    // -----------------------------------------------------------------------

    /// Rebuild the queue rows if the engine has republished the queue.
    fn refresh_queue_rows(&mut self) {
        let revision = self.player.queue_revision();
        if self.queue_revision == Some(revision) {
            return;
        }
        self.queue_revision = Some(revision);

        self.queue_rows = self
            .player
            .queue()
            .iter()
            .map(|entry| {
                // A queued file need not be in the library - playing something
                // from outside it is allowed - so fall back to the filename.
                match self.library.track_at_path(&entry.path) {
                    Some(track) => views::queue::Row {
                        index: entry.index,
                        title: track.title.clone(),
                        artist: track.artist.clone(),
                        album: (track.album != mp_core::library::model::UNKNOWN_ALBUM
                            && !track.album.is_empty())
                        .then(|| track.album.clone()),
                        artist_id: track.artist_id,
                        album_id: track.album_id,
                        duration: track.duration,
                    },
                    None => views::queue::Row {
                        index: entry.index,
                        title: entry
                            .path
                            .file_stem()
                            .map_or_else(String::new, |s| s.to_string_lossy().into()),
                        artist: String::new(),
                        album: None,
                        artist_id: None,
                        album_id: None,
                        duration: None,
                    },
                }
            })
            .collect();
    }

    fn queue_panel(&mut self, ui: &mut Ui) {
        if !self.config.window.queue_panel_open {
            return;
        }

        self.refresh_queue_rows();

        let m = self.theme.metrics;
        let cursor = self.player.queue_cursor();

        let outcome = egui::Panel::right("queue")
            .exact_size(m.queue_width)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(col(self.theme.palette.bg_surface))
                    .inner_margin(egui::Margin::symmetric(
                        m.space(1.5) as i8,
                        m.space(1.5) as i8,
                    )),
            )
            .show(ui, |ui| {
                views::queue::show(
                    ui,
                    &self.theme,
                    &self.queue_rows,
                    cursor,
                    self.config.playback.play_on_single_click,
                )
            })
            .inner;

        if let Some(index) = outcome.jump {
            self.player.jump_to(index);
        }
        if let Some((id, name)) = outcome.open_artist {
            self.open_artist(id, name);
        }
        if let Some((id, title, artist)) = outcome.open_album {
            self.open_album(id, title, artist);
        }
        if let Some((from, to)) = outcome.reorder {
            self.player.reorder_queue(from, to);
        }
        if let Some(index) = outcome.remove {
            self.player.remove_from_queue(index);
        }
        if outcome.clear {
            self.player.clear_queue();
        }
        if outcome.close {
            self.config.window.queue_panel_open = false;
            self.touch();
        }
    }

    /// One of the five library sections, or the tracks of whatever is open.
    fn library_view(&mut self, ui: &mut Ui) {
        let m = self.theme.metrics;

        self.search_row(ui);
        ui.add_space(m.space(1.0));

        // A drill-down and an active search both show a track list; the search
        // wins, because typing should always take you out of wherever you are.
        if self.library.is_searching() {
            self.search_results(ui);
            return;
        }

        if self.library.focus().is_some() {
            self.focused_tracks(ui);
            return;
        }

        match self.library.emptiness(self.view) {
            Emptiness::NoLibrary => self.no_library_state(ui),
            Emptiness::NothingHere => {
                widgets::empty_state(
                    ui,
                    &self.theme,
                    self.view.icon(),
                    self.view.empty_title(),
                    "Nothing in your library carries this information yet.",
                );
            }
            _ => match self.view {
                View::Songs => self.songs_view(ui),
                View::Artists => self.artists_view(ui),
                View::Albums => self.albums_view(ui),
                View::Genres => self.genres_view(ui),
                View::Folders => self.folders_view(ui),
                _ => {}
            },
        }
    }

    /// Search box and the rescan control, above every library view.
    fn search_row(&mut self, ui: &mut Ui) {
        let m = self.theme.metrics;
        let p = self.theme.palette;

        ui.horizontal(|ui| {
            let field_width = (ui.available_width() * 0.45).clamp(m.space(16.0), m.space(40.0));

            crate::widgets::icons::draw(
                ui.painter(),
                Icon::Search,
                Rect::from_min_size(
                    ui.cursor().min + Vec2::new(m.space(0.75), m.space(0.6)),
                    Vec2::splat(m.space(1.75)),
                ),
                col_alpha(p.text_muted, 0.8),
                1.5,
            );
            ui.add_space(m.space(3.0));

            let response = ui.add_sized(
                Vec2::new(field_width, m.space(3.0)),
                egui::TextEdit::singleline(&mut self.search)
                    .hint_text("Search titles, artists, albums"),
            );

            if std::mem::take(&mut self.focus_search) {
                response.request_focus();
            }

            if response.changed() {
                self.library.set_search(self.search.clone());
            }

            // Escape clears rather than only unfocusing, which is what people
            // expect from a filter box.
            if response.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.search.clear();
                self.library.clear_search();
            }

            if !self.search.is_empty()
                && widgets::icon_button_labelled(
                    ui,
                    &self.theme,
                    Icon::Close,
                    m.space(2.5),
                    false,
                    "Clear search",
                )
                .clicked()
            {
                self.search.clear();
                self.library.clear_search();
            }

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let scanning = self.library.is_scanning();
                if widgets::icon_button_labelled(
                    ui,
                    &self.theme,
                    Icon::Plus,
                    m.space(3.0),
                    scanning,
                    if scanning {
                        "Scanning..."
                    } else {
                        "Rescan your folders"
                    },
                )
                .clicked()
                {
                    if scanning {
                        self.library.cancel_scan();
                    } else {
                        let config = self.config.clone();
                        self.library.start_scan(&config);
                    }
                }

                if self.view == View::Songs {
                    self.sort_control(ui);
                }
            });
        });
    }

    /// Sort picker for the track list: what to sort by, and which way.
    ///
    /// Two controls rather than one. The single combo that reversed itself
    /// when you re-picked the current key hid the direction inside a gesture
    /// nobody would guess, and gave no way to flip the order without opening
    /// a menu. Splitting them makes both jobs one click, and lets the
    /// direction button say what it will actually do.
    fn sort_control(&mut self, ui: &mut Ui) {
        let m = self.theme.metrics;
        let p = self.theme.palette;
        let current = self.library.order();
        let descending = self.library.descending();

        // Direction first: in a right-to-left toolbar it lands to the right of
        // the key it modifies, which is the order it reads in.
        let (icon, hint) = if descending {
            (Icon::SortDescending, current.direction_label(true))
        } else {
            (Icon::SortAscending, current.direction_label(false))
        };

        if widgets::icon_button_labelled(ui, &self.theme, icon, m.space(3.0), false, hint).clicked()
        {
            self.library.set_order(current, !descending);
            self.config.library.sort_descending = !descending;
            self.touch();
        }

        ui.add_space(m.space(0.5));

        egui::ComboBox::from_id_salt("sort")
            .selected_text(RichText::new(current.short_label()).color(col(p.text_secondary)))
            .show_ui(ui, |ui| {
                // `TrackNumber` is absent on purpose: it only means anything
                // inside one album, where the view picks it regardless.
                for order in Order::ALL {
                    if order == Order::TrackNumber {
                        continue;
                    }

                    if ui
                        .selectable_label(current == order, order.short_label())
                        .on_hover_text(order.direction_label(descending))
                        .clicked()
                    {
                        // The direction is kept across a change of key. Picking
                        // "Plays" while sorted Z-to-A should not silently jump
                        // to fewest-played.
                        self.library.set_order(order, descending);
                        if let Some(key) = order.as_sort_key() {
                            self.config.library.default_sort = key;
                            self.touch();
                        }
                    }
                }
            });
    }

    /// Shown when there is no library at all.
    fn no_library_state(&mut self, ui: &mut Ui) {
        let m = self.theme.metrics;
        let scanning = self.library.is_scanning();

        if scanning {
            ui.vertical_centered(|ui| {
                ui.add_space(m.space(6.0));
                let phase = self
                    .library
                    .scan_progress()
                    .map_or("Looking for music...", |p| p.phase().label());
                ui.label(
                    RichText::new(phase)
                        .text_style(TextStyle::Body)
                        .color(col(self.theme.palette.text_secondary)),
                );
            });
            return;
        }

        widgets::empty_state(
            ui,
            &self.theme,
            Icon::Songs,
            "No music yet",
            "Add a folder and Resonance will find everything it can play inside it.",
        );
        ui.vertical_centered(|ui| {
            ui.add_space(m.space(2.0));
            if widgets::accent_button(ui, &self.theme, "Add a folder").clicked() {
                self.pick_folder();
            }
        });
    }

    /// The first-run welcome, in place of the usual content.
    fn welcome_view(&mut self, ui: &mut Ui) {
        let outcome = views::welcome::show(ui, &self.theme, self.library.is_scanning());

        if outcome.add_folder {
            self.pick_folder();
        }
        if outcome.dismiss {
            self.welcome = false;
        }
    }

    fn songs_view(&mut self, ui: &mut Ui) {
        let stats = self.library.stats();
        let shown = self.library.tracks().len();
        let detail =
            views::songs::detail_line(shown, stats.tracks, stats.unplayable, stats.untagged);

        browse::section_header(ui, &self.theme, "Songs", &detail, |_| {});
        ui.add_space(self.theme.metrics.space(1.0));

        self.track_list(ui);
    }

    /// The track list plus everything the user can do from a row.
    fn track_list(&mut self, ui: &mut Ui) {
        let current = self.player.current_path().map(std::path::Path::to_path_buf);

        let outcome = views::songs::show(
            ui,
            &self.theme,
            &mut views::songs::Covers {
                textures: &mut self.artwork,
                cache: self.library.art(),
            },
            self.library.tracks(),
            current.as_deref(),
            views::songs::Options {
                tag_editing: self.config.library.allow_tag_editing,
                single_click: self.config.playback.play_on_single_click,
            },
        );

        if let Some(id) = outcome.edit_tags {
            self.open_tag_editor(id);
        }

        if let Some(index) = outcome.play {
            self.play_visible(index);
        }
        if let Some(index) = outcome.play_next {
            let paths = self.visible_path(index).into_iter().collect();
            self.player.play_next(paths);
        }
        if let Some(index) = outcome.enqueue {
            let paths = self.visible_path(index).into_iter().collect();
            self.player.enqueue(paths);
        }
        if let Some(id) = outcome.open_artist {
            let name = self
                .library
                .tracks()
                .iter()
                .find(|t| t.artist_id == Some(id))
                .map_or_else(String::new, |t| t.artist.clone());
            self.library.open(Focus::Artist { id, name });
            self.view = View::Artists;
        }
        if let Some(id) = outcome.open_album
            && let Some(track) = self
                .library
                .tracks()
                .iter()
                .find(|t| t.album_id == Some(id))
        {
            let focus = Focus::Album {
                id,
                title: track.album.clone(),
                artist: track.artist.clone(),
            };
            self.library.open(focus);
            self.view = View::Albums;
        }
        if outcome.add_folder {
            self.pick_folder();
        }
    }

    fn search_results(&mut self, ui: &mut Ui) {
        let count = self.library.tracks().len();

        if count == 0 {
            widgets::empty_state(
                ui,
                &self.theme,
                Icon::Search,
                "Nothing matched",
                "Try fewer words, or part of a title, artist or album.",
            );
            return;
        }

        // Say when the list was cut short. Searching a large library for a
        // common word matches far more than anyone will scroll, so the results
        // are capped — but a count that silently stops at the cap would be the
        // interface lying about how much it found.
        let detail = match (count, self.library.search_was_capped()) {
            (1, _) => "1 result".to_owned(),
            (count, true) => format!("first {count} results — keep typing to narrow"),
            (count, false) => format!("{count} results"),
        };
        browse::section_header(ui, &self.theme, "Search", &detail, |_| {});
        ui.add_space(self.theme.metrics.space(1.0));

        self.track_list(ui);
    }

    /// The tracks of whatever group is open.
    fn focused_tracks(&mut self, ui: &mut Ui) {
        let m = self.theme.metrics;

        let (title, subtitle) = {
            let Some(focus) = self.library.focus() else {
                return;
            };
            (
                focus.title().to_owned(),
                focus.subtitle().map(str::to_owned),
            )
        };

        if browse::focus_header(ui, &self.theme, &title, subtitle.as_deref()) {
            self.library.close_focus();
            return;
        }

        ui.add_space(m.space(0.5));

        let tracks = self.library.tracks();
        let total: Duration = tracks.iter().filter_map(|t| t.duration).sum();
        let detail = format!(
            "{} · {}",
            browse::track_count_label(tracks.len() as u32),
            browse::duration_label(total)
        );

        ui.horizontal(|ui| {
            if widgets::accent_button(ui, &self.theme, "Play all").clicked() {
                self.play_visible(0);
            }
            ui.add_space(m.space(1.0));
            ui.label(
                RichText::new(detail)
                    .text_style(TextStyle::Body)
                    .color(col(self.theme.palette.text_muted)),
            );
        });

        ui.add_space(m.space(1.0));
        self.track_list(ui);
    }

    fn artists_view(&mut self, ui: &mut Ui) {
        let count = self.library.artists().len();
        browse::section_header(
            ui,
            &self.theme,
            "Artists",
            &format!("{count} artists"),
            |_| {},
        );
        ui.add_space(self.theme.metrics.space(1.0));
        self.artist_grid(ui);
    }

    fn artist_grid(&mut self, ui: &mut Ui) {
        let entries: Vec<(i64, String, String, Option<String>)> = self
            .library
            .artists()
            .iter()
            .map(|artist| {
                (
                    artist.id,
                    artist.name.clone(),
                    if artist.album_count > 0 {
                        format!(
                            "{} · {} album{}",
                            browse::track_count_label(artist.track_count),
                            artist.album_count,
                            if artist.album_count == 1 { "" } else { "s" }
                        )
                    } else {
                        browse::track_count_label(artist.track_count)
                    },
                    artist.art_id.clone(),
                )
            })
            .collect();

        let outcome = browse::grid(
            ui,
            &self.theme,
            &mut self.artwork,
            self.library.art(),
            entries.len(),
            |index| {
                let (_, name, subtitle, art) = &entries[index];
                browse::Card {
                    title: name,
                    subtitle: subtitle.clone(),
                    art_id: art.as_deref(),
                    round: true,
                    fallback: Icon::Artists,
                }
            },
        );

        if let Some(index) = outcome.open.or(outcome.play) {
            let (id, name, _, _) = &entries[index];
            self.library.open(Focus::Artist {
                id: *id,
                name: name.clone(),
            });
        }
    }

    fn albums_view(&mut self, ui: &mut Ui) {
        let m = self.theme.metrics;
        let count = self.library.albums().len();
        let total = self.library.stats().albums;
        let hidden = total.saturating_sub(count as u32);

        let mut toggle = self.library.hide_single_albums();
        let detail = if hidden > 0 {
            format!("{count} albums · {hidden} with a single track hidden")
        } else {
            format!("{count} albums")
        };

        browse::section_header(ui, &self.theme, "Albums", &detail, |ui| {
            // Downloaded collections are full of one-track "albums" from junk
            // tags; hiding them is the default, but never silently - the count
            // above says how many are hidden.
            ui.checkbox(&mut toggle, "Hide singles");
        });
        self.library.set_hide_single_albums(toggle);
        ui.add_space(m.space(1.0));

        let entries: Vec<(i64, String, String, String, Option<String>)> = self
            .library
            .albums()
            .iter()
            .map(|album| {
                let mut subtitle = album.artist.clone();
                if let Some(year) = album.year {
                    subtitle = format!("{subtitle} · {year}");
                }
                (
                    album.id,
                    album.title.clone(),
                    album.artist.clone(),
                    subtitle,
                    album.art_id.clone(),
                )
            })
            .collect();

        let outcome = browse::grid(
            ui,
            &self.theme,
            &mut self.artwork,
            self.library.art(),
            entries.len(),
            |index| {
                let (_, title, _, subtitle, art) = &entries[index];
                browse::Card {
                    title,
                    subtitle: subtitle.clone(),
                    art_id: art.as_deref(),
                    round: false,
                    fallback: Icon::Albums,
                }
            },
        );

        if let Some(index) = outcome.open.or(outcome.play) {
            let (id, title, artist, _, _) = &entries[index];
            self.library.open(Focus::Album {
                id: *id,
                title: title.clone(),
                artist: artist.clone(),
            });
        }
    }

    fn genres_view(&mut self, ui: &mut Ui) {
        let count = self.library.genres().len();
        browse::section_header(
            ui,
            &self.theme,
            "Genres",
            &format!("{count} genres"),
            |_| {},
        );
        ui.add_space(self.theme.metrics.space(1.0));

        let entries: Vec<(i64, String, String)> = self
            .library
            .genres()
            .iter()
            .map(|genre| {
                (
                    genre.id,
                    genre.name.clone(),
                    browse::track_count_label(genre.track_count),
                )
            })
            .collect();

        let outcome = browse::list(ui, &self.theme, entries.len(), |index| {
            let (_, name, count) = &entries[index];
            (name.clone(), count.clone())
        });

        if let Some(index) = outcome.open {
            let (id, name, _) = &entries[index];
            self.library.open(Focus::Genre {
                id: *id,
                name: name.clone(),
            });
        }
    }

    fn folders_view(&mut self, ui: &mut Ui) {
        let count = self.library.folders().len();
        browse::section_header(
            ui,
            &self.theme,
            "Folders",
            &format!("{count} folders"),
            |_| {},
        );
        ui.add_space(self.theme.metrics.space(1.0));

        let entries: Vec<(std::path::PathBuf, String, String)> = self
            .library
            .folders()
            .iter()
            .map(|folder| {
                (
                    folder.path.clone(),
                    folder.name.clone(),
                    format!(
                        "{} · {}",
                        browse::track_count_label(folder.track_count),
                        browse::duration_label(folder.total_duration)
                    ),
                )
            })
            .collect();

        let outcome = browse::list(ui, &self.theme, entries.len(), |index| {
            let (_, name, detail) = &entries[index];
            (name.clone(), detail.clone())
        });

        if let Some(index) = outcome.open {
            let (path, name, _) = &entries[index];
            self.library.open(Focus::Folder {
                path: path.clone(),
                name: name.clone(),
            });
        }
    }

    fn equalizer_view(&mut self, ui: &mut Ui) {
        let limiting = self.player.is_limiting();
        let outcome = views::equalizer::show(ui, &self.theme, &mut self.config.equalizer, limiting);

        if outcome.changed {
            // Straight to the engine, then to disk on the usual debounce. The
            // sound has to follow the slider immediately; the file does not.
            self.config.validate();
            self.player.apply_dsp_settings(&self.config);
            self.touch();
        }
    }

    /// Keep playing when the queue runs out, if the user asked for that.
    fn top_up_radio(&mut self) {
        if !self.player.wants_radio() {
            return;
        }

        if !self.config.playback.auto_radio {
            self.player.cancel_radio();
            return;
        }

        let Some(seed_path) = self.player.radio_seed().map(std::path::Path::to_path_buf) else {
            self.player.cancel_radio();
            return;
        };

        let Some(track) = self.library.track_at_path(&seed_path) else {
            // Played from outside the library, so there is nothing to reason
            // from. Asking once and stopping beats retrying every frame.
            self.player.cancel_radio();
            return;
        };

        let count = self.config.playback.radio_batch.clamp(1, 100);

        let next =
            match self
                .library
                .library()
                .radio(mp_core::library::Seed::Track(track.id), &[], count)
            {
                Ok(tracks) => tracks,
                Err(err) => {
                    tracing::error!("auto-radio could not choose anything: {err:#}");
                    self.player.cancel_radio();
                    return;
                }
            };

        if next.is_empty() {
            self.player.cancel_radio();
            return;
        }

        self.player
            .notice(format!("Radio: {} more tracks", next.len()), false);
        self.player
            .continue_with(next.into_iter().map(|track| track.path).collect());
    }

    /// Start or stop the analysis pass to match the setting.
    fn tend_analysis(&mut self) {
        let wanted = self.config.library.analyze_audio_features;
        let running = self.analysis.as_ref().is_some_and(AnalysisJob::is_running);

        if wanted && self.analysis.is_none() {
            self.analysis = AnalysisJob::start(self.library.library());
        } else if !wanted && running {
            // Dropping the handle cancels the thread.
            self.analysis = None;
        }

        if let Some(job) = &self.analysis
            && let Some(err) = job.take_error()
        {
            self.player
                .notice(format!("Sound analysis stopped: {err}"), true);
        }
    }

    /// Open the network activity log in the system file manager.
    ///
    /// A log the user is told about but cannot get to is not much of a
    /// disclosure, and the path alone means finding `%APPDATA%` by hand.
    fn show_activity_log(&mut self) {
        let Some(path) = self.activity.path() else {
            self.player
                .notice("There is no activity log on disk.".to_owned(), true);
            return;
        };

        // Selected in its folder rather than opened, because what opens a
        // `.log` file varies by machine and a folder always works.
        #[cfg(windows)]
        let launched = std::process::Command::new("explorer")
            .arg(format!("/select,{}", path.display()))
            .spawn();

        #[cfg(not(windows))]
        let launched = std::process::Command::new("xdg-open")
            .arg(path.parent().unwrap_or(&path))
            .spawn();

        match launched {
            // Explorer reports failure through its exit code even when it
            // worked, so the spawn succeeding is as much as can be checked.
            Ok(_) => {}
            Err(err) => {
                tracing::warn!("could not open the activity log: {err}");
                self.player
                    .notice(format!("The log is at {}", path.display()), false);
            }
        }
    }

    /// Forget every lyric fetched so far.
    ///
    /// Only the cache on disk. Lyrics already on screen stay until the track
    /// changes, and the session memo in the fetcher is dropped with it so the
    /// next lookup genuinely asks again.
    fn clear_lyrics_cache(&mut self) {
        let cache =
            mp_net::Cache::new(self.paths.cache_dir().join(mp_net::lyrics::CACHE_NAMESPACE));

        match cache.clear() {
            Ok(()) => {
                // Dropped so the in-memory answers go too; it restarts on the
                // next frame if the setting is still on.
                self.lyrics_job = None;
                self.player
                    .notice("Cached lyrics cleared.".to_owned(), false);
            }
            Err(err) => {
                tracing::warn!("could not clear the lyrics cache: {err:#}");
                self.player
                    .notice("Could not clear the cached lyrics.".to_owned(), true);
            }
        }
    }

    /// Start or stop the lyrics fetcher as the setting changes, and collect
    /// anything it has finished.
    ///
    /// The worker exists only while the setting is on. Turning it off drops
    /// the handle, which closes the channel and ends the thread — so "off"
    /// means there is no code running that could make a request, rather than
    /// a flag being checked somewhere.
    fn tend_lyrics(&mut self, ctx: &egui::Context) {
        match (self.config.privacy.online_lyrics, self.lyrics_job.is_some()) {
            (true, false) => {
                self.lyrics_job = LyricsJob::start(
                    self.paths.cache_dir().to_path_buf(),
                    Arc::clone(&self.activity),
                    ctx.clone(),
                );
            }
            (false, true) => self.lyrics_job = None,
            _ => {}
        }

        let matching = if self.config.privacy.online_lyrics_any_release {
            mp_net::lyrics::Match::AnyRelease
        } else {
            mp_net::lyrics::Match::Exact
        };

        if let Some(job) = &mut self.lyrics_job {
            // Pushed every frame rather than watched for changes: it is a
            // comparison of two enum values, and the job ignores a repeat.
            job.set_matching(matching);

            // An answer that landed while the window was idle produced no
            // frame of its own; the worker asks for one, this picks it up.
            if job.poll() {
                ctx.request_repaint();
            }
        }
    }

    fn playlists_view(&mut self, ui: &mut Ui) {
        let outcome = views::playlists::show(
            ui,
            &self.theme,
            &mut self.playlists,
            &mut self.playlist_editing,
        );

        self.apply_playlist_outcome(outcome);

        if let Some(message) = self.playlists.take_error() {
            self.player.notice(message, true);
        }
    }

    fn apply_playlist_outcome(&mut self, outcome: views::playlists::Outcome) {
        // Split out from the view so every write to the index happens in one
        // place, after drawing, rather than scattered through the layout.
        let library = self.library.library_mut();

        if outcome.create
            && let Some(id) = self.playlists.create(library, "New playlist")
        {
            self.playlists.open(library, id);
        }
        if outcome.create_smart
            && let Some(id) = self.playlists.create_smart(library, "New smart playlist")
        {
            self.playlists.open(library, id);
        }
        if let Some(id) = outcome.open {
            self.playlists.open(library, id);
        }
        if outcome.close {
            self.playlists.close();
        }
        if let Some(id) = outcome.delete {
            self.playlists.delete(library, id);
        }
        if let Some((id, name)) = outcome.rename {
            self.playlists.rename(library, id, &name);
        }

        let open = self.playlists.open_playlist().map(|playlist| playlist.id);

        if let Some(id) = open {
            if let Some(position) = outcome.remove_at {
                self.playlists.remove_at(library, id, position);
            }
            if let Some((from, to)) = outcome.move_item {
                self.playlists.move_item(library, id, from, to);
            }
        }

        if let Some(tool) = outcome.set_tool {
            self.playlists.set_tool(library, tool);
        }
        if let Some(query) = outcome.set_query {
            self.playlists.set_query(library, query);
        }
        if let Some(folder) = outcome.set_folder {
            self.playlists.set_folder_filter(library, folder);
        }
        if let Some(track) = outcome.toggle_pick {
            self.playlists.toggle_pick(track);
        }
        if outcome.pick_all {
            self.playlists.pick_all();
        }
        if outcome.clear_picks {
            self.playlists.clear_picks();
        }
        if outcome.add_picked {
            self.playlists.add_picked(library);
        }
        if outcome.refresh_suggestions {
            self.playlists.refresh_suggestions(library);
        }
        if outcome.apply_rules {
            self.playlists.apply_rules(library);
        }

        // File dialogs are modal and block the frame, so they run after every
        // in-memory edit above has already been applied.
        if let Some(id) = outcome.export {
            self.export_playlist(id);
        }
        if outcome.import {
            self.import_playlist();
        }

        // Playing has to come last: it reads the track list, which every edit
        // above may have changed.
        if let Some(index) = outcome.play_from {
            self.playlists.update(self.library.library());
            let paths = self.playlists.track_paths();
            if !paths.is_empty() {
                let start = index.min(paths.len() - 1);
                self.player.play(paths, start);
            }
        }
    }

    fn visualizer_view(&mut self, ui: &mut Ui) {
        let playing = self.player.is_playing();
        let dt = self.viz_dt;

        let outcome = views::visualizer::show(
            ui,
            &self.theme,
            &mut self.visualizers,
            &mut self.config.visualizer,
            playing,
            dt,
        );

        if outcome.changed {
            self.config.validate();
            self.touch();
        }
    }

    /// Keep the system media session in step, and act on its buttons.
    ///
    /// Both halves are cheap when nothing has changed: the session compares
    /// against what it last displayed, and an empty command queue is a lock
    /// and a `Vec::new`.
    fn tend_media_session(&mut self) {
        let now = self.player.now_playing.as_ref().map(|now| NowPlayingInfo {
            title: now.title.clone(),
            artist: now.artist.clone(),
            album: now.album.clone(),
            // The card size rather than the full one: the flyout thumbnail is
            // small, and handing the shell an 800 px JPEG to draw at 80 px is
            // work for nothing.
            artwork: now
                .art_id
                .as_deref()
                .and_then(|id| self.library.library().art_path(id, ArtSize::Card)),
        });

        self.media.set_now_playing(now.as_ref());

        self.media
            .set_state(match (&self.player.now_playing, self.player.is_playing()) {
                (None, _) => PlaybackState::Stopped,
                (Some(_), true) => PlaybackState::Playing,
                (Some(_), false) => PlaybackState::Paused,
            });

        for command in self.media.take_commands() {
            match command {
                MediaCommand::Play if !self.player.is_playing() => self.toggle_play_pause(),
                MediaCommand::Pause | MediaCommand::Stop if self.player.is_playing() => {
                    self.toggle_play_pause();
                }
                MediaCommand::TogglePlayPause => self.toggle_play_pause(),
                MediaCommand::Next => self.player.next(),
                MediaCommand::Previous => self.player.previous(),
                // Already in the state the button asked for.
                MediaCommand::Play | MediaCommand::Pause | MediaCommand::Stop => {}
            }
        }
    }

    /// Paint a panel's themed backdrop, before its content is drawn over it.
    ///
    /// Called as the first thing inside a panel, so everything the panel draws
    /// afterwards lands on top. Doing it after the fact would need a second
    /// layer and an ordering argument; painting first needs neither.
    ///
    /// Falls back to the plain fill whenever the chosen source has nothing to
    /// give — nothing playing, no cover, no audio reaching the visualiser — so
    /// the setting never leaves a panel looking broken.
    fn paint_backdrop(&mut self, ui: &mut Ui, rect: Rect, style: SurfaceStyle, base: Rgb) {
        let intensity = self.config.appearance.background_intensity;

        let plain = |ui: &Ui| {
            ui.painter()
                .rect_filled(rect, egui::CornerRadius::ZERO, col(base));
        };

        match style {
            SurfaceStyle::Solid => plain(ui),

            SurfaceStyle::AlbumArt => {
                // The 64 px thumbnail on purpose: stretched this far it is a
                // free blur, and it is already in the texture cache.
                let texture = self
                    .player
                    .now_playing
                    .as_ref()
                    .and_then(|now| now.art_id.as_deref())
                    .and_then(|id| {
                        self.artwork
                            .get(ui.ctx(), self.library.art(), id, ArtSize::Thumb)
                    });

                match texture {
                    Some(texture) => surface::album_art(ui, rect, base, &texture, intensity),
                    None => plain(ui),
                }
            }

            SurfaceStyle::Visualizer => {
                plain(ui);

                if self.config.visualizer.kind == mp_core::config::VisualizerKind::None
                    || !self.visualizers.is_connected()
                {
                    return;
                }

                let band = surface::visualizer_band(rect);
                let painter = ui.painter().with_clip_rect(band);
                self.visualizers.draw(
                    &painter,
                    band,
                    &self.theme,
                    &self.config.visualizer,
                    self.viz_dt,
                );

                surface::scrim(ui, rect, base, intensity);
            }
        }
    }

    /// The rectangle a panel's frame occupies, from the ui inside it.
    ///
    /// A panel's inner ui starts after its margin, so the backdrop has to be
    /// grown back out or the margin stays a band of flat colour around it.
    fn panel_rect(ui: &Ui, margin: Vec2) -> Rect {
        ui.max_rect().expand2(margin)
    }

    // -----------------------------------------------------------------------
    // Keyboard
    // -----------------------------------------------------------------------

    /// Read the keyboard and act on whatever was bound.
    ///
    /// Bare letters are only honoured when nothing has keyboard focus. Without
    /// that check, typing "smart" into the search box would mute the audio,
    /// shuffle the queue and open the queue panel — the price of bindings
    /// discoverable enough to be worth having.
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        // Two different questions, and the shortcut table needs both. `egui`
        // reports "something has focus" for any widget at all, including a
        // button, which is too broad to use as "the user is typing".
        let text_focused = ctx.text_edit_focused();
        let widget_focused = ctx.egui_wants_keyboard_input();

        let pressed: Vec<Action> = ctx.input(|input| {
            input
                .events
                .iter()
                .filter_map(|event| match event {
                    egui::Event::Key {
                        key,
                        pressed: true,
                        modifiers,
                        ..
                    } => {
                        let action = shortcuts::action_for(*key, *modifiers)?;
                        shortcuts::should_dispatch(*key, action, text_focused, widget_focused)
                            .then_some(action)
                    }
                    _ => None,
                })
                .collect()
        });

        for action in pressed {
            self.perform(action, ctx);
        }
    }

    fn perform(&mut self, action: Action, ctx: &egui::Context) {
        match action {
            Action::PlayPause => self.toggle_play_pause(),
            Action::Next => self.player.next(),
            Action::Previous => self.player.previous(),

            Action::SeekForward => self.nudge_position(shortcuts::SEEK_STEP),
            Action::SeekBack => self.nudge_position(-shortcuts::SEEK_STEP),

            Action::VolumeUp => self.nudge_volume(shortcuts::VOLUME_STEP),
            Action::VolumeDown => self.nudge_volume(-shortcuts::VOLUME_STEP),
            Action::ToggleMute => {
                let muted = !self.config.playback.muted;
                self.config.playback.muted = muted;
                self.player.set_muted(muted);
                self.touch();
            }

            Action::ToggleShuffle => self.cycle_shuffle(),
            Action::CycleRepeat => self.cycle_repeat(),
            Action::ToggleQueue => {
                self.config.window.queue_panel_open = !self.config.window.queue_panel_open;
                self.touch();
            }

            Action::ToggleFullScreen => {
                let playing = self.player.current_path().map(std::path::Path::to_path_buf);
                self.immersive.toggle(playing.as_deref());
            }

            Action::FocusSearch => {
                // Searching from inside the full-screen view means leaving it;
                // there is no search box in there to focus.
                self.immersive.close();
                if !matches!(
                    self.view,
                    View::Songs | View::Artists | View::Albums | View::Genres | View::Folders
                ) {
                    self.go_to(View::Songs);
                }
                self.focus_search = true;
            }

            // Ordered by what is most enclosing, so one press backs out of one
            // thing rather than everything at once.
            Action::Escape => {
                if self.immersive.is_open() {
                    self.immersive.close();
                } else if !self.search.is_empty() {
                    self.search.clear();
                    self.library.clear_search();
                    ctx.memory_mut(|memory| memory.stop_text_input());
                } else if self.library.focus().is_some() {
                    self.library.close_focus();
                }
            }
        }
    }

    /// Seek by a number of seconds, clamped to the track.
    fn nudge_position(&mut self, seconds: f64) {
        let Some(duration) = self.player.duration_secs().filter(|d| *d > 0.0) else {
            return;
        };

        let target = (self.player.position_secs() + seconds).clamp(0.0, duration);
        self.player.seek_fraction((target / duration) as f32);
    }

    fn nudge_volume(&mut self, delta: f32) {
        let volume = (self.config.playback.volume + delta).clamp(0.0, 1.0);
        self.config.playback.volume = volume;
        self.player.set_volume(volume);

        // Nudging the volume up from silence obviously means to hear it.
        if delta > 0.0 && self.config.playback.muted {
            self.config.playback.muted = false;
            self.player.set_muted(false);
        }

        self.touch();
    }

    /// Remember how the window is sized, so it opens the same way next time.
    ///
    /// Two rules make this behave the way people expect.
    ///
    /// The restored size is only recorded while the window is *not* maximised.
    /// Recording it while maximised would overwrite the size with the screen's
    /// own, and un-maximising would then "restore" to full screen — the window
    /// would appear to have lost its old size forever.
    ///
    /// A minimised window reports a size, and it is nonsense. Anything smaller
    /// than the minimum the window can actually be dragged to is ignored
    /// rather than saved.
    fn remember_geometry(&mut self, ctx: &egui::Context) {
        let (size, maximized, minimized) = ctx.input(|input| {
            let viewport = input.viewport();
            (
                viewport.inner_rect.map(|rect| rect.size()),
                viewport.maximized.unwrap_or(false),
                viewport.minimized.unwrap_or(false),
            )
        });

        if minimized {
            return;
        }

        if maximized != self.config.window.maximized {
            self.config.window.maximized = maximized;
            self.touch();
        }

        if maximized {
            return;
        }

        let Some(size) = size else {
            return;
        };

        if size.x < MIN_WINDOW_SIZE[0] || size.y < MIN_WINDOW_SIZE[1] {
            return;
        }

        // A whole point of movement, so a sub-pixel wobble during a drag does
        // not mark the config dirty sixty times a second.
        let moved = (size.x - self.config.window.width).abs() >= 1.0
            || (size.y - self.config.window.height).abs() >= 1.0;

        if moved {
            self.config.window.width = size.x;
            self.config.window.height = size.y;
            self.touch();
        }
    }

    // -----------------------------------------------------------------------
    // Settings bundles
    // -----------------------------------------------------------------------

    /// Write settings, playlists and (optionally) history to one file.
    fn export_bundle(&mut self) {
        let default_name = format!(
            "{}-settings.{}",
            mp_core::APP_NAME.to_lowercase(),
            mp_core::bundle::EXTENSION
        );

        let Some(path) = rfd::FileDialog::new()
            .set_title("Export settings bundle")
            .add_filter("Resonance bundle", &[mp_core::bundle::EXTENSION])
            .set_file_name(default_name)
            .save_file()
        else {
            return;
        };

        // Written from the config in memory, not the file on disk, so an
        // unsaved change made a moment ago is in the bundle.
        self.config.validate();

        let options = mp_core::bundle::ExportOptions {
            include_playlists: true,
            include_statistics: self.config.privacy.bundle_statistics,
        };

        match mp_core::bundle::export(&path, &self.config, self.library.library(), options) {
            Ok(manifest) => {
                tracing::info!(
                    "exported a bundle with {} playlists to {}",
                    manifest.playlists,
                    path.display()
                );
                self.player.notice(
                    format!(
                        "Exported {} playlists and your settings to {}",
                        manifest.playlists,
                        file_label(&path)
                    ),
                    false,
                );
            }
            Err(err) => {
                tracing::error!("could not export a bundle: {err:#}");
                self.player.notice(format!("Could not export: {err}"), true);
            }
        }
    }

    /// Read a bundle back in.
    fn import_bundle(&mut self, mode: mp_core::bundle::Mode) {
        let Some(path) = rfd::FileDialog::new()
            .set_title("Import settings bundle")
            .add_filter("Resonance bundle", &[mp_core::bundle::EXTENSION])
            .pick_file()
        else {
            return;
        };

        // Checked before anything is applied, so a wrong file is reported
        // rather than half-imported.
        if let Err(err) = mp_core::bundle::inspect(&path) {
            tracing::error!("not a usable bundle: {err:#}");
            self.player.notice(format!("{err}"), true);
            return;
        }

        let result = {
            let library = self.library.library_mut();
            mp_core::bundle::import(&path, &mut self.config, library, mode)
        };

        match result {
            Ok(summary) => {
                tracing::info!(
                    "imported a bundle: {} added, {} replaced, {} skipped, {} tracks missing",
                    summary.playlists_added,
                    summary.playlists_replaced,
                    summary.playlists_skipped,
                    summary.tracks_missing.len()
                );
                for missing in &summary.tracks_missing {
                    tracing::info!("  not in the library: {}", missing.display());
                }

                self.player
                    .notice(summary.summary(), !summary.tracks_missing.is_empty());

                if summary.settings_applied {
                    // Everything downstream of the config has to be rebuilt:
                    // the palette, the DSP chain, and the scan roots.
                    self.config.validate();
                    self.player.apply_dsp_settings(&self.config);
                    self.touch();

                    let config = self.config.clone();
                    self.library.start_scan(&config);
                }

                self.playlists.invalidate();
                let library = self.library.library();
                self.playlists.update(library);
            }
            Err(err) => {
                tracing::error!("could not import a bundle: {err:#}");
                self.player.notice(format!("Could not import: {err}"), true);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Tag editing
    // -----------------------------------------------------------------------

    /// Open the editor on a track, seeded from the file rather than the index.
    ///
    /// Read from the file every time, because the index is a cache that can be
    /// out of date and the editor must show what is actually there — otherwise
    /// a stale row would be written back over a newer tag.
    fn open_tag_editor(&mut self, id: mp_core::library::TrackId) {
        if !self.config.library.allow_tag_editing {
            return;
        }

        let Ok(Some(track)) = self.library.library().track(id) else {
            self.player
                .notice("That track is no longer in the library".into(), true);
            return;
        };

        match mp_core::library::tags::read(&track.path) {
            Ok(values) => {
                self.tag_editor
                    .open(id, &track.title, &track.path.to_string_lossy(), &values);
            }
            Err(err) => {
                tracing::error!("could not read tags from {}: {err:#}", track.path.display());
                self.player
                    .notice(format!("Could not read that file: {err}"), true);
            }
        }
    }

    /// Draw the editor and act on it.
    fn tag_editor_dialog(&mut self, ctx: &egui::Context) {
        if !self.tag_editor.is_open() {
            return;
        }

        // The setting can be turned off while the dialog is open. Closing it
        // rather than letting the open window keep write access is the only
        // reading of that switch that means anything.
        if !self.config.library.allow_tag_editing {
            self.tag_editor.close();
            return;
        }

        let outcome = views::tag_editor::show(ctx, &self.theme, &mut self.tag_editor);

        if outcome.close {
            self.tag_editor.close();
            return;
        }
        if outcome.back {
            self.tag_editor.back();
        }
        if outcome.reset {
            self.tag_editor.reset();
        }

        let Some(id) = self.tag_editor.track() else {
            return;
        };

        if outcome.review {
            let edit = self.tag_editor.edit();
            match self.library.library().preview_tag_edit(id, &edit) {
                Ok(changes) => self.tag_editor.confirm_with(changes),
                Err(err) => self.tag_editor.fail(format!("{err}")),
            }
        }

        if outcome.apply {
            let edit = self.tag_editor.edit();
            match self.library.library_mut().edit_tags(id, &edit) {
                Ok(Some(record)) => {
                    tracing::info!(
                        "tag edit {} on {}: {}",
                        record.id,
                        record.path.display(),
                        record.summary()
                    );
                    self.player.notice(
                        format!("Saved. {} — undo from Settings.", record.summary()),
                        false,
                    );
                    self.tag_editor.settle();
                    self.tag_editor.close();

                    // The row on screen still shows the old text until the
                    // index catches up, and the file's fingerprint has just
                    // been cleared, so a scan is the cheapest way to make the
                    // list agree with the file.
                    let config = self.config.clone();
                    self.library.start_scan(&config);
                }
                Ok(None) => {
                    self.tag_editor
                        .fail("Nothing changed, so nothing was written");
                }
                Err(err) => {
                    tracing::error!("tag edit failed: {err:#}");
                    self.tag_editor.fail(format!("{err}"));
                }
            }
        }
    }

    /// Put a journalled tag edit back.
    fn undo_tag_edit(&mut self, record: i64) {
        match self.library.library_mut().revert_tag_edit(record) {
            Ok(()) => {
                tracing::info!("reverted tag edit {record}");
                self.player.notice("Edit undone".into(), false);

                // Same reasoning as after a write: the file has changed and the
                // index has not.
                let config = self.config.clone();
                self.library.start_scan(&config);
            }
            Err(err) => {
                tracing::error!("could not revert tag edit {record}: {err:#}");
                self.player.notice(format!("Could not undo: {err}"), true);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Full-screen now playing
    // -----------------------------------------------------------------------

    /// Draw the immersive view in place of the whole shell.
    ///
    /// The title bar stays, because the window is undecorated and without it
    /// there would be no way to move, minimise or close it. Everything else —
    /// nav rail, content, player bar — is gone: the point of the view is that
    /// there is nothing on screen but the record.
    fn immersive_view(&mut self, ui: &mut Ui, dt: f32) {
        crate::window_frame::title_bar(ui, &self.theme, mp_core::APP_TITLE);

        let scene = views::now_playing::Scene {
            theme: &self.theme,
            now: self.player.now_playing.as_ref(),
            artwork: &mut self.artwork,
            art_cache: self.library.art(),
            palette: self.adaptive.palette(),
            visualizers: &mut self.visualizers,
            viz: &self.config.visualizer,
            position: self.player.position_secs(),
            duration: self.player.duration_secs(),
            progress: self.player.progress(),
            playing: self.player.is_playing(),
            scrubbing: self.player.scrubbing,
            dt,
        };

        let outcome = egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(col(self.theme.palette.bg_base)))
            .show(ui, |ui| {
                views::now_playing::show(ui, &mut self.immersive, scene)
            })
            .inner;

        if outcome.close {
            self.immersive.close();
        }
        if outcome.toggle_lyrics {
            self.immersive.toggle_lyrics();
        }
        if outcome.toggle_play {
            self.toggle_play_pause();
        }
        if outcome.next {
            self.player.next();
        }
        if outcome.previous {
            self.player.previous();
        }
        if let Some(fraction) = outcome.seek {
            self.player.seek_fraction(fraction);
        }

        // The window border is the last thing painted in the normal shell too.
        let ctx = ui.ctx();
        crate::window_frame::resize_handles(ctx);
        crate::window_frame::window_border(ctx, &self.theme);
    }

    fn settings_view(&mut self, ui: &mut Ui) {
        let summary = self.fonts.summary();
        let analysis = self.analysis.as_ref().map(AnalysisJob::status);

        // Read per frame rather than cached: the list is short, the query is a
        // bounded index scan, and a cache here would need invalidating from
        // three places for no measurable gain.
        let history = if self.config.library.allow_tag_editing {
            self.library
                .library()
                .tag_history(TAG_HISTORY_SHOWN)
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        let log_path = self.activity.path();

        let outcome = views::settings::show(
            ui,
            &self.theme,
            &mut self.config,
            &summary,
            analysis,
            &history,
            views::settings::Live {
                sleep: self.player.sleep(),
                fades: self.player.fades(),
                network: views::settings::Network {
                    source: &mp_net::source::LRCLIB,
                    entries: self.activity.len(),
                    requests: self.activity.requests_made(),
                    log_path: log_path.as_deref(),
                },
            },
        );

        if outcome.show_activity_log {
            self.show_activity_log();
        }

        if outcome.clear_lyrics_cache {
            self.clear_lyrics_cache();
        }

        if let Some(choice) = outcome.set_sleep {
            self.player.set_sleep(choice);
        }

        if let Some(record) = outcome.undo_tag_edit {
            self.undo_tag_edit(record);
        }

        if outcome.reopen_device {
            // Applied immediately rather than on the next launch: a device
            // picker you have to restart the app to test is not a picker.
            self.player.reopen_device(
                self.config.playback.output_device.clone(),
                self.config.playback.buffer_frames,
            );
        }

        if outcome.export_bundle {
            self.export_bundle();
        }
        if outcome.import_bundle_replace {
            self.import_bundle(mp_core::bundle::Mode::Replace);
        }
        if outcome.import_bundle_merge {
            self.import_bundle(mp_core::bundle::Mode::Merge);
        }

        if outcome.changed.0 {
            // Level correction and the equalizer both live in Settings as well;
            // pushing on any change is cheaper than working out which one moved.
            self.config.validate();
            self.player.apply_dsp_settings(&self.config);
            self.touch();
        }

        if outcome.restyle {
            let ctx = ui.ctx().clone();
            self.restyle(&ctx);
        }

        if let Some(index) = outcome.remove_folder
            && index < self.config.library.watched_folders.len()
        {
            let removed = self.config.library.watched_folders.remove(index);
            tracing::info!("removed music folder {}", removed.display());
            self.touch();

            let config = self.config.clone();
            self.library.start_scan(&config);
        }

        if outcome.add_folder_requested {
            self.pick_folder();
        }
    }

    /// Transient messages, stacked above the player bar.
    fn notices(&mut self, ui: &mut Ui) {
        if self.player.notices.is_empty() {
            return;
        }

        let m = self.theme.metrics;
        let p = self.theme.palette;
        let screen = ui.ctx().content_rect();

        let mut y = screen.bottom() - m.player_bar_height - m.space(2.0);

        for notice in self.player.notices.iter().rev().take(3) {
            // Fade out over the last second of the notice's life.
            let alpha = notice.ttl.min(1.0);
            let text_color = if notice.is_error {
                p.error
            } else {
                p.text_primary
            };

            let painter = ui.ctx().layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("notices"),
            ));

            let font = TextStyle::Body.resolve(ui.style());
            let galley = painter.layout(
                notice.text.clone(),
                font,
                col_alpha(text_color, alpha),
                screen.width() * 0.5,
            );

            let pad = m.space(1.25);
            let size = galley.size() + Vec2::splat(pad * 2.0);
            let rect = Rect::from_min_size(
                egui::Pos2::new(screen.center().x - size.x * 0.5, y - size.y),
                size,
            );

            painter.rect_filled(
                rect,
                egui::CornerRadius::same(m.radius_medium),
                col_alpha(p.bg_elevated, alpha * 0.97),
            );
            painter.galley(rect.min + Vec2::splat(pad), galley, col(text_color));

            y -= size.y + m.space(0.75);
        }
    }

    /// Save a playlist as an M3U8 file the rest of the world can read.
    fn export_playlist(&mut self, id: mp_core::library::PlaylistId) {
        let name = self
            .playlists
            .playlists()
            .iter()
            .find(|playlist| playlist.id == id)
            .map_or_else(|| "Playlist".to_owned(), |playlist| playlist.name.clone());

        let Some(path) = rfd::FileDialog::new()
            .set_title("Export playlist")
            .add_filter("M3U8 playlist", &["m3u8", "m3u"])
            .set_file_name(format!("{}.m3u8", sanitise_filename(&name)))
            .save_file()
        else {
            return;
        };

        match self.library.library().export_playlist(id, &path) {
            Ok(count) => {
                tracing::info!("exported {count} tracks to {}", path.display());
                self.player.notice(
                    format!("Exported {count} tracks to {}", file_label(&path)),
                    false,
                );
            }
            Err(err) => {
                tracing::error!("could not export playlist: {err:#}");
                self.player.notice(format!("Could not export: {err}"), true);
            }
        }
    }

    /// Read an M3U8 file in as a new playlist.
    fn import_playlist(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .set_title("Import playlist")
            .add_filter("M3U8 playlist", &["m3u8", "m3u"])
            .pick_file()
        else {
            return;
        };

        let report = self.library.library_mut().import_playlist(&path);

        match report {
            Ok(report) => {
                tracing::info!(
                    "imported {} tracks from {} ({} missing)",
                    report.added,
                    path.display(),
                    report.missing.len()
                );

                // Named individually in the log rather than the toast: a
                // playlist can be missing dozens of files and a toast listing
                // them all would cover the window.
                for missing in &report.missing {
                    tracing::info!("  not in the library: {}", missing.display());
                }

                self.player
                    .notice(report.summary(), !report.missing.is_empty());

                let library = self.library.library();
                self.playlists.invalidate();
                self.playlists.update(library);
                self.playlists.open(library, report.playlist);
            }
            Err(err) => {
                tracing::error!("could not import playlist: {err:#}");
                self.player.notice(format!("Could not import: {err}"), true);
            }
        }
    }

    /// Open a native folder picker and add the result to the library.
    fn pick_folder(&mut self) {
        let Some(folder) = rfd::FileDialog::new()
            .set_title("Choose a music folder")
            .pick_folder()
        else {
            return;
        };

        if self.config.library.watched_folders.contains(&folder) {
            self.player.notice(
                format!("{} is already in your library", folder.display()),
                false,
            );
            return;
        }

        tracing::info!("added music folder {}", folder.display());
        self.config.library.watched_folders.push(folder);
        self.touch();

        let config = self.config.clone();
        self.library.start_scan(&config);
    }

    /// Report a finished scan once, so the user learns what changed.
    fn announce_scan(&mut self) {
        let Some(summary) = self.library.last_summary.take() else {
            return;
        };

        if summary.cancelled {
            self.player.notice("Scan cancelled".to_owned(), false);
            return;
        }

        if summary.changed_anything() {
            self.player.notice(summary.describe(), false);
            self.artwork.clear();
        }

        if summary.unplayable > 0 && summary.added > 0 {
            let count = summary.unplayable;
            self.player.notice(
                format!(
                    "{count} file{} could not be decoded by this build",
                    if count == 1 { "" } else { "s" }
                ),
                false,
            );
        }
    }
}

impl ResonanceApp {
    /// How long to wait before the next visualiser frame.
    ///
    /// An unfocused window is usually behind something else, so spending a
    /// core on sixty frames a second of animation nobody can see is the kind
    /// of thing that shows up as a warm laptop. Halving the rate is close to
    /// invisible when the window *is* partly visible.
    fn viz_interval(&self, ctx: &egui::Context) -> f32 {
        let cap = self.config.visualizer.fps_cap.clamp(15, 240) as f32;

        let focused = ctx.input(|i| i.focused);
        let effective = if !focused && self.config.visualizer.low_power_when_unfocused {
            cap.min(30.0)
        } else {
            cap
        };

        1.0 / effective
    }
}

impl eframe::App for ResonanceApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut Frame) {
        self.remember_geometry(ui.ctx());
        self.handle_shortcuts(ui.ctx());
        self.maybe_save();

        let now = Instant::now();
        let dt = (now - self.last_frame).as_secs_f32().min(0.25);
        self.last_frame = now;

        self.artwork.begin_frame();
        self.library.poll_watched(&self.config, now);
        let library_changed = self.library.update();
        if library_changed {
            self.announce_scan();
            // Cover ids are content hashes, and a rescan can retire the one a
            // cached palette was read for, so the cache goes. The colour on
            // screen stays: the cover has not changed just because the scanner
            // ran, and dropping it flashed the whole shell back to the
            // configured accent for the length of a fade.
            self.adaptive.forget_palettes();
        }
        self.player.update(
            dt,
            &mut self.library,
            self.config.privacy.track_play_history,
        );
        self.follow_artwork(ui.ctx(), dt);
        self.playlists.update(self.library.library());
        self.top_up_radio();
        self.tend_analysis();
        self.tend_lyrics(ui.ctx());

        // Lyrics are read from disk, so this only does anything while the
        // full-screen view is open and the track has changed under it. The
        // fetcher is `None` unless online lyrics are switched on, in which
        // case a track with nothing on disk is looked up in the background.
        self.immersive
            .observe(self.player.now_playing.as_ref(), self.lyrics_job.as_mut());

        self.tend_media_session();

        // The visualiser is analysed only while it is on screen. Running the
        // FFT for a panel nobody is looking at is pure waste, and skipping it
        // is why the rest of the app costs nothing when the view is elsewhere.
        //
        // The full-screen view paints it behind the artwork, so it counts as
        // on screen there too.
        // A panel using the visualiser as its backdrop counts as on screen
        // too, or the bars would sit frozen behind the content.
        let wants_backdrop = surface::needs_visualizer([
            self.config.appearance.content_background,
            self.config.appearance.player_background,
        ]);

        let showing_visualizer =
            (self.view == View::Visualizer || self.immersive.is_open() || wants_backdrop)
                && self.config.visualizer.kind != mp_core::config::VisualizerKind::None;

        if showing_visualizer {
            self.viz_dt = dt;
            self.visualizers
                .update(self.player.engine(), &self.config.visualizer, dt);
        }

        // egui only repaints on input by default, so an animating seek bar,
        // expiring notices, a running scan and covers still decoding all need
        // to ask for the next frame explicitly.
        if showing_visualizer {
            // The visualiser is the one thing here that wants a real frame
            // rate rather than an occasional nudge.
            ui.ctx()
                .request_repaint_after(Duration::from_secs_f32(self.viz_interval(ui.ctx())));
        } else if self.player.is_playing()
            || !self.player.notices.is_empty()
            // An armed sleep timer counts down in `update`, which only runs on
            // a frame. Without this it would stall the moment playback was
            // paused and never fire.
            || self.player.sleep().is_some()
        {
            ui.ctx().request_repaint_after(Duration::from_millis(50));
        } else if self.library.is_scanning() {
            ui.ctx().request_repaint_after(Duration::from_millis(150));
        } else if self.config.library.watch_for_changes {
            // An idle window is not repainted, so the watch would never tick.
            // Asked for at the poll interval rather than at a frame rate: this
            // exists to wake up occasionally, not to animate.
            ui.ctx().request_repaint_after(LibraryState::WATCH_INTERVAL);
        }
        if self.artwork.wants_repaint() {
            ui.ctx().request_repaint();
        }

        // A pending save needs one more frame to happen on. egui stops
        // painting as soon as the window goes quiet, so without this a setting
        // changed and then left alone is never written: the debounce elapses
        // with no frame to notice it.
        if self.dirty {
            ui.ctx().request_repaint_after(SAVE_DEBOUNCE);
        }

        // A library appearing is the welcome's job being done, so it steps
        // aside on its own rather than waiting to be dismissed.
        if self.welcome && self.library.stats().tracks > 0 {
            self.welcome = false;
        }

        // The full-screen view replaces the shell rather than sitting over it,
        // so nothing underneath is laid out or drawn at all.
        if self.immersive.is_open() {
            self.immersive_view(ui, dt);
            return;
        }

        // The window is undecorated, so the chrome is ours to draw. The title
        // bar claims the top strip before any other panel.
        crate::window_frame::title_bar(ui, &self.theme, mp_core::APP_TITLE);

        self.nav_rail(ui);
        self.player_bar(ui);
        self.queue_panel(ui);
        self.content(ui);

        self.notices(ui);

        // Above the notices and below the window chrome: a modal dialog has to
        // sit over the content it is editing.
        self.tag_editor_dialog(ui.ctx());

        // Foreground layers, so they sit above the panels rather than under.
        let ctx = ui.ctx();
        crate::window_frame::resize_handles(ctx);
        crate::window_frame::window_border(ctx, &self.theme);
    }

    fn save(&mut self, _storage: &mut dyn eframe::Storage) {
        // eframe's own storage is unused; the TOML config is the source of
        // truth so settings stay hand-editable. Kept as a second chance rather
        // than the only one, because eframe persistence is switched off here
        // and this hook is not guaranteed to run.
        self.save_config();
    }

    fn on_exit(&mut self) {
        // Settings are written here rather than left to eframe's `save`: this
        // app turns eframe persistence off, so that hook is not something to
        // rely on. Anything still pending would otherwise be lost on close.
        self.save_config();

        // Bank the tail of whatever was playing before the process goes away.
        self.player
            .flush_listening(&mut self.library, self.config.privacy.track_play_history);

        // Fold the write-ahead log back into the database so the index is one
        // self-contained file when the app is not running. It matters most in
        // portable mode, where the whole folder gets copied to a stick — a
        // copy that catches the database without its log is a copy of a
        // library missing whatever was scanned most recently.
        if let Err(err) = self.library.library().checkpoint() {
            tracing::warn!("could not checkpoint the library index: {err:#}");
        }
    }
}

/// Replace what Windows will not accept in a filename.
///
/// A playlist can be called anything, including `AC/DC: Live?`, and handing
/// that straight to a save dialog produces either a rejected name or a
/// surprise subdirectory.
fn sanitise_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if r#"\/:*?"<>|"#.contains(c) { '-' } else { c })
        .collect();

    let trimmed = cleaned.trim().trim_end_matches('.').trim();

    if trimmed.is_empty() {
        "Playlist".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Just the filename, for a message that has to fit on one line.
fn file_label(path: &std::path::Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into(),
    )
}
