//! Fetching lyrics without the interface ever waiting for them.
//!
//! A request takes anywhere from thirty milliseconds to the ten-second
//! timeout, and the rate limiter can add half a second on top. None of that
//! may happen on the frame that opens the full-screen view, so the lookup goes
//! to a worker thread and the answer arrives whenever it arrives.
//!
//! ## The repaint is the part that is easy to get wrong
//!
//! egui only draws when something asks it to. A window sitting idle with the
//! lyrics pane open produces no frames at all, so an answer landing in the
//! channel would sit there unnoticed until the user happened to move the
//! mouse. The worker therefore calls `request_repaint` after every result —
//! that one line is what makes the words appear on their own.
//!
//! ## Asked once, and only once
//!
//! [`LyricsJob`] remembers every track it has already resolved this session,
//! misses included. Closing and reopening the view, or coming back to a track
//! later, costs nothing and produces no second entry in the activity log. The
//! on-disk cache in `mp-net` covers the same ground across restarts; this
//! covers it within one.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};

use mp_core::library::Lyrics;
use mp_core::library::{lyrics, names};
use mp_net::Activity;
use mp_net::lyrics::{Client, Match, Query};

use crate::player::NowPlaying;

/// What the worker is asked to do.
struct Job {
    path: PathBuf,
    query: Query,
    /// Carried per job rather than read on the worker, so a job already in
    /// flight finishes under the policy it was sent with.
    matching: Match,
}

/// What it sends back. `None` means asked and answered with nothing.
type Answer = (PathBuf, Option<Lyrics>);

/// A worker thread that looks up lyrics, and a memo of what it has found.
pub struct LyricsJob {
    jobs: Sender<Job>,
    answers: Receiver<Answer>,
    /// Every track resolved this session. The value is the answer, which may
    /// legitimately be "there are none".
    known: HashMap<PathBuf, Option<Lyrics>>,
    /// Tracks currently in flight, so one is not asked about twice.
    pending: HashSet<PathBuf>,
    /// How hard to look, from the user's settings.
    matching: Match,
}

impl std::fmt::Debug for LyricsJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LyricsJob")
            .field("known", &self.known.len())
            .field("pending", &self.pending.len())
            .finish()
    }
}

impl LyricsJob {
    /// Start the worker.
    ///
    /// `ctx` is cloned and kept so a landing answer can ask for a frame; see
    /// the note on repainting above. Returns `None` if the thread will not
    /// start, in which case the app simply has no online lyrics.
    pub fn start(cache_root: PathBuf, activity: Arc<Activity>, ctx: egui::Context) -> Option<Self> {
        let (jobs, incoming) = std::sync::mpsc::channel::<Job>();
        let (outgoing, answers) = std::sync::mpsc::channel::<Answer>();

        let spawned = std::thread::Builder::new()
            .name("resonance-lyrics".into())
            .spawn(move || {
                // Built on this thread: it owns the transport, and nothing on
                // the UI side should be able to reach it.
                let client = Client::new(&cache_root, activity);

                // Ends when the sender is dropped, which is when the app quits.
                while let Ok(job) = incoming.recv() {
                    let found = client.fetch(&job.query, job.matching).and_then(|fetched| {
                        let text = fetched.best()?;
                        Some(lyrics::parse(
                            text,
                            lyrics::Source::Fetched(client.source().label.to_owned()),
                        ))
                    });

                    if outgoing.send((job.path, found)).is_err() {
                        break;
                    }

                    // Without this the answer waits for the user to jog the
                    // mouse. An idle egui window draws nothing at all.
                    ctx.request_repaint();
                }
            });

        if let Err(err) = spawned {
            tracing::error!("could not start the lyrics thread: {err}");
            return None;
        }

        Some(Self {
            jobs,
            answers,
            known: HashMap::new(),
            pending: HashSet::new(),
            matching: Match::default(),
        })
    }

    /// Collect anything the worker has finished. Call once per frame.
    ///
    /// Returns whether anything landed, so the caller can repaint.
    pub fn poll(&mut self) -> bool {
        let mut landed = false;

        loop {
            match self.answers.try_recv() {
                Ok((path, found)) => {
                    self.pending.remove(&path);
                    self.known.insert(path, found);
                    landed = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }

        landed
    }

    /// Ask about a track, unless it is already known or already in flight.
    pub fn want(&mut self, track: &NowPlaying) {
        if self.known.contains_key(&track.path) || self.pending.contains(&track.path) {
            return;
        }

        let query = query_for(track);
        if !query.is_answerable() {
            // Remembered as a miss so it is not reconsidered every frame. It
            // never reaches the network, and never reaches the log.
            self.known.insert(track.path.clone(), None);
            return;
        }

        self.pending.insert(track.path.clone());

        if self
            .jobs
            .send(Job {
                path: track.path.clone(),
                query,
                matching: self.matching,
            })
            .is_err()
        {
            // The worker is gone. Record a miss so the interface stops asking.
            self.pending.remove(&track.path);
            self.known.insert(track.path.clone(), None);
        }
    }

    /// Set how hard to look, from the settings.
    ///
    /// Loosening it discards the misses recorded so far, because those were
    /// decided under the stricter rule and the looser one may well find them.
    /// The hits are kept: they are already the best answer available.
    pub fn set_matching(&mut self, matching: Match) {
        if self.matching == matching {
            return;
        }

        self.matching = matching;
        self.known.retain(|_, found| found.is_some());
    }

    pub fn matching(&self) -> Match {
        self.matching
    }

    /// The answer for a track, once there is one.
    ///
    /// The outer `Option` is whether it has been resolved; the inner is
    /// whether anything was found.
    pub fn answer(&self, path: &Path) -> Option<&Option<Lyrics>> {
        self.known.get(path)
    }

    /// Whether a lookup for this track is still out.
    pub fn is_waiting_for(&self, path: &Path) -> bool {
        self.pending.contains(path)
    }
}

/// Build the question from what the player already knows about the track.
///
/// The artist and album go as they are. The title has its video decoration
/// removed first — `(Official Video)`, `[HD]`, `[dQw4w9WgXcQ]` — because
/// scanning only strips that from *filenames*, so a tagged rip keeps it, and
/// the lookup matches the title exactly.
///
/// It was worth checking rather than assuming: LRCLIB is contributed to by
/// people whose files have the same problems, so their copies might plausibly
/// carry the same decoration. They do not. A title with `(Official Video)` on
/// it misses where the same title without it is found.
///
/// [`strip_decoration`](names::strip_decoration) only removes decoration that
/// cannot name a different recording, so `(Live)`, `(Acoustic)` and `[Remix]`
/// are left on. The file itself is never touched — this cleans the question,
/// not the track.
fn query_for(track: &NowPlaying) -> Query {
    let title = names::strip_decoration(&track.title);

    // A title that was nothing but decoration is left as it was. There is no
    // answer either way, and the log should show what the file actually says.
    let title = if title.trim().is_empty() {
        track.title.clone()
    } else {
        title
    };

    let mut query = Query::new(track.artist.clone(), title);

    if let Some(album) = &track.album {
        query = query.with_album(album.clone());
    }

    if let Some(duration) = track.duration {
        query = query.with_duration(duration);
    }

    query
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn track(artist: &str, title: &str) -> NowPlaying {
        NowPlaying {
            path: PathBuf::from(format!("C:/music/{title}.mp3")),
            title: title.to_owned(),
            artist: artist.to_owned(),
            album: Some("Pablo Honey".to_owned()),
            artist_id: None,
            album_id: None,
            art_id: None,
            duration: Some(Duration::from_secs(239)),
        }
    }

    #[test]
    fn the_query_carries_what_the_player_knows() {
        let query = query_for(&track("Radiohead", "Creep"));

        assert_eq!(query.artist, "Radiohead");
        assert_eq!(query.title, "Creep");
        assert_eq!(query.album.as_deref(), Some("Pablo Honey"));
        assert_eq!(query.duration, Some(Duration::from_secs(239)));
        assert!(query.is_answerable());
    }

    #[test]
    fn a_track_with_no_artist_is_not_worth_asking_about() {
        let mut untagged = track("", "Track 03");
        untagged.album = None;
        untagged.duration = None;

        assert!(!query_for(&untagged).is_answerable());
    }

    /// The interface calls this every frame while the view is open, so an
    /// unanswerable track must be settled once rather than reconsidered
    /// sixty times a second.
    #[test]
    fn an_unanswerable_track_is_resolved_without_asking_anyone() {
        let ctx = egui::Context::default();
        let dir = tempfile::tempdir().expect("temp dir");
        let mut job = LyricsJob::start(
            dir.path().to_path_buf(),
            Arc::new(Activity::in_memory()),
            ctx,
        )
        .expect("start");

        let untagged = track("", "Track 03");
        job.want(&untagged);

        assert!(!job.is_waiting_for(&untagged.path), "nothing was sent");
        assert!(
            matches!(job.answer(&untagged.path), Some(None)),
            "it should be settled as a miss"
        );
    }

    #[test]
    fn a_track_is_only_asked_about_once() {
        let ctx = egui::Context::default();
        let dir = tempfile::tempdir().expect("temp dir");
        let mut job = LyricsJob::start(
            dir.path().to_path_buf(),
            Arc::new(Activity::in_memory()),
            ctx,
        )
        .expect("start");

        // Settled synchronously, so this needs no network and no waiting.
        let untagged = track("", "");
        job.want(&untagged);
        job.want(&untagged);
        job.want(&untagged);

        assert_eq!(job.known.len(), 1);
        assert!(job.pending.is_empty());
    }

    #[test]
    fn an_unresolved_track_has_no_answer_yet() {
        let ctx = egui::Context::default();
        let dir = tempfile::tempdir().expect("temp dir");
        let job = LyricsJob::start(
            dir.path().to_path_buf(),
            Arc::new(Activity::in_memory()),
            ctx,
        )
        .expect("start");

        assert!(job.answer(Path::new("C:/music/Never asked.mp3")).is_none());
        assert!(!job.is_waiting_for(Path::new("C:/music/Never asked.mp3")));
    }

    /// Nothing landing must not be reported as something landing, or the
    /// window repaints continuously for no reason.
    #[test]
    fn polling_an_idle_worker_reports_nothing() {
        let ctx = egui::Context::default();
        let dir = tempfile::tempdir().expect("temp dir");
        let mut job = LyricsJob::start(
            dir.path().to_path_buf(),
            Arc::new(Activity::in_memory()),
            ctx,
        )
        .expect("start");

        assert!(!job.poll());
    }

    /// Turning the fallback on has to reconsider the tracks that already
    /// missed, or the setting appears to do nothing until the app restarts.
    #[test]
    fn loosening_the_match_forgets_the_misses_it_might_now_find() {
        let ctx = egui::Context::default();
        let dir = tempfile::tempdir().expect("temp dir");
        let mut job = LyricsJob::start(
            dir.path().to_path_buf(),
            Arc::new(mp_net::Activity::in_memory()),
            ctx,
        )
        .expect("start");

        let missed = PathBuf::from("C:/music/Missed.mp3");
        let found = PathBuf::from("C:/music/Found.mp3");
        job.known.insert(missed.clone(), None);
        job.known.insert(
            found.clone(),
            Some(lyrics::parse("Words", lyrics::Source::Embedded)),
        );

        job.set_matching(Match::AnyRelease);

        assert!(
            job.answer(&missed).is_none(),
            "the miss should be open to being asked again"
        );
        assert!(
            job.answer(&found).is_some(),
            "a hit is already the best answer there is"
        );
    }

    /// Pushed from the settings every frame, so a repeat must not keep
    /// clearing the memo and re-asking for every track.
    #[test]
    fn setting_the_same_match_again_changes_nothing() {
        let ctx = egui::Context::default();
        let dir = tempfile::tempdir().expect("temp dir");
        let mut job = LyricsJob::start(
            dir.path().to_path_buf(),
            Arc::new(mp_net::Activity::in_memory()),
            ctx,
        )
        .expect("start");

        job.set_matching(Match::AnyRelease);
        job.known.insert(PathBuf::from("C:/music/Missed.mp3"), None);

        job.set_matching(Match::AnyRelease);

        assert_eq!(job.known.len(), 1, "the memo was cleared for no reason");
    }

    #[test]
    fn a_new_job_looks_for_the_exact_recording() {
        let ctx = egui::Context::default();
        let dir = tempfile::tempdir().expect("temp dir");
        let job = LyricsJob::start(
            dir.path().to_path_buf(),
            Arc::new(mp_net::Activity::in_memory()),
            ctx,
        )
        .expect("start");

        assert_eq!(job.matching(), Match::Exact);
    }

    // -- building the question ----------------------------------------------

    fn playing(title: &str) -> NowPlaying {
        NowPlaying {
            path: PathBuf::from("C:/music/Track.mp3"),
            title: title.to_owned(),
            artist: "The Living Tombstone".to_owned(),
            album: Some("Single".to_owned()),
            artist_id: None,
            album_id: None,
            art_id: None,
            duration: Some(std::time::Duration::from_secs(263)),
        }
    }

    /// Scanning strips this from filenames but not from tags, so a tagged rip
    /// carries it into a lookup that matches the title exactly.
    #[test]
    fn video_decoration_is_taken_off_the_title_before_asking() {
        assert_eq!(
            query_for(&playing("Die in a Fire (Official Video)")).title,
            "Die in a Fire"
        );
        assert_eq!(
            query_for(&playing("Die in a Fire [HD]")).title,
            "Die in a Fire"
        );
        assert_eq!(
            query_for(&playing("Die in a Fire (Lyrics) [4K]")).title,
            "Die in a Fire"
        );
    }

    /// The line between cleaning a title and asking about a different song.
    #[test]
    fn an_aside_that_names_the_recording_is_kept() {
        for title in [
            "Die in a Fire (Live)",
            "Die in a Fire (Acoustic)",
            "Die in a Fire [Remix]",
            "Die in a Fire (feat. Somebody)",
        ] {
            assert_eq!(query_for(&playing(title)).title, title);
        }
    }

    /// The file is never touched, and neither is the rest of the question.
    #[test]
    fn only_the_title_is_cleaned() {
        let query = query_for(&playing("Die in a Fire (Official Video)"));

        assert_eq!(query.artist, "The Living Tombstone");
        assert_eq!(query.album.as_deref(), Some("Single"));
        assert_eq!(query.duration, Some(std::time::Duration::from_secs(263)));
    }

    /// Stripping everything would leave nothing to ask about, and the log
    /// should show what the file actually says.
    #[test]
    fn a_title_that_is_only_decoration_is_left_as_it_is() {
        assert_eq!(
            query_for(&playing("(Official Video)")).title,
            "(Official Video)"
        );
    }

    #[test]
    fn an_ordinary_title_is_passed_through_untouched() {
        assert_eq!(query_for(&playing("Die in a Fire")).title, "Die in a Fire");
    }
}
