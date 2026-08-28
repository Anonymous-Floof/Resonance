//! State for the full-screen now-playing view.
//!
//! Everything here exists to keep work off the frame that opens the view.
//! Lyrics live in the audio file's tags or in a sidecar beside it, so finding
//! them means touching the disk — cheap, but not free, and certainly not
//! something to repeat sixty times a second. They are read once per track and
//! only while the view is actually open, since a user who never opens it
//! should never pay for it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use mp_core::library::Lyrics;
use mp_core::library::lyrics;

/// The full-screen view's state.
#[derive(Debug, Default)]
pub struct Immersive {
    open: bool,
    /// Whether the lyrics pane is showing, when there are lyrics to show.
    lyrics_pane: bool,

    /// The track the loaded lyrics belong to.
    ///
    /// Kept alongside the lyrics rather than inferred, so "we looked and found
    /// nothing" is remembered as firmly as a hit and the disk is not searched
    /// again every frame for a track that has none.
    loaded_for: Option<PathBuf>,
    lyrics: Option<Lyrics>,

    /// The line the pane was last scrolled to.
    ///
    /// Auto-scroll fires on the *change* rather than every frame. Re-centring
    /// continuously would make the pane impossible to scroll by hand: every
    /// drag would be yanked straight back to the current line.
    scrolled_to: Option<usize>,
}

impl Immersive {
    pub fn new() -> Self {
        Self {
            lyrics_pane: true,
            ..Self::default()
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Open the view, if there is anything to look at.
    ///
    /// Opening onto an empty screen would be a dead end with a close button,
    /// so with nothing playing this does nothing at all.
    pub fn open(&mut self, playing: Option<&Path>) {
        if playing.is_some() {
            self.open = true;
        }
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    pub fn toggle(&mut self, playing: Option<&Path>) {
        if self.open {
            self.close();
        } else {
            self.open(playing);
        }
    }

    /// Whether the lyrics pane is wanted *and* there is something to put in it.
    pub fn shows_lyrics(&self) -> bool {
        self.lyrics_pane && self.has_lyrics()
    }

    pub fn has_lyrics(&self) -> bool {
        self.lyrics.as_ref().is_some_and(|l| !l.is_empty())
    }

    pub fn toggle_lyrics(&mut self) {
        self.lyrics_pane = !self.lyrics_pane;
    }

    pub fn lyrics(&self) -> Option<&Lyrics> {
        self.lyrics.as_ref()
    }

    /// Whether the pane should scroll to `active` now.
    ///
    /// Answers true once per change of line, so scrolling by hand sticks until
    /// the song moves on.
    pub fn take_scroll(&mut self, active: Option<usize>) -> bool {
        if self.scrolled_to == active {
            return false;
        }
        self.scrolled_to = active;
        active.is_some()
    }

    /// Which lyric line is being sung, if the lyrics are timed.
    pub fn active_line(&self, position: Duration) -> Option<usize> {
        self.lyrics.as_ref()?.active_at(position)
    }

    /// Load lyrics for the track on screen, if they are not already loaded.
    ///
    /// Called every frame while the view is open; does disk work only when the
    /// track has actually changed. Nothing happens while the view is closed,
    /// which is the whole reason lyrics are not part of the library index.
    pub fn observe(&mut self, playing: Option<&Path>) {
        if !self.open {
            // Dropped rather than kept: reopening on the same track costs one
            // file read, and holding a stale set risks showing the wrong words
            // for a moment on the way back in.
            self.loaded_for = None;
            self.lyrics = None;
            return;
        }

        if self.loaded_for.as_deref() == playing {
            return;
        }

        self.loaded_for = playing.map(Path::to_path_buf);
        self.lyrics = playing.and_then(lyrics::for_track);
        self.scrolled_to = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track() -> PathBuf {
        PathBuf::from("C:/music/Song.mp3")
    }

    #[test]
    fn it_starts_closed() {
        let state = Immersive::new();
        assert!(!state.is_open());
        assert!(!state.has_lyrics());
    }

    /// Opening with nothing playing would show an empty screen whose only
    /// control is the way back out.
    #[test]
    fn it_will_not_open_with_nothing_playing() {
        let mut state = Immersive::new();
        state.open(None);
        assert!(!state.is_open());

        state.toggle(None);
        assert!(!state.is_open());
    }

    #[test]
    fn it_opens_and_closes_with_a_track() {
        let mut state = Immersive::new();
        let path = track();

        state.toggle(Some(&path));
        assert!(state.is_open());

        state.toggle(Some(&path));
        assert!(!state.is_open());
    }

    /// The pane can be wanted without there being anything to put in it.
    #[test]
    fn the_lyrics_pane_needs_lyrics_to_show() {
        let mut state = Immersive::new();
        let path = track();
        state.open(Some(&path));

        assert!(!state.shows_lyrics(), "there are no lyrics for this track");

        state.lyrics = Some(lyrics::parse("[00:01.00]A line", lyrics::Source::Embedded));
        assert!(state.shows_lyrics());

        state.toggle_lyrics();
        assert!(!state.shows_lyrics(), "the user put the pane away");
        assert!(state.has_lyrics(), "but the lyrics are still there");
    }

    /// Nothing touches the disk while the view is closed.
    #[test]
    fn closing_forgets_the_lyrics() {
        let mut state = Immersive::new();
        let path = track();

        state.open(Some(&path));
        state.lyrics = Some(lyrics::parse("Words", lyrics::Source::Embedded));
        state.loaded_for = Some(path.clone());

        state.close();
        state.observe(Some(&path));

        assert!(state.lyrics().is_none());
        assert!(state.loaded_for.is_none());
    }

    /// A track with no lyrics must be remembered as such, or every frame goes
    /// looking for a file that is not there.
    #[test]
    fn a_track_without_lyrics_is_only_looked_up_once() {
        let mut state = Immersive::new();
        let path = PathBuf::from("no-such-folder/Nothing.mp3");

        state.open(Some(&path));
        state.observe(Some(&path));

        assert!(state.lyrics().is_none());
        assert_eq!(
            state.loaded_for.as_deref(),
            Some(path.as_path()),
            "the miss should be recorded so it is not repeated"
        );
    }

    #[test]
    fn the_active_line_follows_the_playhead() {
        let mut state = Immersive::new();
        state.lyrics = Some(lyrics::parse(
            "[00:05.00]One\n[00:10.00]Two",
            lyrics::Source::Embedded,
        ));

        assert_eq!(state.active_line(Duration::from_secs(0)), None);
        assert_eq!(state.active_line(Duration::from_secs(7)), Some(0));
        assert_eq!(state.active_line(Duration::from_secs(30)), Some(1));
    }

    /// Auto-scroll must fire on a change of line and then stop, or scrolling
    /// by hand is impossible.
    #[test]
    fn scrolling_happens_once_per_line() {
        let mut state = Immersive::new();

        assert!(state.take_scroll(Some(3)), "the line changed");
        assert!(!state.take_scroll(Some(3)), "and has not changed since");
        assert!(state.take_scroll(Some(4)));

        // Before the first timed line there is nothing to scroll to.
        assert!(!state.take_scroll(None));
    }

    #[test]
    fn changing_track_reloads() {
        let mut state = Immersive::new();
        let first = track();
        let second = PathBuf::from("C:/music/Other.mp3");

        state.open(Some(&first));
        state.observe(Some(&first));
        state.lyrics = Some(lyrics::parse("Stale", lyrics::Source::Embedded));

        state.observe(Some(&second));

        assert_eq!(state.loaded_for.as_deref(), Some(second.as_path()));
        assert!(
            state.lyrics().is_none(),
            "the previous track's words must not linger"
        );
    }
}
