//! Playlist state for the interface.
//!
//! Mirrors [`LibraryState`](crate::library::LibraryState): the database is the
//! source of truth, this holds the snapshot the current frame draws from, and
//! everything that mutates goes through here so the snapshot can be refreshed
//! in exactly one place.
//!
//! Reading is deliberately eager rather than lazy. A playlist is hundreds of
//! rows, not tens of thousands, and re-reading one after an edit costs less
//! than the bookkeeping needed to patch a cached copy correctly.

use std::path::{Path, PathBuf};

use mp_core::library::model::{Filter, Folder, Order, Track, TrackId};
use mp_core::library::{Library, Playlist, PlaylistId, Seed, SmartRules, Suggestion};

/// Which builder tool is open, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tool {
    #[default]
    None,
    /// Browse the library and tick tracks to add.
    ///
    /// The way the first track gets into a playlist. Without it the builder is
    /// a closed loop: "add similar" needs something to be similar *to*, so a
    /// new playlist could never be started at all.
    Library,
    /// Ranked suggestions seeded from this playlist.
    Similar,
    /// Rules, for a smart playlist.
    Rules,
}

pub struct PlaylistState {
    playlists: Vec<Playlist>,

    /// The playlist being viewed, and its tracks.
    open: Option<Playlist>,
    tracks: Vec<Track>,

    tool: Tool,
    suggestions: Vec<Suggestion>,

    /// Tracks offered by the library browser, and how it is filtered.
    candidates: Vec<Track>,
    query: String,
    folder: Option<PathBuf>,
    folders: Vec<Folder>,
    /// Set when the browser had more to show than it is showing.
    truncated: bool,

    /// Suggestions the user has ticked, ready to add.
    picked: Vec<TrackId>,

    /// Rules being edited, applied only when the user says so — a live-applied
    /// half-built rule would keep emptying the playlist as it was typed.
    draft_rules: Option<SmartRules>,

    /// Set when something changed and the snapshot needs rebuilding.
    stale: bool,
    /// The most recent failure, for the caller to surface.
    error: Option<String>,
}

impl PlaylistState {
    pub fn new() -> Self {
        Self {
            playlists: Vec::new(),
            open: None,
            tracks: Vec::new(),
            tool: Tool::None,
            suggestions: Vec::new(),
            candidates: Vec::new(),
            query: String::new(),
            folder: None,
            folders: Vec::new(),
            truncated: false,
            picked: Vec::new(),
            draft_rules: None,
            stale: true,
            error: None,
        }
    }

    // -- reading -----------------------------------------------------------

    pub fn playlists(&self) -> &[Playlist] {
        &self.playlists
    }

    pub fn open_playlist(&self) -> Option<&Playlist> {
        self.open.as_ref()
    }

    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    pub fn tool(&self) -> Tool {
        self.tool
    }

    pub fn suggestions(&self) -> &[Suggestion] {
        &self.suggestions
    }

    /// Tracks the library browser is offering.
    pub fn candidates(&self) -> &[Track] {
        &self.candidates
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn folder_filter(&self) -> Option<&Path> {
        self.folder.as_deref()
    }

    pub fn folders(&self) -> &[Folder] {
        &self.folders
    }

    /// Whether the browser is showing only part of what matched.
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    pub fn draft_rules(&self) -> Option<&SmartRules> {
        self.draft_rules.as_ref()
    }

    /// Lift the rule draft out for the duration of a frame.
    ///
    /// The rule editor needs the draft mutably while the rest of the view reads
    /// the state immutably, and no amount of method-splitting makes those two
    /// borrows coexist. Taking the draft out, rendering, and putting it back is
    /// what lets the editor be drawn *in place* — an earlier attempt drew it
    /// after the view returned, which put it below the track list where there
    /// was no room left and it never appeared at all.
    pub fn take_draft(&mut self) -> Option<SmartRules> {
        self.draft_rules.take()
    }

    pub fn put_draft(&mut self, draft: Option<SmartRules>) {
        self.draft_rules = draft;
    }

    pub fn is_picked(&self, id: TrackId) -> bool {
        self.picked.contains(&id)
    }

    pub fn picked_count(&self) -> usize {
        self.picked.len()
    }

    /// The paths of the open playlist, in order, for handing to the player.
    pub fn track_paths(&self) -> Vec<PathBuf> {
        self.tracks.iter().map(|track| track.path.clone()).collect()
    }

    /// Take the last error, if any.
    pub fn take_error(&mut self) -> Option<String> {
        self.error.take()
    }

    /// Rebuild the snapshot if anything has changed. Returns whether it did.
    pub fn update(&mut self, library: &Library) -> bool {
        if !self.stale {
            return false;
        }
        self.stale = false;

        match library.playlists() {
            Ok(playlists) => self.playlists = playlists,
            Err(err) => self.fail("could not read your playlists", err),
        }

        if let Some(id) = self.open.as_ref().map(|playlist| playlist.id) {
            match library.playlist(id) {
                // Deleted from under us — close rather than showing a ghost.
                Ok(None) => self.close(),
                Ok(Some(playlist)) => {
                    self.open = Some(playlist);
                    match library.playlist_tracks(id) {
                        Ok(tracks) => self.tracks = tracks,
                        Err(err) => self.fail("could not read that playlist", err),
                    }
                }
                Err(err) => self.fail("could not read that playlist", err),
            }
        }

        true
    }

    /// Mark the snapshot as needing a rebuild.
    pub fn invalidate(&mut self) {
        self.stale = true;
    }

    fn fail(&mut self, what: &str, err: anyhow::Error) {
        tracing::error!("{what}: {err:#}");
        self.error = Some(format!("{what}."));
    }

    // -- navigation --------------------------------------------------------

    pub fn open(&mut self, library: &Library, id: PlaylistId) {
        match library.playlist(id) {
            Ok(Some(playlist)) => {
                // A smart playlist opens with its own rules loaded, so the
                // editor starts from what is actually in force.
                self.draft_rules = playlist.rules.clone();
                self.open = Some(playlist);
                self.tool = Tool::None;
                self.suggestions.clear();
                self.picked.clear();
                self.stale = true;
            }
            Ok(None) => self.close(),
            Err(err) => self.fail("could not open that playlist", err),
        }
    }

    pub fn close(&mut self) {
        self.open = None;
        self.tracks.clear();
        self.tool = Tool::None;
        self.suggestions.clear();
        self.picked.clear();
        self.draft_rules = None;
    }

    /// Rebuild the library browser's list from the current filter.
    pub fn refresh_candidates(&mut self, library: &Library) {
        // The folder list is only needed by this tool, so it is fetched when
        // the tool opens rather than kept current all the time.
        if self.folders.is_empty() {
            match library.folders() {
                Ok(folders) => self.folders = folders,
                Err(err) => self.fail("could not list your folders", err),
            }
        }

        let query = self.query.trim();

        let found = if !query.is_empty() {
            library.search(query, Some(BROWSE_LIMIT + 1))
        } else if let Some(folder) = &self.folder {
            library.tracks(&Filter::Folder(folder.clone()), Order::Title, false)
        } else {
            library.tracks(&Filter::All, Order::Title, false)
        };

        match found {
            Ok(mut tracks) => {
                // Capped rather than paged: this is a picker, and a list long
                // enough to need paging is one that wants the search box.
                self.truncated = tracks.len() > BROWSE_LIMIT;
                tracks.truncate(BROWSE_LIMIT);
                self.candidates = tracks;
            }
            Err(err) => {
                self.candidates.clear();
                self.fail("could not read your library", err);
            }
        }
    }

    pub fn set_query(&mut self, library: &Library, query: String) {
        self.query = query;
        self.refresh_candidates(library);
    }

    pub fn set_folder_filter(&mut self, library: &Library, folder: Option<PathBuf>) {
        self.folder = folder;
        self.refresh_candidates(library);
    }

    /// Put the open tool away.
    ///
    /// The draft rules are deliberately kept: an edit that was never applied is
    /// still work the user did, and discarding it because a panel closed would
    /// be the worst possible moment to throw it away.
    pub fn close_tool(&mut self) {
        self.tool = Tool::None;
        self.suggestions.clear();
        self.candidates.clear();
        self.picked.clear();
    }

    pub fn set_tool(&mut self, library: &Library, tool: Tool) {
        // Toggling the open tool closes it, which is what clicking a pressed
        // button should do.
        self.tool = if self.tool == tool { Tool::None } else { tool };

        match self.tool {
            Tool::Similar => self.refresh_suggestions(library),
            Tool::Library => self.refresh_candidates(library),
            _ => {}
        }
    }

    // -- editing -----------------------------------------------------------

    pub fn create(&mut self, library: &Library, name: &str) -> Option<PlaylistId> {
        match library.create_playlist(name) {
            Ok(id) => {
                self.stale = true;
                Some(id)
            }
            Err(err) => {
                self.fail("could not create the playlist", err);
                None
            }
        }
    }

    pub fn create_smart(&mut self, library: &Library, name: &str) -> Option<PlaylistId> {
        match library.create_smart_playlist(name, &SmartRules::default()) {
            Ok(id) => {
                self.stale = true;
                Some(id)
            }
            Err(err) => {
                self.fail("could not create the playlist", err);
                None
            }
        }
    }

    pub fn rename(&mut self, library: &Library, id: PlaylistId, name: &str) {
        if let Err(err) = library.rename_playlist(id, name) {
            self.fail("could not rename the playlist", err);
        }
        self.stale = true;
    }

    pub fn delete(&mut self, library: &Library, id: PlaylistId) {
        if let Err(err) = library.delete_playlist(id) {
            self.fail("could not delete the playlist", err);
        }

        if self.open.as_ref().is_some_and(|open| open.id == id) {
            self.close();
        }
        self.stale = true;
    }

    pub fn add_tracks(&mut self, library: &mut Library, id: PlaylistId, tracks: &[TrackId]) {
        if let Err(err) = library.add_to_playlist(id, tracks) {
            self.fail("could not add to the playlist", err);
        }
        self.stale = true;
    }

    pub fn remove_at(&mut self, library: &mut Library, id: PlaylistId, position: usize) {
        if let Err(err) = library.remove_from_playlist(id, position) {
            self.fail("could not remove that track", err);
        }
        self.stale = true;
    }

    pub fn move_item(&mut self, library: &mut Library, id: PlaylistId, from: usize, to: usize) {
        if let Err(err) = library.move_in_playlist(id, from, to) {
            self.fail("could not reorder the playlist", err);
        }
        self.stale = true;
    }

    /// Commit the rules being edited.
    pub fn apply_rules(&mut self, library: &Library) {
        let (Some(playlist), Some(rules)) = (self.open.as_ref(), self.draft_rules.as_ref()) else {
            return;
        };

        if let Err(err) = library.set_playlist_rules(playlist.id, rules) {
            self.fail("could not save the rules", err);
        }

        // Same reasoning as adding: the rules are in force, the panel has
        // nothing left to say, and the result is the track list underneath it.
        self.close_tool();

        self.stale = true;
    }

    // -- suggestions -------------------------------------------------------

    /// Recompute the suggestion list for the open playlist.
    pub fn refresh_suggestions(&mut self, library: &Library) {
        let Some(playlist) = self.open.as_ref() else {
            self.suggestions.clear();
            return;
        };

        // A smart playlist has no stored items to seed from, so it is seeded
        // from a track it currently matches instead.
        let seed = if playlist.is_smart() {
            match self.tracks.first() {
                Some(track) => Seed::Track(track.id),
                None => {
                    self.suggestions.clear();
                    return;
                }
            }
        } else {
            Seed::Playlist(playlist.id)
        };

        match library.suggest(seed, SUGGESTION_COUNT) {
            Ok(suggestions) => {
                self.suggestions = suggestions;
                self.picked.clear();
            }
            Err(err) => {
                self.suggestions.clear();
                self.fail("could not work out what to suggest", err);
            }
        }
    }

    pub fn toggle_pick(&mut self, id: TrackId) {
        if let Some(at) = self.picked.iter().position(|picked| *picked == id) {
            self.picked.remove(at);
        } else {
            self.picked.push(id);
        }
    }

    /// Tick everything the open tool is showing.
    ///
    /// Which list that is depends on the tool. An earlier version always read
    /// the suggestions, which left the "All" button in the library browser
    /// silently doing nothing.
    pub fn pick_all(&mut self) {
        self.picked = match self.tool {
            Tool::Library => self.candidates.iter().map(|track| track.id).collect(),
            Tool::Similar => self
                .suggestions
                .iter()
                .map(|suggestion| suggestion.track.id)
                .collect(),
            _ => Vec::new(),
        };
    }

    pub fn clear_picks(&mut self) {
        self.picked.clear();
    }

    /// Add everything ticked, then refresh so the same tracks are not offered
    /// straight back.
    pub fn add_picked(&mut self, library: &mut Library) {
        let Some(id) = self.open.as_ref().map(|playlist| playlist.id) else {
            return;
        };

        if self.picked.is_empty() {
            return;
        }

        let picked = std::mem::take(&mut self.picked);
        self.add_tracks(library, id, &picked);

        // The panel has done what it was opened to do, so it goes away. Leaving
        // it up means the only way to dismiss it is knowing that the toolbar
        // button toggles — and what it is showing is stale the moment the
        // tracks land, because the list it was drawn from has just changed.
        //
        // Re-opening recomputes from the playlist as it now stands, which is
        // also what keeps the tracks just added from being offered straight
        // back.
        self.close_tool();

        self.stale = true;
        self.update(library);
    }

    /// Whether a track is already in the open playlist.
    ///
    /// Shown in the browser so adding the same thing twice is a deliberate act
    /// rather than an accident — a playlist may legitimately repeat a track,
    /// but rarely by mistake.
    pub fn already_holds(&self, id: TrackId) -> bool {
        self.tracks.iter().any(|track| track.id == id)
    }
}

/// How many tracks the library browser lists at once.
///
/// A picker, not a library view — past a few hundred rows the search box is
/// the answer, not more scrolling.
pub const BROWSE_LIMIT: usize = 300;

/// How many suggestions to work out at a time.
///
/// Enough to scroll through and pick from, few enough that the ranking stays
/// meaningful — the tail of a longer list is padding, not recommendation.
pub const SUGGESTION_COUNT: usize = 40;

impl Default for PlaylistState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mp_core::library::Library;

    fn library_with_tracks() -> Library {
        let library = Library::in_memory().unwrap();

        library
            .connection()
            .execute_batch(
                "INSERT INTO artists (id, name, sort_name) VALUES (1, 'A', 'a');
                 INSERT INTO genres (id, name, sort_name) VALUES (1, 'G', 'g');",
            )
            .unwrap();

        for id in 1..=4 {
            library
                .connection()
                .execute(
                    "INSERT INTO tracks (
                         id, path, folder, file_name, mtime, size, title, sort_title,
                         artist_id, year, duration_ms, added_at, last_seen_at
                     ) VALUES (?1, ?2, '/m', 'x.mp3', 1, 2, ?3, ?3, 1, 2000, 60000, 0, 0)",
                    rusqlite::params![id, format!("/m/{id}.mp3"), format!("t{id}")],
                )
                .unwrap();
            library
                .connection()
                .execute(
                    "INSERT INTO track_genres (track_id, genre_id) VALUES (?1, 1)",
                    rusqlite::params![id],
                )
                .unwrap();
        }

        library
    }

    #[test]
    fn a_created_playlist_appears_after_an_update() {
        let library = library_with_tracks();
        let mut state = PlaylistState::new();

        state.create(&library, "Evening");

        assert!(state.update(&library));
        assert_eq!(state.playlists().len(), 1);
        assert_eq!(state.playlists()[0].name, "Evening");
    }

    /// The snapshot only rebuilds when something changed, so a quiet frame
    /// costs nothing.
    #[test]
    fn update_does_nothing_when_nothing_changed() {
        let library = library_with_tracks();
        let mut state = PlaylistState::new();

        assert!(state.update(&library), "the first update should populate");
        assert!(!state.update(&library), "an idle update did work");

        state.invalidate();
        assert!(state.update(&library));
    }

    #[test]
    fn opening_a_playlist_loads_its_tracks() {
        let mut library = library_with_tracks();
        let mut state = PlaylistState::new();

        let id = state.create(&library, "Set").unwrap();
        state.add_tracks(&mut library, id, &[1, 2, 3]);
        state.open(&library, id);
        state.update(&library);

        assert_eq!(state.tracks().len(), 3);
        assert_eq!(state.open_playlist().unwrap().id, id);
        assert_eq!(state.track_paths().len(), 3);
    }

    /// A playlist deleted elsewhere must not linger on screen.
    #[test]
    fn a_deleted_playlist_closes_itself() {
        let library = library_with_tracks();
        let mut state = PlaylistState::new();

        let id = state.create(&library, "Doomed").unwrap();
        state.open(&library, id);
        state.update(&library);
        assert!(state.open_playlist().is_some());

        library.delete_playlist(id).unwrap();
        state.invalidate();
        state.update(&library);

        assert!(
            state.open_playlist().is_none(),
            "a deleted playlist stayed open"
        );
    }

    #[test]
    fn deleting_the_open_playlist_closes_it() {
        let library = library_with_tracks();
        let mut state = PlaylistState::new();

        let id = state.create(&library, "Doomed").unwrap();
        state.open(&library, id);
        state.delete(&library, id);

        assert!(state.open_playlist().is_none());
    }

    /// Clicking the open tool again should close it.
    #[test]
    fn a_tool_toggles_rather_than_only_opening() {
        let library = library_with_tracks();
        let mut state = PlaylistState::new();

        let id = state.create(&library, "Set").unwrap();
        state.open(&library, id);

        state.set_tool(&library, Tool::Similar);
        assert_eq!(state.tool(), Tool::Similar);

        state.set_tool(&library, Tool::Similar);
        assert_eq!(state.tool(), Tool::None);
    }

    #[test]
    fn suggestions_are_seeded_from_the_open_playlist() {
        let mut library = library_with_tracks();
        let mut state = PlaylistState::new();

        let id = state.create(&library, "Set").unwrap();
        state.add_tracks(&mut library, id, &[1]);
        state.open(&library, id);
        state.update(&library);

        state.set_tool(&library, Tool::Similar);

        assert!(!state.suggestions().is_empty(), "nothing was suggested");
        assert!(
            !state
                .suggestions()
                .iter()
                .any(|suggestion| suggestion.track.id == 1),
            "a track already in the playlist was suggested"
        );
    }

    #[test]
    fn picking_and_adding_moves_tracks_into_the_playlist() {
        let mut library = library_with_tracks();
        let mut state = PlaylistState::new();

        let id = state.create(&library, "Set").unwrap();
        state.add_tracks(&mut library, id, &[1]);
        state.open(&library, id);
        state.update(&library);
        state.set_tool(&library, Tool::Similar);

        let first = state.suggestions()[0].track.id;
        state.toggle_pick(first);
        assert!(state.is_picked(first));
        assert_eq!(state.picked_count(), 1);

        state.add_picked(&mut library);

        assert_eq!(
            state.picked_count(),
            0,
            "picks were not cleared after adding"
        );
        assert_eq!(state.tracks().len(), 2);
        assert!(state.tracks().iter().any(|track| track.id == first));
    }

    /// Closing a tool by hand must not throw away an unapplied rule edit.
    #[test]
    fn closing_a_tool_keeps_the_rule_draft() {
        use mp_core::library::smart::{Field, Node, Op, Rule};

        let library = library_with_tracks();
        let mut state = PlaylistState::new();

        let id = state.create_smart(&library, "Smart").unwrap();
        state.open(&library, id);
        state.set_tool(&library, Tool::Rules);

        let mut draft = state.take_draft().expect("a smart playlist has rules");
        draft
            .root
            .nodes
            .push(Node::Rule(Rule::new(Field::Artist, Op::Contains, "a")));
        state.put_draft(Some(draft));

        state.close_tool();

        assert_eq!(
            state.draft_rules().map(|rules| rules.root.nodes.len()),
            Some(1),
            "closing the panel discarded an unapplied edit"
        );
    }

    /// Adding puts the panel away — there is otherwise no obvious way to
    /// dismiss it, and what it shows is stale as soon as the tracks land.
    #[test]
    fn adding_closes_the_panel() {
        let mut library = library_with_tracks();
        let mut state = PlaylistState::new();

        let id = state.create(&library, "Set").unwrap();
        state.add_tracks(&mut library, id, &[1]);
        state.open(&library, id);
        state.update(&library);

        for tool in [Tool::Similar, Tool::Library] {
            state.set_tool(&library, tool);
            assert_eq!(state.tool(), tool);

            state.pick_all();
            assert!(state.picked_count() > 0, "{tool:?} offered nothing to pick");

            state.add_picked(&mut library);

            assert_eq!(
                state.tool(),
                Tool::None,
                "{tool:?} stayed open after adding"
            );
            assert_eq!(state.picked_count(), 0);
            assert!(state.suggestions().is_empty());
            assert!(state.candidates().is_empty());
        }
    }

    /// Re-opening after an add has to recompute, or the panel would offer back
    /// exactly the tracks that were just added.
    #[test]
    fn tracks_just_added_are_not_suggested_again() {
        let mut library = library_with_tracks();
        let mut state = PlaylistState::new();

        let id = state.create(&library, "Set").unwrap();
        state.add_tracks(&mut library, id, &[1]);
        state.open(&library, id);
        state.update(&library);
        state.set_tool(&library, Tool::Similar);

        state.pick_all();
        let added: Vec<TrackId> = state
            .suggestions()
            .iter()
            .map(|suggestion| suggestion.track.id)
            .collect();
        assert!(!added.is_empty());

        state.add_picked(&mut library);

        // Open it again, the way the user would.
        state.set_tool(&library, Tool::Similar);

        for id in added {
            assert!(
                !state
                    .suggestions()
                    .iter()
                    .any(|suggestion| suggestion.track.id == id),
                "track {id} was suggested again after being added"
            );
        }
    }

    /// Applying rules puts that panel away as well.
    #[test]
    fn applying_rules_closes_the_panel() {
        use mp_core::library::smart::{Field, Node, Op, Rule};

        let library = library_with_tracks();
        let mut state = PlaylistState::new();

        let id = state.create_smart(&library, "Smart").unwrap();
        state.open(&library, id);
        state.update(&library);

        state.set_tool(&library, Tool::Rules);
        assert_eq!(state.tool(), Tool::Rules);

        let mut draft = state.take_draft().expect("a smart playlist has rules");
        draft
            .root
            .nodes
            .push(Node::Rule(Rule::new(Field::Title, Op::Is, "t2")));
        state.put_draft(Some(draft));

        state.apply_rules(&library);
        state.update(&library);

        assert_eq!(state.tool(), Tool::None, "the rules panel stayed open");
        assert_eq!(state.tracks().len(), 1, "the rules were not applied");

        // And the draft survives, so re-opening shows what is in force rather
        // than an empty editor.
        assert!(state.draft_rules().is_some());
    }

    /// The gap the screen exposed: without a browser, "add similar" needs
    /// something to be similar to, so a new playlist could never be started.
    #[test]
    fn a_brand_new_playlist_can_be_filled_from_the_library() {
        let mut library = library_with_tracks();
        let mut state = PlaylistState::new();

        let id = state.create(&library, "Fresh").unwrap();
        state.open(&library, id);
        state.update(&library);

        // Nothing to suggest from, which is exactly the dead end.
        state.set_tool(&library, Tool::Similar);
        assert!(state.suggestions().is_empty());

        // The browser is the way in.
        state.set_tool(&library, Tool::Library);
        assert!(
            !state.candidates().is_empty(),
            "the browser offered nothing"
        );

        let first = state.candidates()[0].id;
        state.toggle_pick(first);
        state.add_picked(&mut library);

        assert_eq!(state.tracks().len(), 1);
        assert_eq!(state.tracks()[0].id, first);

        // And now similar has something to work from.
        state.set_tool(&library, Tool::Similar);
        assert!(!state.suggestions().is_empty());
    }

    #[test]
    fn the_browser_can_be_searched_and_filtered_by_folder() {
        let library = library_with_tracks();
        let mut state = PlaylistState::new();

        let id = state.create(&library, "Set").unwrap();
        state.open(&library, id);
        state.set_tool(&library, Tool::Library);

        let all = state.candidates().len();
        assert!(all >= 4);

        state.set_query(&library, "t2".to_owned());
        assert!(
            state.candidates().len() < all,
            "searching did not narrow the list"
        );

        // Clearing the search restores everything.
        state.set_query(&library, String::new());
        assert_eq!(state.candidates().len(), all);

        // And the folder list is populated for the picker.
        assert!(!state.folders().is_empty(), "no folders offered");
    }

    /// Adding the same track twice is allowed but should be visibly marked.
    #[test]
    fn the_browser_marks_tracks_already_in_the_playlist() {
        let mut library = library_with_tracks();
        let mut state = PlaylistState::new();

        let id = state.create(&library, "Set").unwrap();
        state.add_tracks(&mut library, id, &[1]);
        state.open(&library, id);
        state.update(&library);
        state.set_tool(&library, Tool::Library);

        assert!(state.already_holds(1));
        assert!(!state.already_holds(2));
    }

    #[test]
    fn toggling_a_pick_twice_deselects_it() {
        let mut state = PlaylistState::new();

        state.toggle_pick(7);
        assert!(state.is_picked(7));

        state.toggle_pick(7);
        assert!(!state.is_picked(7));
        assert_eq!(state.picked_count(), 0);
    }

    #[test]
    fn a_smart_playlist_opens_with_its_rules_loaded() {
        let library = library_with_tracks();
        let mut state = PlaylistState::new();

        let id = state.create_smart(&library, "Smart").unwrap();
        state.open(&library, id);

        assert!(
            state.draft_rules().is_some(),
            "the rule editor opened with nothing in it"
        );
        assert!(state.open_playlist().unwrap().is_smart());
    }

    #[test]
    fn editing_and_applying_rules_changes_what_the_playlist_holds() {
        use mp_core::library::smart::{Field, Node, Op, Rule};

        let library = library_with_tracks();
        let mut state = PlaylistState::new();

        let id = state.create_smart(&library, "Smart").unwrap();
        state.open(&library, id);
        state.update(&library);
        assert_eq!(
            state.tracks().len(),
            4,
            "an empty rule set should match all"
        );

        // The view lifts the draft out to edit it, the way the rule editor
        // does, then puts it back.
        let mut draft = state.take_draft().expect("a smart playlist has rules");
        draft
            .root
            .nodes
            .push(Node::Rule(Rule::new(Field::Title, Op::Is, "t2")));
        state.put_draft(Some(draft));

        state.apply_rules(&library);
        state.update(&library);

        assert_eq!(state.tracks().len(), 1);
        assert_eq!(state.tracks()[0].title, "t2");
    }

    /// A half-typed rule must not take effect until it is applied.
    #[test]
    fn editing_rules_does_not_change_the_playlist_until_applied() {
        use mp_core::library::smart::{Field, Node, Op, Rule};

        let library = library_with_tracks();
        let mut state = PlaylistState::new();

        let id = state.create_smart(&library, "Smart").unwrap();
        state.open(&library, id);
        state.update(&library);

        let mut draft = state.take_draft().expect("a smart playlist has rules");
        draft.root.nodes.push(Node::Rule(Rule::new(
            Field::Title,
            Op::Is,
            "nothing matches",
        )));
        state.put_draft(Some(draft));

        state.invalidate();
        state.update(&library);

        assert_eq!(
            state.tracks().len(),
            4,
            "an unapplied draft changed the playlist"
        );
    }

    #[test]
    fn removing_a_track_shortens_the_playlist() {
        let mut library = library_with_tracks();
        let mut state = PlaylistState::new();

        let id = state.create(&library, "Set").unwrap();
        state.add_tracks(&mut library, id, &[1, 2, 3]);
        state.open(&library, id);
        state.update(&library);

        state.remove_at(&mut library, id, 1);
        state.update(&library);

        let ids: Vec<TrackId> = state.tracks().iter().map(|track| track.id).collect();
        assert_eq!(ids, vec![1, 3]);
    }

    #[test]
    fn reordering_moves_a_track() {
        let mut library = library_with_tracks();
        let mut state = PlaylistState::new();

        let id = state.create(&library, "Set").unwrap();
        state.add_tracks(&mut library, id, &[1, 2, 3]);
        state.open(&library, id);
        state.update(&library);

        state.move_item(&mut library, id, 0, 2);
        state.update(&library);

        let ids: Vec<TrackId> = state.tracks().iter().map(|track| track.id).collect();
        assert_eq!(ids, vec![2, 3, 1]);
    }

    #[test]
    fn renaming_shows_the_new_name() {
        let library = library_with_tracks();
        let mut state = PlaylistState::new();

        let id = state.create(&library, "Before").unwrap();
        state.rename(&library, id, "After");
        state.update(&library);

        assert_eq!(state.playlists()[0].name, "After");
    }
}
