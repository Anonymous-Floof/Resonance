//! The UI's view of playback: the engine handle and the cached "what is
//! playing" state that the engine reports through events.
//!
//! Kept out of `app.rs` so the shell stays about layout while this stays about
//! state, and out of `library.rs` because playback and the index are genuinely
//! separate concerns — the queue is a list of paths, and it neither knows nor
//! cares whether those paths are indexed.
//!
//! The UI never blocks on the engine: commands are fire-and-forget, and
//! everything read back is either an atomic or an event drained once a frame.

use std::path::{Path, PathBuf};
use std::time::Duration;

use mp_audio::engine::{AudioEngine, Command, Event};
use mp_core::Config;
use mp_core::library::Track;

use crate::library::LibraryState;

/// What the player bar needs to know about the current track.
#[derive(Debug, Clone)]
pub struct NowPlaying {
    pub path: PathBuf,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    /// Cover for the player bar, resolved from the index.
    pub art_id: Option<String>,
    pub duration: Option<Duration>,
}

impl NowPlaying {
    /// Fall back to the filename when the track is not in the index — playing a
    /// file from outside the library has to still show something sensible.
    fn from_path(path: PathBuf, duration: Option<Duration>) -> Self {
        let title = path.file_stem().map_or_else(
            || path.display().to_string(),
            |s| s.to_string_lossy().into(),
        );
        Self {
            path,
            title,
            artist: mp_core::library::model::UNKNOWN_ARTIST.to_owned(),
            album: None,
            art_id: None,
            duration,
        }
    }

    fn from_track(track: &Track, duration: Option<Duration>) -> Self {
        Self {
            path: track.path.clone(),
            title: track.title.clone(),
            artist: track.artist.clone(),
            album: (track.album != mp_core::library::model::UNKNOWN_ALBUM)
                .then(|| track.album.clone()),
            art_id: track.art_id.clone(),
            // The engine's measured duration is authoritative; the tag can lie.
            duration: duration.or(track.duration),
        }
    }

    /// Second line of the player bar.
    pub fn subtitle(&self) -> String {
        match &self.album {
            Some(album) => format!("{} — {album}", self.artist),
            None => self.artist.clone(),
        }
    }
}

/// A transient message shown to the user, e.g. a file that would not play.
#[derive(Debug, Clone)]
pub struct Notice {
    pub text: String,
    pub is_error: bool,
    /// Seconds remaining before it fades. Counted down in the UI loop.
    pub ttl: f32,
}

/// Everything the UI owns about playback.
pub struct Player {
    /// `None` when the audio device could not be opened; the UI stays usable
    /// and shows why rather than refusing to start.
    engine: Option<AudioEngine>,
    /// Why the engine is missing, for the UI to display.
    pub engine_error: Option<String>,

    pub now_playing: Option<NowPlaying>,
    pub notices: Vec<Notice>,

    /// True while the user drags the seek bar, so playback position does not
    /// fight the handle for control of it.
    pub scrubbing: Option<f32>,

    /// The paths currently queued, so the list can show what is coming.
    queue: Vec<PathBuf>,

    /// The track auto-radio should continue from.
    ///
    /// Remembered separately because by the time the queue has finished,
    /// `now_playing` has been cleared — and "what were we listening to" is
    /// exactly the question radio needs answered.
    last_played: Option<PathBuf>,

    /// Set when the queue ran out and radio should top it up.
    ///
    /// A flag rather than an immediate call: the event arrives inside
    /// `update`, which does not have the library handle mutably, and topping
    /// up needs to read the index.
    wants_radio: bool,
}

impl Player {
    pub fn new(config: &Config) -> Self {
        let (engine, engine_error) = match AudioEngine::new(&config.playback) {
            Ok(engine) => (Some(engine), None),
            Err(err) => {
                tracing::error!("audio engine unavailable: {err:#}");
                (None, Some(err.to_string()))
            }
        };

        let player = Self {
            engine,
            engine_error,
            now_playing: None,
            notices: Vec::new(),
            scrubbing: None,
            queue: Vec::new(),
            last_played: None,
            wants_radio: false,
        };

        // The engine starts neutral, so the saved settings have to be pushed
        // to it before anything plays — otherwise the first track of every
        // session ignores the equalizer until something is touched.
        player.apply_dsp_settings(config);
        player
    }

    /// Send the equalizer and level-correction settings to the engine.
    ///
    /// Called at startup and whenever either changes. Cheap: the engine
    /// recomputes coefficients once and the callback picks them up on its next
    /// buffer.
    pub fn apply_dsp_settings(&self, config: &Config) {
        let Some(engine) = &self.engine else { return };
        engine.set_equalizer(&config.equalizer);
        engine.set_replay_gain(
            config.playback.replay_gain,
            config.playback.replay_gain_fallback_db,
        );
    }

    /// Whether the limiter is currently reducing gain.
    pub fn is_limiting(&self) -> bool {
        self.engine.as_ref().is_some_and(AudioEngine::is_limiting)
    }

    pub fn engine(&self) -> Option<&AudioEngine> {
        self.engine.as_ref()
    }

    fn send(&self, command: Command) {
        if let Some(engine) = &self.engine {
            engine.send(command);
        }
    }

    // -- transport ---------------------------------------------------------

    /// Play `paths` starting at `start`.
    ///
    /// The whole visible list is queued rather than the single track, so
    /// pressing play on a row inside an album continues through that album.
    pub fn play(&mut self, paths: Vec<PathBuf>, start: usize) {
        // Whatever radio was about to do, the user has just said otherwise.
        self.wants_radio = false;

        if paths.is_empty() || start >= paths.len() {
            return;
        }
        self.queue = paths.clone();
        self.send(Command::PlayNow {
            tracks: paths,
            start,
        });
    }

    pub fn queue_len(&self) -> usize {
        self.queue.len()
    }

    pub fn toggle_play_pause(&mut self) {
        self.send(Command::TogglePlayPause);
    }

    /// Whether the transport has anything to act on.
    pub fn has_queue(&self) -> bool {
        !self.queue.is_empty()
    }

    pub fn next(&self) {
        self.send(Command::Next);
    }

    pub fn previous(&self) {
        self.send(Command::Previous);
    }

    pub fn seek_fraction(&self, fraction: f32) {
        self.send(Command::SeekFraction(fraction));
    }

    pub fn set_volume(&self, slider: f32) {
        self.send(Command::SetVolume(slider));
    }

    pub fn set_muted(&self, muted: bool) {
        self.send(Command::SetMuted(muted));
    }

    pub fn set_repeat(&self, mode: mp_core::config::RepeatMode) {
        self.send(Command::SetRepeat(mode));
    }

    pub fn set_shuffle(&self, mode: mp_core::config::ShuffleMode) {
        self.send(Command::SetShuffle(mode));
    }

    pub fn reopen_device(&self, name: Option<String>, buffer_frames: Option<u32>) {
        self.send(Command::ReopenDevice {
            name,
            buffer_frames,
        });
    }

    // -- state read back ---------------------------------------------------

    pub fn is_playing(&self) -> bool {
        self.engine
            .as_ref()
            .is_some_and(|e| e.status().is_playing())
    }

    pub fn position_secs(&self) -> f64 {
        self.engine.as_ref().map_or(0.0, AudioEngine::position_secs)
    }

    pub fn duration_secs(&self) -> Option<f64> {
        self.engine.as_ref().and_then(AudioEngine::duration_secs)
    }

    /// Seek-bar position, showing the drag target while the user is scrubbing.
    pub fn progress(&self) -> f32 {
        self.scrubbing
            .unwrap_or_else(|| self.engine.as_ref().map_or(0.0, AudioEngine::progress))
    }

    /// Underrun count, for the debug overlay.
    pub fn xruns(&self) -> u64 {
        self.engine.as_ref().map_or(0, AudioEngine::xruns)
    }

    /// Path of the playing track, for highlighting it in any list.
    pub fn current_path(&self) -> Option<&Path> {
        self.now_playing.as_ref().map(|n| n.path.as_path())
    }

    // -- per-frame work ----------------------------------------------------

    /// Drain engine events. Call once per frame.
    ///
    /// Takes the library so a track that starts playing can be shown with its
    /// real title and cover rather than its filename.
    ///
    /// Returns whether anything changed that warrants a repaint.
    pub fn update(&mut self, dt: f32, library: &mut LibraryState) -> bool {
        for notice in &mut self.notices {
            notice.ttl -= dt;
        }
        let expired = self.notices.len();
        self.notices.retain(|n| n.ttl > 0.0);
        let mut changed = expired != self.notices.len();

        let Some(engine) = &self.engine else {
            return changed;
        };

        let events = engine.poll_events();
        changed |= !events.is_empty();

        for event in events {
            match event {
                Event::TrackStarted { path, duration, .. } => {
                    self.now_playing = Some(match library.track_at_path(&path) {
                        Some(track) => NowPlaying::from_track(&track, duration),
                        None => NowPlaying::from_path(path.clone(), duration),
                    });
                    library.record_play(&path);
                    self.last_played = Some(path);
                }

                Event::QueueFinished => {
                    self.now_playing = None;
                    self.wants_radio = true;
                }

                Event::TrackFailed { path, reason } => {
                    self.notice(
                        format!("Could not play {}: {reason}", file_label(&path)),
                        true,
                    );
                }

                Event::DeviceChanged { name, sample_rate } => {
                    tracing::info!("now using {name} at {sample_rate} Hz");
                }

                Event::QueueChanged { .. } => {}
            }
        }

        changed
    }

    /// Whether the queue has run out and radio should continue it.
    pub fn wants_radio(&self) -> bool {
        self.wants_radio
    }

    /// The track radio should continue from.
    pub fn radio_seed(&self) -> Option<&Path> {
        self.last_played.as_deref()
    }

    /// Continue playing from `tracks`, chosen by the caller.
    ///
    /// Clears the request whether or not anything was found, so a library with
    /// nothing to suggest asks once and then stops rather than retrying on
    /// every frame forever.
    pub fn continue_with(&mut self, tracks: Vec<PathBuf>) {
        self.wants_radio = false;

        if tracks.is_empty() {
            return;
        }

        self.play(tracks, 0);
    }

    /// Abandon a pending radio request — the user started something else.
    pub fn cancel_radio(&mut self) {
        self.wants_radio = false;
    }

    pub fn notice(&mut self, text: String, is_error: bool) {
        tracing::info!("{text}");
        self.notices.push(Notice {
            text,
            is_error,
            ttl: 6.0,
        });
    }
}

/// A track's filename without its extension.
fn file_label(path: &Path) -> String {
    path.file_stem().map_or_else(
        || path.display().to_string(),
        |s| s.to_string_lossy().into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(album: &str) -> Track {
        Track {
            id: 1,
            path: PathBuf::from("a.mp3"),
            title: "Title".into(),
            artist: "Artist".into(),
            album: album.into(),
            album_id: None,
            artist_id: None,
            track_no: None,
            disc_no: None,
            year: None,
            duration: Some(Duration::from_secs(200)),
            art_id: None,
            tagged: true,
            play_count: 0,
        }
    }

    /// A track with no real album should not show the placeholder name in the
    /// player bar - it would read as an album called "Singles & Loose Tracks".
    #[test]
    fn a_loose_track_shows_only_its_artist() {
        let now = NowPlaying::from_track(&track(mp_core::library::model::UNKNOWN_ALBUM), None);
        assert_eq!(now.album, None);
        assert_eq!(now.subtitle(), "Artist");
    }

    #[test]
    fn an_album_track_shows_both() {
        let now = NowPlaying::from_track(&track("Quiet Machine"), None);
        assert_eq!(now.subtitle(), "Artist — Quiet Machine");
    }

    /// The engine measures the stream; a tag is only a claim about it.
    #[test]
    fn the_measured_duration_wins_over_the_tag() {
        let measured = Duration::from_secs(123);
        let now = NowPlaying::from_track(&track("X"), Some(measured));
        assert_eq!(now.duration, Some(measured));

        let untimed = NowPlaying::from_track(&track("X"), None);
        assert_eq!(untimed.duration, Some(Duration::from_secs(200)));
    }

    #[test]
    fn a_file_outside_the_library_still_gets_a_name() {
        let now = NowPlaying::from_path(PathBuf::from(r"D:\loose\Some Song.mp3"), None);
        assert_eq!(now.title, "Some Song");
        assert_eq!(now.artist, mp_core::library::model::UNKNOWN_ARTIST);
    }
}
