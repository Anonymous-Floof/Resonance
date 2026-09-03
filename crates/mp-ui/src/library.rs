//! The UI's view of the library: what is on screen, and how it got there.
//!
//! The index itself lives in `mp-core` and is the source of truth. This holds
//! the *current query* and its materialised result, refreshed only when
//! something actually changes rather than once per frame — a list view redraws
//! sixty times a second and must never re-run a query to do it.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mp_core::config::Config;
use mp_core::library::{
    Album, Artist, ArtistId, Filter, Folder, Genre, Library, Order, Progress, ScanOptions, Stats,
    Summary, Track,
};

use crate::views::View;

/// A group the user has drilled into from a list view.
///
/// Kept separate from [`View`] so the nav rail and the content can disagree: a
/// user inside "Albums → Drink the Sea" is still in the Albums section, and
/// going back should return them to the album grid rather than the rail's
/// default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Focus {
    Artist {
        id: ArtistId,
        name: String,
    },
    Album {
        id: i64,
        title: String,
        artist: String,
    },
    Genre {
        id: i64,
        name: String,
    },
    Folder {
        path: PathBuf,
        name: String,
    },
}

impl Focus {
    pub fn title(&self) -> &str {
        match self {
            Self::Artist { name, .. } | Self::Genre { name, .. } | Self::Folder { name, .. } => {
                name
            }
            Self::Album { title, .. } => title,
        }
    }

    /// Second line under the heading, when there is something worth saying.
    pub fn subtitle(&self) -> Option<&str> {
        match self {
            Self::Album { artist, .. } => Some(artist),
            _ => None,
        }
    }

    fn filter(&self) -> Filter {
        match self {
            Self::Artist { id, .. } => Filter::Artist(*id),
            Self::Album { id, .. } => Filter::Album(*id),
            Self::Genre { id, .. } => Filter::Genre(*id),
            Self::Folder { path, .. } => Filter::Folder(path.clone()),
        }
    }

    /// Tracks inside an album belong in album order; everywhere else the user's
    /// chosen sort applies.
    fn order(&self, fallback: Order) -> Order {
        match self {
            Self::Album { .. } => Order::TrackNumber,
            _ => fallback,
        }
    }
}

/// A scan running on its own thread.
struct ScanJob {
    progress: Arc<Progress>,
    outcome: Arc<Mutex<Option<anyhow::Result<Summary>>>>,
}

/// Everything the content views read from.
pub struct LibraryState {
    library: Library,

    /// The drill-down currently open, if any.
    focus: Option<Focus>,
    /// What the user typed in the search box.
    search: String,

    order: Order,
    descending: bool,
    /// Hide albums that only have one track — see [`Self::hide_single_albums`].
    hide_single_albums: bool,

    tracks: Vec<Track>,
    artists: Vec<Artist>,
    albums: Vec<Album>,
    genres: Vec<Genre>,
    folders: Vec<Folder>,
    stats: Stats,

    scan: Option<ScanJob>,
    /// Result of the last finished scan, for the status line.
    pub last_summary: Option<Summary>,

    /// Set when the track list no longer matches the selection.
    ///
    /// Split from `groups_stale` because the two go stale for different
    /// reasons and one of them is far more expensive. Typing in the search box
    /// changes the tracks and nothing else — re-running the artist, album,
    /// genre and folder queries on every keystroke as well cost around eighty
    /// milliseconds a character on a thirty-thousand-track library, for
    /// results that were identical every time.
    tracks_stale: bool,

    /// When the watched folders were last polled for changes.
    last_watch_check: Option<Instant>,
    /// Whether the watch has had its first tick, which only starts the clock.
    watch_started: bool,

    /// Bumped whenever anything the statistics are derived from changes.
    ///
    /// Deliberately not bumped by search or sorting, which change what is on
    /// screen without changing a single fact about the library — the Home page
    /// caches against this, and rebuilding it on every keystroke would undo
    /// the point of caching it at all.
    stats_epoch: u64,

    /// Set when the browse lists no longer match the library.
    ///
    /// Only the library's *contents* changing, a change of drill-down (albums
    /// are filtered by the focused artist) or the single-track album setting
    /// can do this.
    groups_stale: bool,
}

impl LibraryState {
    pub fn new(library: Library, config: &Config) -> Self {
        let mut state = Self {
            stats_epoch: 0,
            last_watch_check: None,
            watch_started: false,
            library,
            focus: None,
            search: String::new(),
            order: config.library.default_sort.into(),
            descending: config.library.sort_descending,
            hide_single_albums: true,
            tracks: Vec::new(),
            artists: Vec::new(),
            albums: Vec::new(),
            genres: Vec::new(),
            folders: Vec::new(),
            stats: Stats::default(),
            scan: None,
            last_summary: None,
            tracks_stale: true,
            groups_stale: true,
        };
        state.refresh();
        state
    }

    // -- what is on screen -------------------------------------------------

    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    pub fn artists(&self) -> &[Artist] {
        &self.artists
    }

    pub fn albums(&self) -> &[Album] {
        &self.albums
    }

    pub fn genres(&self) -> &[Genre] {
        &self.genres
    }

    pub fn folders(&self) -> &[Folder] {
        &self.folders
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    pub fn focus(&self) -> Option<&Focus> {
        self.focus.as_ref()
    }

    pub fn is_searching(&self) -> bool {
        !self.search.trim().is_empty()
    }

    pub fn order(&self) -> Order {
        self.order
    }

    pub fn descending(&self) -> bool {
        self.descending
    }

    pub fn hide_single_albums(&self) -> bool {
        self.hide_single_albums
    }

    /// Cover art store, for the texture cache.
    pub fn art(&self) -> &mp_core::library::ArtCache {
        self.library.art()
    }

    /// Mutable access, for the operations that need a transaction.
    ///
    /// Adding to a playlist rewrites several rows at once, so it takes the
    /// connection mutably; reads do not.
    pub fn library_mut(&mut self) -> &mut Library {
        &mut self.library
    }

    pub fn library(&self) -> &Library {
        &self.library
    }

    /// Paths for the list currently on screen, in the order it is displayed.
    ///
    /// This is what playback queues: pressing play on the fourth row of an
    /// album should queue that album, in that order, from that row.
    pub fn visible_paths(&self) -> Vec<PathBuf> {
        self.tracks.iter().map(|track| track.path.clone()).collect()
    }

    // -- changing what is on screen ---------------------------------------

    pub fn set_search(&mut self, text: String) {
        if self.search != text {
            self.search = text;
            // Tracks only. The artists, albums, genres and folders on offer do
            // not depend on what is in the search box.
            self.tracks_stale = true;
        }
    }

    pub fn clear_search(&mut self) {
        self.set_search(String::new());
    }

    pub fn open(&mut self, focus: Focus) {
        if self.focus.as_ref() != Some(&focus) {
            self.focus = Some(focus);
            // Both: the album list is narrowed to the focused artist.
            self.invalidate();
        }
    }

    /// Leave a drill-down and return to the list it was opened from.
    pub fn close_focus(&mut self) {
        if self.focus.take().is_some() {
            self.invalidate();
        }
    }

    pub fn set_order(&mut self, order: Order, descending: bool) {
        if self.order != order || self.descending != descending {
            self.order = order;
            self.descending = descending;
            // Sorting reorders the tracks; the browse lists have their own
            // fixed order and are untouched by it.
            self.tracks_stale = true;
        }
    }

    pub fn set_hide_single_albums(&mut self, hide: bool) {
        if self.hide_single_albums != hide {
            self.hide_single_albums = hide;
            // Only the album list is filtered by this.
            self.groups_stale = true;
        }
    }

    /// Look up one track by its path.
    ///
    /// Goes to the index rather than scanning the visible list: the playing
    /// track is often *not* in the current view, because the user carried on
    /// browsing after pressing play.
    pub fn track_at_path(&self, path: &std::path::Path) -> Option<Track> {
        let id = self.library.id_for_path(path).ok()??;
        self.library.track(id).ok()?
    }

    /// Note that a track was played.
    pub fn record_play(&mut self, path: &std::path::Path) {
        let Ok(Some(id)) = self.library.id_for_path(path) else {
            return;
        };
        match self.library.record_play(id) {
            Ok(()) => self.stats_epoch = self.stats_epoch.wrapping_add(1),
            Err(err) => tracing::debug!("could not record a play: {err:#}"),
        }
    }

    /// Add to the time a track has been listened to.
    ///
    /// Separate from [`record_play`](Self::record_play): a play is decided
    /// once, listening keeps accruing for as long as the audio runs.
    pub fn add_listening(&mut self, path: &std::path::Path, seconds: f64) {
        let Ok(Some(id)) = self.library.id_for_path(path) else {
            return;
        };
        match self.library.add_listening(id, seconds) {
            Ok(()) => self.stats_epoch = self.stats_epoch.wrapping_add(1),
            Err(err) => tracing::debug!("could not record listening time: {err:#}"),
        }
    }

    // -- scanning ----------------------------------------------------------

    pub fn is_scanning(&self) -> bool {
        self.scan.is_some()
    }

    /// Progress of the running scan, if there is one.
    pub fn scan_progress(&self) -> Option<&Progress> {
        self.scan.as_ref().map(|job| job.progress.as_ref())
    }

    /// How often a watched library is re-checked for changes.
    ///
    /// Polled rather than subscribed to. A rescan only reads files whose size
    /// or mtime has moved, so an unchanged library costs a directory walk and
    /// nothing else — cheaper than the dependency and the per-platform
    /// behaviour a real filesystem watcher brings with it. The cost of that
    /// choice is latency: a file added now shows up within this interval
    /// rather than instantly.
    pub const WATCH_INTERVAL: Duration = Duration::from_secs(45);

    /// Rescan a watched library if it is due.
    ///
    /// Does nothing while a scan is already running, while the setting is off,
    /// or before the interval has elapsed, so it is safe to call every frame.
    pub fn poll_watched(&mut self, config: &Config, now: Instant) {
        if !config.library.watch_for_changes || self.scan.is_some() {
            return;
        }

        if let Some(last) = self.last_watch_check
            && now.duration_since(last) < Self::WATCH_INTERVAL
        {
            return;
        }

        self.last_watch_check = Some(now);

        // The very first call only starts the clock. Startup already scans, and
        // scanning twice in the first second helps nobody.
        if self.watch_started {
            self.start_scan(config);
        } else {
            self.watch_started = true;
        }
    }

    /// Start a scan on a background thread.
    ///
    /// Does nothing if one is already running: two scans would contend for the
    /// same disk and produce the same answer.
    pub fn start_scan(&mut self, config: &Config) {
        if self.scan.is_some() {
            return;
        }

        let options = ScanOptions::from_config(&config.library);
        if options.roots.is_empty() {
            // Nothing to scan, but the index may still hold tracks from a
            // folder that has just been removed, so clear it out.
            self.rescan_inline(&options);
            return;
        }

        let Some(scanner) = self.library.detached_scanner(options) else {
            // An in-memory library has no file for a second connection to
            // attach to. Rare, and better than refusing to scan at all.
            let options = ScanOptions::from_config(&config.library);
            self.rescan_inline(&options);
            return;
        };

        let progress = Arc::new(Progress::new());
        let outcome = Arc::new(Mutex::new(None));

        let thread_progress = Arc::clone(&progress);
        let thread_outcome = Arc::clone(&outcome);

        let spawned = std::thread::Builder::new()
            .name("resonance-scan".into())
            .spawn(move || {
                let result = scanner.run(&thread_progress);
                match thread_outcome.lock() {
                    Ok(mut slot) => *slot = Some(result),
                    Err(poisoned) => *poisoned.into_inner() = Some(result),
                }
            });

        match spawned {
            Ok(_) => self.scan = Some(ScanJob { progress, outcome }),
            Err(err) => tracing::error!("could not start the scan thread: {err}"),
        }
    }

    /// Ask a running scan to stop.
    pub fn cancel_scan(&self) {
        if let Some(job) = &self.scan {
            job.progress.cancel();
        }
    }

    /// Fallback path for a library with no file behind it.
    fn rescan_inline(&mut self, options: &ScanOptions) {
        let progress = Progress::new();
        match self.library.scan_blocking(options, &progress) {
            Ok(summary) => {
                self.last_summary = Some(summary);
                self.invalidate();
            }
            Err(err) => tracing::error!("scan failed: {err:#}"),
        }
    }

    // -- per-frame work ----------------------------------------------------

    /// Adopt a finished scan and re-run the current query if needed.
    ///
    /// Returns whether anything changed, so the caller can decide to repaint.
    /// A counter that changes whenever the statistics need recomputing.
    pub fn stats_epoch(&self) -> u64 {
        self.stats_epoch
    }

    pub fn update(&mut self) -> bool {
        let mut changed = false;

        if self.take_finished_scan() {
            changed = true;
            self.stats_epoch = self.stats_epoch.wrapping_add(1);
        }

        if self.tracks_stale || self.groups_stale {
            self.refresh();
            changed = true;
        }

        changed
    }

    fn take_finished_scan(&mut self) -> bool {
        let Some(job) = &self.scan else {
            return false;
        };

        let finished = match job.outcome.lock() {
            Ok(mut slot) => slot.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };

        let Some(result) = finished else {
            return false;
        };

        self.scan = None;

        let changed = match result {
            Ok(summary) => {
                tracing::info!("scan: {}", summary.describe());
                if summary.removed > 0 {
                    // Only worth the directory walk when something actually
                    // stopped referring to a cover.
                    let _ = self.library.prune_art_cache();
                }
                self.last_summary = Some(summary);
                summary.changed_anything()
            }
            // The library may have been left part-written, so assume the worst
            // and re-read. A failed scan is rare; a stale list is not worth it.
            Err(err) => {
                tracing::error!("scan failed: {err:#}");
                true
            }
        };

        if !changed {
            // The watcher rescans every 45 seconds whether or not anything has
            // changed, and almost always nothing has. Reporting a change
            // regardless made every one of those re-run the browse queries and
            // recompute the statistics, for an answer identical to the one
            // already on screen.
            return false;
        }

        // The scan ran on another connection, so this one has to re-read.
        self.invalidate();
        true
    }

    /// The filter the current selection describes.
    fn filter(&self) -> Filter {
        if self.is_searching() {
            return Filter::Search(self.search.trim().to_owned());
        }
        self.focus
            .as_ref()
            .map_or(Filter::All, |focus| focus.filter())
    }

    /// Re-read whichever queries have gone stale.
    ///
    /// The browse lists are deliberately not re-read for a change of search or
    /// sort: they do not depend on either, and they are most of the cost.
    fn refresh(&mut self) {
        if self.tracks_stale {
            self.refresh_tracks();
        }
        if self.groups_stale {
            self.refresh_groups();
        }
    }

    fn refresh_tracks(&mut self) {
        self.tracks_stale = false;

        let filter = self.filter();
        let order = self
            .focus
            .as_ref()
            .map_or(self.order, |focus| focus.order(self.order));

        match self.library.tracks(&filter, order, self.descending) {
            Ok(tracks) => self.tracks = tracks,
            Err(err) => {
                tracing::error!("track query failed: {err:#}");
                self.tracks.clear();
            }
        }
    }

    /// The artist, album, genre and folder lists, plus the library counts.
    ///
    /// All of them together, not just the visible one: the counts in the nav
    /// rail and the empty states read from the others.
    fn refresh_groups(&mut self) {
        self.groups_stale = false;

        let min_album_tracks = u32::from(self.hide_single_albums) + 1;
        let album_artist = match &self.focus {
            Some(Focus::Artist { id, .. }) => Some(*id),
            _ => None,
        };

        self.artists = self.library.artists().unwrap_or_else(|err| {
            tracing::error!("artist query failed: {err:#}");
            Vec::new()
        });
        self.albums = self
            .library
            .albums(album_artist, min_album_tracks)
            .unwrap_or_else(|err| {
                tracing::error!("album query failed: {err:#}");
                Vec::new()
            });
        self.genres = self.library.genres().unwrap_or_else(|err| {
            tracing::error!("genre query failed: {err:#}");
            Vec::new()
        });
        self.folders = self.library.folders().unwrap_or_else(|err| {
            tracing::error!("folder query failed: {err:#}");
            Vec::new()
        });
        self.stats = self.library.stats().unwrap_or_default();
    }

    /// Force the next frame to re-read everything.
    pub fn invalidate(&mut self) {
        self.tracks_stale = true;
        self.groups_stale = true;
    }

    /// Whether the search hit the result cap, so the view can say so.
    pub fn search_was_capped(&self) -> bool {
        self.is_searching() && self.tracks.len() >= mp_core::library::query::SEARCH_LIMIT
    }

    /// Whether this view has nothing to show, distinguishing "no library" from
    /// "nothing matched" so the empty state can say the right thing.
    pub fn emptiness(&self, view: View) -> Emptiness {
        if self.stats.tracks == 0 {
            return Emptiness::NoLibrary;
        }
        if self.is_searching() && self.tracks.is_empty() {
            return Emptiness::NoMatches;
        }

        let count = match view {
            View::Songs => self.tracks.len(),
            View::Artists => self.artists.len(),
            View::Albums => self.albums.len(),
            View::Genres => self.genres.len(),
            View::Folders => self.folders.len(),
            _ => 1,
        };

        if count == 0 {
            Emptiness::NothingHere
        } else {
            Emptiness::HasContent
        }
    }
}

/// Why a view has nothing to draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Emptiness {
    HasContent,
    /// No folders have been added, or nothing in them could be read.
    NoLibrary,
    /// A search that matched nothing.
    NoMatches,
    /// The library has tracks, but none carry what this view groups by.
    NothingHere,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> LibraryState {
        LibraryState::new(Library::in_memory().unwrap(), &Config::default())
    }

    #[test]
    fn an_empty_library_says_so_rather_than_showing_no_matches() {
        let state = state();
        assert_eq!(state.emptiness(View::Songs), Emptiness::NoLibrary);
        assert_eq!(state.emptiness(View::Artists), Emptiness::NoLibrary);
    }

    /// Nothing is queued to re-run.
    fn settle(state: &mut LibraryState) {
        state.tracks_stale = false;
        state.groups_stale = false;
    }

    /// Opening the same group twice must not schedule redundant queries.
    #[test]
    fn reopening_the_same_focus_is_not_a_change() {
        let mut state = state();
        settle(&mut state);

        let focus = Focus::Genre {
            id: 1,
            name: "Rock".into(),
        };
        state.open(focus.clone());
        assert!(state.tracks_stale);

        settle(&mut state);
        state.open(focus);
        assert!(
            !state.tracks_stale && !state.groups_stale,
            "an identical selection should be a no-op"
        );
    }

    /// The measurement that prompted the split: typing in the search box cost
    /// a re-run of the artist, album, genre and folder queries on every
    /// keystroke, for results that could not possibly have changed.
    #[test]
    fn typing_a_search_does_not_disturb_the_browse_lists() {
        let mut state = state();
        settle(&mut state);

        state.set_search("paper".into());

        assert!(state.tracks_stale, "the track list must be re-read");
        assert!(
            !state.groups_stale,
            "the browse lists do not depend on the search box"
        );
    }

    /// Nor does re-sorting: the browse lists have their own fixed order.
    #[test]
    fn changing_the_sort_does_not_disturb_the_browse_lists() {
        let mut state = state();
        settle(&mut state);

        state.set_order(Order::PlayCount, true);

        assert!(state.tracks_stale);
        assert!(!state.groups_stale);
    }

    /// Opening an artist *does* narrow the album list, so that one is both.
    #[test]
    fn opening_an_artist_reruns_the_album_list_too() {
        let mut state = state();
        settle(&mut state);

        state.open(Focus::Artist {
            id: 3,
            name: "Someone".into(),
        });

        assert!(state.tracks_stale);
        assert!(
            state.groups_stale,
            "albums are filtered by the focused artist"
        );
    }

    /// And hiding single-track albums touches only the album list.
    #[test]
    fn the_single_album_filter_leaves_the_tracks_alone() {
        let mut state = state();
        settle(&mut state);

        state.set_hide_single_albums(!state.hide_single_albums());

        assert!(!state.tracks_stale, "the track list is unaffected");
        assert!(state.groups_stale);
    }

    #[test]
    fn leaving_a_group_returns_to_the_unfiltered_list() {
        let mut state = state();
        state.open(Focus::Artist {
            id: 7,
            name: "Someone".into(),
        });
        assert!(matches!(state.filter(), Filter::Artist(7)));

        state.close_focus();
        assert!(matches!(state.filter(), Filter::All));
    }

    /// Search has to win over a drill-down, or typing while inside an album
    /// would silently search only that album.
    #[test]
    fn searching_escapes_the_current_group() {
        let mut state = state();
        state.open(Focus::Album {
            id: 3,
            title: "Some Album".into(),
            artist: "Someone".into(),
        });
        state.set_search("paper".into());

        assert!(matches!(state.filter(), Filter::Search(text) if text == "paper"));

        state.clear_search();
        assert!(matches!(state.filter(), Filter::Album(3)));
    }

    #[test]
    fn album_tracks_are_ordered_by_track_number() {
        let focus = Focus::Album {
            id: 1,
            title: "A".into(),
            artist: "B".into(),
        };
        assert_eq!(focus.order(Order::Title), Order::TrackNumber);

        let other = Focus::Genre {
            id: 1,
            name: "Rock".into(),
        };
        assert_eq!(other.order(Order::Title), Order::Title);
    }

    #[test]
    fn whitespace_is_not_a_search() {
        let mut state = state();
        state.set_search("   ".into());
        assert!(!state.is_searching());
        assert!(matches!(state.filter(), Filter::All));
    }
}
