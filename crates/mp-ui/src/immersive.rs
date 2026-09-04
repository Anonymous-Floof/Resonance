//! State for the full-screen now-playing view.
//!
//! Everything here exists to keep work off the frame that opens the view.
//! Lyrics live in the audio file's tags or in a sidecar beside it, so finding
//! them means touching the disk — cheap, but not free, and certainly not
//! something to repeat sixty times a second. They are read once per track and
//! only while the view is actually open, since a user who never opens it
//! should never pay for it.
//!
//! ## Disk first, and only then the network
//!
//! When the setting is on and the track has nothing on disk, the lookup is
//! handed to [`LyricsJob`] and this carries on without it. Local lyrics are
//! always preferred and are never overwritten by a fetch: a `.lrc` the user
//! put there themselves is the answer they chose, and a service's copy does
//! not get to replace it.
//!
//! The view is the only thing that triggers a fetch. Nothing is looked up for
//! a track merely because it played — a user who never opens this screen never
//! causes a request, which keeps the traffic proportional to the interest.

use std::path::{Path, PathBuf};
use std::time::Duration;

use mp_core::library::Lyrics;
use mp_core::library::lyrics;

use crate::lyrics_job::LyricsJob;
use crate::player::NowPlaying;

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

    /// Whether a fetch is out for the track on screen.
    ///
    /// Drives the "Looking for lyrics…" line, and stops the answer being
    /// looked for on every frame of every track that will never have one.
    awaiting: bool,
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
    ///
    /// A lookup in flight counts: the pane opens to say it is looking, so the
    /// words do not appear out of an empty screen a second later with no
    /// explanation of where they came from.
    pub fn shows_lyrics(&self) -> bool {
        self.lyrics_pane && (self.has_lyrics() || self.is_awaiting_lyrics())
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

    /// Whether the words on screen came from off the machine.
    pub fn lyrics_are_fetched(&self) -> bool {
        self.lyrics.as_ref().is_some_and(|l| l.source.is_fetched())
    }

    /// Whether a lookup is out for the track on screen.
    pub fn is_awaiting_lyrics(&self) -> bool {
        self.awaiting && self.lyrics.is_none()
    }

    /// Load lyrics for the track on screen, if they are not already loaded.
    ///
    /// Called every frame while the view is open; does disk work only when the
    /// track has actually changed. Nothing happens while the view is closed,
    /// which is the whole reason lyrics are not part of the library index.
    ///
    /// `fetcher` is `None` when online lyrics are switched off, which is the
    /// default. With it `None` this behaves exactly as the offline build does:
    /// the disk is consulted and that is the end of it.
    pub fn observe(&mut self, playing: Option<&NowPlaying>, fetcher: Option<&mut LyricsJob>) {
        if !self.open {
            // Dropped rather than kept: reopening on the same track costs one
            // file read, and holding a stale set risks showing the wrong words
            // for a moment on the way back in.
            self.loaded_for = None;
            self.lyrics = None;
            self.awaiting = false;
            return;
        }

        let path = playing.map(|track| track.path.as_path());

        if self.loaded_for.as_deref() != path {
            self.loaded_for = path.map(Path::to_path_buf);
            self.lyrics = path.and_then(lyrics::for_track);
            self.scrolled_to = None;
            self.awaiting = false;

            // Local lyrics win, and nothing is asked when they exist.
            if self.lyrics.is_none()
                && let Some(track) = playing
                && let Some(fetcher) = fetcher
            {
                fetcher.want(track);
                self.take_fetched(track.path.as_path(), fetcher);
            }

            return;
        }

        // Same track as last frame. An answer may have landed since.
        if self.awaiting
            && let Some(path) = path
            && let Some(fetcher) = fetcher
        {
            self.take_fetched(path, fetcher);
        }
    }

    /// Pick up a fetched answer, if the worker has produced one yet.
    fn take_fetched(&mut self, path: &Path, fetcher: &LyricsJob) {
        match fetcher.answer(path) {
            Some(found) => {
                self.lyrics = found.clone();
                self.scrolled_to = None;
                self.awaiting = false;
            }
            None => self.awaiting = fetcher.is_waiting_for(path),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track() -> PathBuf {
        PathBuf::from("C:/music/Song.mp3")
    }

    /// A playing track with no lyrics anywhere near it. Every test here runs
    /// with `fetcher: None`, which is the default configuration and the one
    /// that behaves exactly as the offline build does.
    fn playing(path: &Path) -> NowPlaying {
        NowPlaying {
            path: path.to_path_buf(),
            title: "Song".to_owned(),
            artist: "Someone".to_owned(),
            album: None,
            artist_id: None,
            album_id: None,
            art_id: None,
            duration: None,
        }
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
        state.observe(Some(&playing(&path)), None);

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
        state.observe(Some(&playing(&path)), None);

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
        state.observe(Some(&playing(&first)), None);
        state.lyrics = Some(lyrics::parse("Stale", lyrics::Source::Embedded));

        state.observe(Some(&playing(&second)), None);

        assert_eq!(state.loaded_for.as_deref(), Some(second.as_path()));
        assert!(
            state.lyrics().is_none(),
            "the previous track's words must not linger"
        );
    }

    /// A `.lrc` the user put there themselves is the answer they chose. A
    /// service's copy does not get to replace it, and no request is made for a
    /// track that already has words on disk.
    #[test]
    fn local_lyrics_win_and_nothing_is_asked_for_them() {
        let dir = tempfile::tempdir().expect("temp dir");
        let song = dir.path().join("Song.mp3");
        std::fs::write(&song, b"not really audio").expect("write");
        std::fs::write(song.with_extension("lrc"), "[00:01.00]Mine").expect("write");

        let mut fetcher = LyricsJob::start(
            dir.path().to_path_buf(),
            std::sync::Arc::new(mp_net::Activity::in_memory()),
            egui::Context::default(),
        )
        .expect("start");

        let mut state = Immersive::new();
        state.open(Some(&song));
        state.observe(Some(&playing(&song)), Some(&mut fetcher));

        assert!(state.has_lyrics());
        assert!(
            !state.lyrics_are_fetched(),
            "the sidecar should have been used"
        );
        assert!(
            !fetcher.is_waiting_for(&song),
            "nothing should have been asked about a track that has lyrics"
        );
        assert!(!state.is_awaiting_lyrics());
    }

    /// With online lyrics off — the default — this must behave exactly as the
    /// offline build does: consult the disk, and stop there.
    #[test]
    fn with_no_fetcher_nothing_waits_on_anything() {
        let mut state = Immersive::new();
        let path = PathBuf::from("no-such-folder/Nothing.mp3");

        state.open(Some(&path));
        state.observe(Some(&playing(&path)), None);

        assert!(state.lyrics().is_none());
        assert!(!state.is_awaiting_lyrics(), "there is nothing to wait for");
        assert!(!state.shows_lyrics());
    }

    /// The pane opens while the lookup is out, so the words do not appear from
    /// an empty column a second later with no explanation.
    #[test]
    fn the_pane_opens_to_say_it_is_looking() {
        let mut state = Immersive::new();
        state.lyrics_pane = true;
        state.awaiting = true;

        assert!(!state.has_lyrics());
        assert!(state.is_awaiting_lyrics());
        assert!(state.shows_lyrics(), "the pane should say it is looking");
    }

    /// Once words arrive, the waiting state must clear or the pane keeps
    /// claiming to be looking for something it already has.
    #[test]
    fn finding_words_ends_the_waiting() {
        let mut state = Immersive::new();
        state.awaiting = true;
        state.lyrics = Some(lyrics::parse(
            "[00:01.00]Found",
            lyrics::Source::Fetched("LRCLIB".to_owned()),
        ));

        assert!(!state.is_awaiting_lyrics());
        assert!(state.lyrics_are_fetched(), "and it should say where from");
    }

    /// Closing the view must drop the pending state too, or reopening shows a
    /// stale "Looking for lyrics" for a track that was resolved long ago.
    #[test]
    fn closing_forgets_that_it_was_waiting() {
        let mut state = Immersive::new();
        let path = track();

        state.open(Some(&path));
        state.awaiting = true;

        state.close();
        state.observe(Some(&playing(&path)), None);

        assert!(!state.is_awaiting_lyrics());
    }
}
