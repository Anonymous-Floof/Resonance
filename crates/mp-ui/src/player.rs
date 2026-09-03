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
use mp_audio::queue::QueueEntry;
use mp_core::Config;
use mp_core::library::Track;
use mp_core::library::model::{AlbumId, ArtistId};

use crate::library::LibraryState;

/// What the player bar needs to know about the current track.
#[derive(Debug, Clone)]
pub struct NowPlaying {
    pub path: PathBuf,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    /// Where the artist and album names point.
    ///
    /// Carried here rather than looked up on click: the player bar is drawn
    /// every frame and the index lookup is a database query, but more to the
    /// point a name is not a key. Two artists can share one.
    pub artist_id: Option<ArtistId>,
    pub album_id: Option<AlbumId>,
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
            artist_id: None,
            album_id: None,
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
            artist_id: track.artist_id,
            album_id: track.album_id,
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

/// How much of a track has to be heard before it counts as a *play*.
///
/// Half of it, or four minutes, whichever comes first — the rule scrobblers
/// have long since converged on. A skip is not a play: counting one the moment
/// a track started made a two-second sample and a full listen
/// indistinguishable.
///
/// This threshold decides one thing only, and it is worth being blunt about
/// why. An earlier version let it decide listening *time* as well, writing the
/// track's total at the moment the bar was cleared and never adding to it
/// again — so a four-minute track played end to end was recorded as two
/// minutes, and seven minutes of listening came out as three. Whether
/// something counts as a play and how long it was listened to are different
/// questions, and only the first one has a threshold.
const PLAY_FRACTION: f64 = 0.5;
const PLAY_CEILING_SECS: f64 = 240.0;

/// The bar for a track whose duration the container never declared.
const PLAY_UNKNOWN_SECS: f64 = 30.0;

/// What a sleep timer is waiting for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Sleep {
    /// Stop after this many more seconds.
    ///
    /// Counted against wall clock rather than against playback, because a
    /// sleep timer is a promise about when the room goes quiet, not about how
    /// much music gets heard. Pausing does not buy you more time.
    In(f64),
    /// Stop when the current track finishes.
    EndOfTrack,
}

impl Sleep {
    /// Seconds left, for anything that wants to show a countdown.
    pub fn remaining(self) -> Option<f64> {
        match self {
            Self::In(secs) => Some(secs.max(0.0)),
            Self::EndOfTrack => None,
        }
    }
}

/// How much listening to bank before writing it down.
///
/// Listening is credited every frame but saved periodically: a database write
/// per frame would be absurd, and losing at most this much to a power cut is a
/// fair price. It is also flushed whenever the track changes and when the app
/// closes, so the only way to lose any at all is to be killed mid-track.
const FLUSH_SECS: f64 = 10.0;

/// Progress through the track currently open.
#[derive(Debug, Clone)]
struct Listening {
    path: PathBuf,
    /// Seconds of actual playback.
    ///
    /// Accumulated from frame time while the engine is playing rather than
    /// read from the playback position, so pausing does not keep counting and
    /// dragging the seek bar to the end does not invent listening that never
    /// happened.
    heard: f64,
    /// How much of `heard` has already been written down.
    flushed: f64,
    /// What `heard` has to reach for this to count as a play.
    threshold: f64,
    /// Whether the play has been counted.
    counted: bool,
}

impl Listening {
    fn new(path: PathBuf, duration: Option<Duration>) -> Self {
        let threshold = match duration {
            Some(duration) if duration.as_secs_f64() > 0.0 => {
                (duration.as_secs_f64() * PLAY_FRACTION).min(PLAY_CEILING_SECS)
            }
            // Nothing to take a fraction of; fall back to a flat bar.
            _ => PLAY_UNKNOWN_SECS,
        };

        Self {
            path,
            heard: 0.0,
            flushed: 0.0,
            threshold,
            counted: false,
        }
    }

    /// Listening credited but not yet written down.
    fn pending(&self) -> f64 {
        (self.heard - self.flushed).max(0.0)
    }

    /// Whether this listen has just become a play.
    fn became_a_play(&self) -> bool {
        !self.counted && self.heard >= self.threshold
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

    /// The queue in play order, as the engine last reported it.
    ///
    /// The engine owns the order — shuffle rearranges it there, and a shuffled
    /// queue reshuffles itself when it wraps — so this is a mirror of what it
    /// published, never something the UI computes for itself.
    queue: Vec<QueueEntry>,

    /// Index into the engine's track list of whatever is playing.
    current_index: Option<usize>,

    /// Bumped whenever the mirrored queue is replaced.
    ///
    /// Resolving a queue entry to a library track is a database lookup, and a
    /// panel showing the queue would otherwise redo every one of them on every
    /// frame. This lets that work be cached against something cheap to compare.
    queue_revision: u64,

    /// Progress towards counting the open track as played.
    listening: Option<Listening>,

    /// A pending sleep timer, if one is set.
    sleep: Option<Sleep>,

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
            current_index: None,
            queue_revision: 0,
            listening: None,
            sleep: None,
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
        engine.set_trim_silence(config.playback.trim_silence);
        engine.set_crossfade(
            config.playback.crossfade_seconds,
            config.playback.crossfade_curve,
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
        // Seeded in the order asked for so the transport works this frame; the
        // engine corrects it to the real play order on its next event.
        self.queue_revision = self.queue_revision.wrapping_add(1);
        self.queue = paths
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, path)| QueueEntry { index, path })
            .collect();

        self.send(Command::PlayNow {
            tracks: paths,
            start,
        });
    }

    /// Append to the queue without disturbing what is playing.
    pub fn enqueue(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        self.wants_radio = false;
        self.send(Command::Enqueue(paths));
    }

    /// Insert so it plays directly after the current track.
    pub fn play_next(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        self.wants_radio = false;
        self.send(Command::PlayNext(paths));
    }

    /// Jump to a queue entry by its engine-side index.
    pub fn jump_to(&mut self, index: usize) {
        self.wants_radio = false;
        self.send(Command::JumpTo(index));
    }

    /// Drop one entry from the queue by its engine-side index.
    ///
    /// The engine refuses to remove the track it is playing, so this can be
    /// called for any index without the caller having to check first.
    pub fn remove_from_queue(&mut self, index: usize) {
        self.send(Command::Remove(index));
    }

    /// Move a queue entry within the play order.
    ///
    /// Positions are within the order the panel shows, not track indices.
    pub fn reorder_queue(&mut self, from: usize, to: usize) {
        self.send(Command::Reorder { from, to });
    }

    pub fn clear_queue(&mut self) {
        self.wants_radio = false;
        self.send(Command::ClearQueue);
    }

    // -- sleep timer -------------------------------------------------------

    /// Arm, re-arm, or cancel the sleep timer.
    pub fn set_sleep(&mut self, sleep: Option<Sleep>) {
        self.sleep = sleep;
    }

    /// The pending sleep timer, if any.
    pub fn sleep(&self) -> Option<Sleep> {
        self.sleep
    }

    pub fn pause(&self) {
        self.send(Command::Pause);
    }

    /// The queue in play order.
    pub fn queue(&self) -> &[QueueEntry] {
        &self.queue
    }

    /// A counter that changes whenever [`queue`](Self::queue) is replaced.
    pub fn queue_revision(&self) -> u64 {
        self.queue_revision
    }

    /// Position within [`queue`](Self::queue) of the track playing now.
    pub fn queue_cursor(&self) -> Option<usize> {
        let current = self.current_index?;
        self.queue.iter().position(|entry| entry.index == current)
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
    /// Crossfades begun this session.
    ///
    /// Surfaced in Settings because a fade is otherwise unfalsifiable: "that
    /// sounded like a hard cut" and "no fade was attempted" are the same
    /// observation to a listener, and they need completely different fixes.
    pub fn fades(&self) -> u64 {
        self.engine.as_ref().map_or(0, AudioEngine::fades)
    }

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
    /// `track_history` is the user's privacy setting. It gates the write and
    /// nothing else: listening is still measured in memory so the transport
    /// behaves identically either way, but with the setting off nothing about
    /// what was played ever reaches the disk.
    ///
    /// Returns whether anything changed that warrants a repaint.
    pub fn update(&mut self, dt: f32, library: &mut LibraryState, track_history: bool) -> bool {
        for notice in &mut self.notices {
            notice.ttl -= dt;
        }
        let expired = self.notices.len();
        self.notices.retain(|n| n.ttl > 0.0);
        let mut changed = expired != self.notices.len();

        let (playing, events) = {
            let Some(engine) = &self.engine else {
                return changed;
            };
            (engine.status().is_playing(), engine.poll_events())
        };
        changed |= !events.is_empty();

        if let Some(Sleep::In(remaining)) = self.sleep {
            let left = remaining - f64::from(dt);
            if left <= 0.0 {
                self.sleep = None;
                self.pause();
                self.notice("Sleep timer finished.".to_owned(), false);
                changed = true;
            } else {
                self.sleep = Some(Sleep::In(left));
                changed = true;
            }
        }

        // Credited before the events are drained, so a track that finishes in
        // this frame is paid for the frame it finished in rather than losing
        // it to the `TrackStarted` that replaces the accumulator.
        if playing && let Some(listening) = &mut self.listening {
            listening.heard += f64::from(dt);
        }

        // Two separate facts, deliberately. Passing the bar records a play,
        // once. Time keeps accumulating either way — thirty seconds of a track
        // you skipped is still thirty seconds you spent listening.
        if let Some(listening) = &mut self.listening
            && listening.became_a_play()
        {
            listening.counted = true;
            let path = listening.path.clone();

            if track_history {
                library.record_play(&path);
            }
            changed = true;
        }

        if let Some(listening) = &mut self.listening
            && listening.pending() >= FLUSH_SECS
        {
            let pending = listening.pending();
            listening.flushed = listening.heard;
            let path = listening.path.clone();

            if track_history {
                library.add_listening(&path, pending);
            }
        }

        for event in events {
            match event {
                Event::TrackStarted {
                    path,
                    index,
                    duration,
                } => {
                    // The outgoing track's last few seconds, before the
                    // accumulator is replaced and they are lost.
                    flush(&mut self.listening, library, track_history);

                    // A track starting means the one the user meant to fall
                    // asleep to has ended. Stop before the new one is heard.
                    if self.sleep == Some(Sleep::EndOfTrack) {
                        self.sleep = None;
                        self.pause();
                        self.notice("Sleep timer finished.".to_owned(), false);
                    }

                    self.now_playing = Some(match library.track_at_path(&path) {
                        Some(track) => NowPlaying::from_track(&track, duration),
                        None => NowPlaying::from_path(path.clone(), duration),
                    });
                    self.current_index = Some(index);

                    // A track that was not heard for long enough simply does
                    // not count: the accumulator is replaced, never flushed.
                    self.listening = Some(Listening::new(path.clone(), duration));
                    self.last_played = Some(path);
                }

                Event::QueueFinished => {
                    // Nothing more will play, so the timer has nothing left to
                    // wait for. Leaving it armed would stop a queue the user
                    // starts an hour later.
                    self.sleep = None;
                    flush(&mut self.listening, library, track_history);
                    self.now_playing = None;
                    self.current_index = None;
                    self.listening = None;
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

                Event::QueueChanged { entries } => {
                    self.queue = entries;
                    self.queue_revision = self.queue_revision.wrapping_add(1);
                }
            }
        }

        changed
    }

    /// Write down any listening banked but not yet saved.
    ///
    /// Called on the way out, so the tail of whatever was playing when the
    /// window closed is not rounded away by the flush interval.
    pub fn flush_listening(&mut self, library: &mut LibraryState, track_history: bool) {
        flush(&mut self.listening, library, track_history);
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

/// Save whatever listening `listening` has banked.
///
/// Free-standing rather than a method so it can be called from inside the
/// event loop, which already holds a borrow of the player.
///
/// The bank is cleared even when history is switched off, so that turning the
/// setting back on does not suddenly commit everything listened to while it
/// was off.
fn flush(listening: &mut Option<Listening>, library: &mut LibraryState, track_history: bool) {
    let Some(listening) = listening else {
        return;
    };

    let pending = listening.pending();
    if pending <= 0.0 {
        return;
    }
    listening.flushed = listening.heard;

    if track_history {
        library.add_listening(&listening.path, pending);
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

    fn listening(secs: Option<f64>) -> Listening {
        Listening::new(
            PathBuf::from("track.flac"),
            secs.map(Duration::from_secs_f64),
        )
    }

    /// Play `seconds` of `listen`, flushing whenever enough has banked up.
    ///
    /// Mirrors what `update` does per frame, so the tests exercise the real
    /// accounting rather than a simplified version of it.
    fn play_for(listen: &mut Listening, seconds: f64) -> f64 {
        let mut saved = 0.0;
        let frame: f64 = 1.0 / 60.0;
        let mut elapsed = 0.0;

        while elapsed < seconds {
            let step = frame.min(seconds - elapsed);
            listen.heard += step;
            elapsed += step;

            if listen.pending() >= FLUSH_SECS {
                saved += listen.pending();
                listen.flushed = listen.heard;
            }
        }

        // What the flush at the end of the track would write.
        saved += listen.pending();
        listen.flushed = listen.heard;
        saved
    }

    #[test]
    fn a_track_played_in_full_is_credited_in_full() {
        // The exact case that exposed the old model: a four-minute track and a
        // three-minute one, both played end to end, reported as three minutes
        // in total instead of seven.
        let mut first = listening(Some(240.0));
        let mut second = listening(Some(180.0));

        let total = play_for(&mut first, 240.0) + play_for(&mut second, 180.0);

        assert!(
            (total - 420.0).abs() < 0.01,
            "seven minutes played must credit seven minutes, got {total}"
        );
    }

    #[test]
    fn crossing_the_play_threshold_does_not_stop_the_clock() {
        // The precise defect: passing the bar used to write the total once and
        // never add to it again, so the second half of every track vanished.
        let mut listen = listening(Some(240.0));

        play_for(&mut listen, 120.0);
        assert!(listen.became_a_play(), "half way is a play");
        listen.counted = true;

        let after = play_for(&mut listen, 120.0);
        assert!(
            (after - 120.0).abs() < 0.01,
            "the second half must still be credited, got {after}"
        );
        assert!((listen.heard - 240.0).abs() < 0.01);
    }

    #[test]
    fn listening_is_credited_in_full_even_when_it_never_becomes_a_play() {
        // Thirty seconds of a ten-minute track is not a play, but it is
        // still thirty seconds of listening.
        let mut listen = listening(Some(600.0));
        let saved = play_for(&mut listen, 30.0);

        assert!(!listen.became_a_play(), "nowhere near the threshold");
        assert!((saved - 30.0).abs() < 0.01, "got {saved}");
    }

    #[test]
    fn nothing_is_ever_saved_twice() {
        let mut listen = listening(Some(600.0));

        let first = play_for(&mut listen, 100.0);
        let second = play_for(&mut listen, 0.0);

        assert!((first - 100.0).abs() < 0.01);
        assert!(
            second.abs() < f64::EPSILON,
            "a flush with nothing banked must write nothing, got {second}"
        );
    }

    #[test]
    fn a_repeated_track_keeps_accumulating() {
        // Repeat-one restarts playback without a new track, so the same
        // accumulator keeps running. An hour on one track is an hour.
        let mut listen = listening(Some(180.0));
        let mut total = 0.0;
        for _ in 0..20 {
            total += play_for(&mut listen, 180.0);
        }

        assert!((total - 3_600.0).abs() < 0.1, "got {total}");
    }

    #[test]
    fn a_sleep_timer_reports_what_is_left() {
        assert_eq!(Sleep::In(90.0).remaining(), Some(90.0));
        assert_eq!(Sleep::EndOfTrack.remaining(), None);
    }

    #[test]
    fn a_sleep_timer_never_reports_negative_time() {
        // The countdown clears itself at zero, but a frame long enough to
        // overshoot must not surface as a negative number in the UI.
        assert_eq!(Sleep::In(-3.0).remaining(), Some(0.0));
    }

    #[test]
    fn sleep_modes_are_distinguishable() {
        // The end-of-track arm and the countdown arm are checked separately in
        // `update`; conflating them would stop playback at the wrong moment.
        assert_ne!(Sleep::EndOfTrack, Sleep::In(0.0));
        assert_eq!(Sleep::In(60.0), Sleep::In(60.0));
    }

    #[test]
    fn a_normal_track_counts_as_a_play_at_half_way() {
        let mut listen = listening(Some(200.0));
        assert!((listen.threshold - 100.0).abs() < f64::EPSILON);

        listen.heard = 99.0;
        assert!(
            !listen.became_a_play(),
            "a skip before half way is not a play"
        );

        listen.heard = 100.0;
        assert!(listen.became_a_play());
    }

    #[test]
    fn a_long_track_counts_after_four_minutes() {
        // An hour-long mix should not need thirty minutes to count.
        let listen = listening(Some(3_600.0));
        assert!((listen.threshold - PLAY_CEILING_SECS).abs() < f64::EPSILON);
    }

    #[test]
    fn a_track_with_no_declared_duration_uses_a_flat_bar() {
        for duration in [None, Some(0.0)] {
            let listen = listening(duration);
            assert!(
                (listen.threshold - PLAY_UNKNOWN_SECS).abs() < f64::EPSILON,
                "{duration:?} should fall back rather than produce a zero bar"
            );
        }
    }

    #[test]
    fn a_zero_length_track_never_counts_by_accident() {
        // A duration of zero taking a fraction of itself would give a
        // threshold of zero, which every track clears instantly.
        let listen = listening(Some(0.0));
        assert!(listen.threshold > 0.0);
        assert!(!listen.became_a_play(), "nothing has been heard yet");
    }

    #[test]
    fn a_play_is_only_counted_once() {
        let mut listen = listening(Some(60.0));
        listen.heard = 40.0;
        assert!(listen.became_a_play());

        listen.counted = true;
        listen.heard = 60.0;
        assert!(
            !listen.became_a_play(),
            "holding a finished track must not count it again"
        );
    }
}
