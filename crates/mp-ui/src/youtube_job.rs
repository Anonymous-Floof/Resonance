//! Turning a pasted link into something playing, without the interface ever
//! waiting for it.
//!
//! Resolving a link is a request; fetching the audio is several megabytes over
//! whatever connection the user has. Neither may happen on a frame, so both go
//! to a worker thread and the track starts whenever it starts.
//!
//! ## One at a time, deliberately
//!
//! Unlike the lyrics and artwork passes, this does one thing at once and says
//! what it is doing while it does it. A user who has just pasted a link is
//! waiting for that link, and a queue of half-finished downloads would be
//! worse than a short wait with an honest label on it.
//!
//! ## The repaint is the part that is easy to get wrong
//!
//! egui only draws when something asks it to, so a window sitting idle
//! produces no frames and an answer landing in the channel would wait for the
//! user to jog the mouse. The worker asks for a repaint at every step, which
//! is what makes the label change and the track start on their own.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};

use mp_core::library::art::ArtCache;
use mp_core::library::names;
use mp_net::Activity;
use mp_net::tool::YtDlp;
use mp_net::youtube::{Client, Query, Resolved};

use crate::player::StreamFacts;

/// How much fetched audio to keep.
///
/// A ceiling rather than a setting: the right number is "enough that replaying
/// something recent is instant, and not so much that a music player quietly
/// eats a disk", and no user wants to be asked. Two gigabytes is several
/// hundred tracks at the bitrate this fetches.
pub const CACHE_BUDGET: u64 = 2 * 1024 * 1024 * 1024;

/// Whether the program this needs is installed, and which one.
///
/// Worked out once and kept. Finding out runs the program to ask its version,
/// which is not something to do while drawing a frame.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToolStatus {
    /// Where it was found, absolute, so the settings screen can say which copy
    /// is being used rather than leaving the user to guess at their PATH.
    pub program: Option<PathBuf>,
    pub version: Option<String>,
}

impl ToolStatus {
    /// Go and look.
    pub fn look(configured: Option<&Path>) -> Self {
        let Some(tool) = YtDlp::locate(configured) else {
            return Self::default();
        };

        Self {
            version: tool.version(),
            program: Some(tool.program().to_owned()),
        }
    }

    pub fn is_installed(&self) -> bool {
        self.program.is_some()
    }

    /// One line for the settings screen.
    ///
    /// The version is worth showing because the failure it predicts is
    /// invisible otherwise: extraction breaks when the service changes, and an
    /// old copy fails on links a current one handles.
    pub fn summary(&self) -> String {
        match (&self.program, &self.version) {
            (None, _) => "yt-dlp was not found. It is not shipped with Resonance and is never downloaded for you - install it and it will be picked up.".to_owned(),
            (Some(program), None) => format!(
                "Found {}, which would not say what version it is. It may be too old to work.",
                program.display()
            ),
            (Some(program), Some(version)) => {
                format!("Using {}, version {version}.", program.display())
            }
        }
    }
}

/// What the worker is doing, for the label on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Idle,
    Resolving,
    Fetching,
}

impl Stage {
    fn from_code(code: u8) -> Self {
        match code {
            1 => Self::Resolving,
            2 => Self::Fetching,
            _ => Self::Idle,
        }
    }

    fn code(self) -> u8 {
        match self {
            Self::Idle => 0,
            Self::Resolving => 1,
            Self::Fetching => 2,
        }
    }

    /// What to say while this is happening.
    pub fn label(self) -> Option<&'static str> {
        match self {
            Self::Idle => None,
            Self::Resolving => Some("Looking up the link"),
            Self::Fetching => Some("Fetching the audio"),
        }
    }
}

/// What came back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// A file on disk, ready to play, and what is known about it.
    Ready {
        path: PathBuf,
        facts: Box<StreamFacts>,
    },
    /// Nothing playable, and why not in words the user can act on.
    Nothing { why: String },
}

/// A worker thread that turns links into files.
pub struct YoutubeJob {
    jobs: Sender<Query>,
    answers: Receiver<Answer>,
    stage: Arc<AtomicU8>,
    /// Set while something is in flight, so a second paste does not queue up
    /// behind the first without saying so.
    busy: bool,
}

impl std::fmt::Debug for YoutubeJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("YoutubeJob")
            .field("stage", &self.stage())
            .field("busy", &self.busy)
            .finish()
    }
}

impl YoutubeJob {
    /// Start the worker.
    ///
    /// Returns `None` if the thread will not start, in which case the app
    /// simply cannot play links.
    pub fn start(
        tool: YtDlp,
        cache_root: PathBuf,
        audio_dir: PathBuf,
        art_root: PathBuf,
        activity: Arc<Activity>,
        ctx: egui::Context,
    ) -> Option<Self> {
        let (jobs, incoming) = std::sync::mpsc::channel::<Query>();
        let (outgoing, answers) = std::sync::mpsc::channel::<Answer>();

        let stage = Arc::new(AtomicU8::new(Stage::Idle.code()));
        let worker_stage = Arc::clone(&stage);
        let sweep_dir = audio_dir.clone();

        let spawned = std::thread::Builder::new()
            .name("resonance-youtube".into())
            .spawn(move || {
                // Built on this thread: it owns the transport and the runner,
                // and nothing on the UI side should be able to reach either.
                let client = Client::new(Box::new(tool), cache_root, audio_dir, activity);
                let art = ArtCache::new(art_root);

                // Ends when the sender is dropped, which is when the app quits
                // or the setting is switched off.
                while let Ok(query) = incoming.recv() {
                    let answer = run(&client, &art, &sweep_dir, &query, &worker_stage, &ctx);

                    worker_stage.store(Stage::Idle.code(), Ordering::Relaxed);

                    if outgoing.send(answer).is_err() {
                        break;
                    }

                    ctx.request_repaint();
                }
            });

        if let Err(err) = spawned {
            tracing::error!("could not start the youtube thread: {err}");
            return None;
        }

        Some(Self {
            jobs,
            answers,
            stage,
            busy: false,
        })
    }

    pub fn stage(&self) -> Stage {
        Stage::from_code(self.stage.load(Ordering::Relaxed))
    }

    pub fn is_busy(&self) -> bool {
        self.busy
    }

    /// What to say while this is working, or `None` when it is idle.
    ///
    /// Driven by `busy` rather than by the stage alone. There is a moment
    /// between a link being sent and the worker picking it up where the stage
    /// is still idle, and a button that went live in that gap would accept a
    /// second link and then refuse it for the wrong reason.
    pub fn working(&self) -> Option<&'static str> {
        if !self.busy {
            return None;
        }

        Some(working_label(self.stage()))
    }

    /// Ask for a link.
    ///
    /// `false` when it will not be tried: something is already in flight, or
    /// the text does not name a video. A link that names nothing is refused
    /// here rather than sent, so nothing unrecognised is ever handed to the
    /// program.
    pub fn want(&mut self, link: &str) -> bool {
        if self.busy {
            return false;
        }

        let query = Query::new(link);
        if !query.is_answerable() {
            return false;
        }

        if self.jobs.send(query).is_err() {
            return false;
        }

        self.busy = true;
        true
    }

    /// Take whatever has landed.
    pub fn poll(&mut self) -> Option<Answer> {
        match self.answers.try_recv() {
            Ok(answer) => {
                self.busy = false;
                Some(answer)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                // The worker is gone, so nothing is coming. Clearing this is
                // what stops the interface waiting forever on a dead thread.
                self.busy = false;
                None
            }
        }
    }
}

/// What to say while a job that has been asked for is running.
///
/// Separate from [`YoutubeJob::working`] so the awkward case can be tested
/// without a thread: a link that has been sent but not yet picked up is busy
/// while its stage is still idle, and it still needs a label or the button
/// goes live and takes a second link.
fn working_label(stage: Stage) -> &'static str {
    stage.label().unwrap_or("Working")
}

/// One link, start to finish.
fn run(
    client: &Client,
    art: &ArtCache,
    audio_dir: &Path,
    query: &Query,
    stage: &AtomicU8,
    ctx: &egui::Context,
) -> Answer {
    stage.store(Stage::Resolving.code(), Ordering::Relaxed);
    ctx.request_repaint();

    let Some(resolved) = client.resolve(query) else {
        return Answer::Nothing {
            why: "Could not read that link. It may be private, removed, or offer no audio this build can play - the activity log says which.".to_owned(),
        };
    };

    stage.store(Stage::Fetching.code(), Ordering::Relaxed);
    ctx.request_repaint();

    let Some(audio) = client.fetch_audio(&resolved) else {
        return Answer::Nothing {
            why: format!(
                "Found \"{}\" but could not fetch the audio.",
                resolved.title
            ),
        };
    };

    // Best effort, and never a reason to fail: a track with no picture still
    // plays. Stored through the same cache an embedded cover goes through, so
    // it gets the same thumbnails and the same accent colour.
    let art_id = client
        .fetch_thumbnail(&resolved)
        .and_then(|bytes| match art.store(&bytes) {
            Ok(art_id) => Some(art_id),
            Err(err) => {
                tracing::warn!("could not store a fetched thumbnail: {err}");
                None
            }
        });

    // After the fetch rather than before, so the file just downloaded is
    // already on disk and counted.
    sweep(audio_dir, CACHE_BUDGET, &audio.path);

    Answer::Ready {
        path: audio.path,
        facts: Box::new(facts_from(&resolved, art_id)),
    }
}

/// What to show for a fetched track.
///
/// The fetcher returns what the service said, untouched. This is where the
/// library's own naming rules are applied — the same ones a file called
/// `Artist - Title (Official Video).mp3` gets on the way into the index, since
/// a video title is exactly that string by another route.
///
/// Only what is shown is cleaned. Nothing here is written anywhere.
fn facts_from(resolved: &Resolved, art_id: Option<String>) -> StreamFacts {
    let artist = names::strip_channel_suffix(&resolved.artist);
    let artist = if artist.trim().is_empty() {
        resolved.artist.clone()
    } else {
        artist
    };

    let title = names::strip_watermarks(&resolved.title);
    let title = names::strip_decoration(&title);
    let title = names::strip_redundant_artist_prefix(&title, &artist);
    let title = if title.trim().is_empty() {
        resolved.title.clone()
    } else {
        title
    };

    StreamFacts {
        title,
        artist,
        album: resolved.album.clone(),
        art_id,
        duration: resolved.duration,
    }
}

/// Keep the fetched audio under its ceiling, oldest first.
///
/// Oldest *fetched*, not least recently played: the only timestamp there is,
/// is the one the download left. Replaying something does not renew it, which
/// makes this simpler than an LRU and very slightly worse, and the cost of
/// being wrong is one re-download.
///
/// `protect` is never removed however old it looks — it is the file that has
/// just arrived and is about to play.
fn sweep(dir: &Path, budget: u64, protect: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    let mut files: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
    let mut total = 0;

    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };

        if !metadata.is_file() {
            continue;
        }

        total += metadata.len();

        // A part file belongs to a download in progress. Its size counts
        // towards the total, but removing it would break that download.
        if path.extension().is_some_and(|ext| ext == "part") || path == protect {
            continue;
        }

        let modified = metadata.modified().unwrap_or(std::time::UNIX_EPOCH);
        files.push((path, metadata.len(), modified));
    }

    if total <= budget {
        return;
    }

    files.sort_by_key(|(_, _, modified)| *modified);

    for (path, size, _) in files {
        if total <= budget {
            break;
        }

        match std::fs::remove_file(&path) {
            Ok(()) => {
                total = total.saturating_sub(size);
                tracing::debug!("forgot fetched audio: {}", path.display());
            }
            // Windows refuses to remove a file that is open, which is exactly
            // what should happen to whatever is playing. Skip it and carry on.
            Err(err) => tracing::debug!("kept {}: {err}", path.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn resolved(title: &str, artist: &str) -> Resolved {
        Resolved {
            video_id: "dQw4w9WgXcQ".into(),
            title: title.into(),
            artist: artist.into(),
            album: None,
            duration: Some(Duration::from_secs(213)),
            thumbnail: None,
        }
    }

    // -- what gets shown ----------------------------------------------------

    #[test]
    fn a_channel_name_becomes_an_artist() {
        let facts = facts_from(&resolved("Some Song", "Nightgrove - Topic"), None);
        assert_eq!(facts.artist, "Nightgrove");

        let facts = facts_from(&resolved("Some Song", "HalcyonVEVO"), None);
        assert_eq!(facts.artist, "Halcyon");
    }

    #[test]
    fn video_decoration_comes_off_the_title() {
        let facts = facts_from(
            &resolved("Die in a Fire (Official Video)", "The Living Tombstone"),
            None,
        );

        assert_eq!(facts.title, "Die in a Fire");
    }

    /// A video title very often repeats the artist, and the player bar shows
    /// the artist on the next line anyway.
    #[test]
    fn a_title_does_not_repeat_its_own_artist() {
        let facts = facts_from(
            &resolved(
                "Rick Astley - Never Gonna Give You Up (Official Video)",
                "Rick Astley",
            ),
            None,
        );

        assert_eq!(facts.title, "Never Gonna Give You Up");
    }

    /// The same conservatism the rest of the app applies: an aside that could
    /// name a different recording is kept, because dropping it would change
    /// which song this is rather than tidy the name of it.
    #[test]
    fn an_aside_that_names_a_different_recording_survives() {
        for kept in ["(Live)", "(Acoustic)", "[Remix]", "(feat. Someone)"] {
            let title = format!("Some Song {kept}");
            let facts = facts_from(&resolved(&title, "A Band"), None);

            assert_eq!(facts.title, title, "{kept} should have been kept");
        }
    }

    /// Cleaning must never leave nothing behind. A title that is entirely
    /// decoration keeps what it had.
    #[test]
    fn a_title_that_is_all_decoration_is_left_alone() {
        let facts = facts_from(&resolved("(Official Video)", "A Band"), None);
        assert_eq!(facts.title, "(Official Video)");
    }

    #[test]
    fn an_artist_that_is_all_suffix_is_left_alone() {
        let facts = facts_from(&resolved("Some Song", "VEVO"), None);
        assert!(!facts.artist.trim().is_empty());
    }

    // -- keeping the cache under its ceiling --------------------------------

    fn write_aged(dir: &Path, name: &str, size: usize, age: Duration) -> PathBuf {
        use std::io::Write;

        let path = dir.join(name);
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(&vec![b'a'; size]).unwrap();

        // The sweep orders by modification time, so the ages have to be real.
        file.set_modified(SystemTime::now() - age).unwrap();

        path
    }

    #[test]
    fn a_cache_under_its_ceiling_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let keep = write_aged(dir.path(), "a.m4a", 100, Duration::from_secs(60));

        sweep(dir.path(), 1000, Path::new("nothing"));

        assert!(keep.exists());
    }

    #[test]
    fn the_oldest_goes_first() {
        let dir = tempfile::tempdir().unwrap();
        let old = write_aged(dir.path(), "old.m4a", 600, Duration::from_secs(9000));
        let new = write_aged(dir.path(), "new.m4a", 600, Duration::from_secs(60));

        sweep(dir.path(), 1000, Path::new("nothing"));

        assert!(!old.exists(), "the oldest should have gone");
        assert!(new.exists(), "the newest should have stayed");
    }

    /// The file that just arrived is about to play, however old the clock says
    /// it is.
    #[test]
    fn what_is_about_to_play_is_never_removed() {
        let dir = tempfile::tempdir().unwrap();
        let playing = write_aged(dir.path(), "playing.m4a", 900, Duration::from_secs(9000));
        let other = write_aged(dir.path(), "other.m4a", 900, Duration::from_secs(60));

        sweep(dir.path(), 1000, &playing);

        assert!(playing.exists(), "the protected file was removed");
        assert!(!other.exists());
    }

    /// Removing one would break a download in progress, but its bytes are real
    /// and have to count or the ceiling means nothing while one is running.
    #[test]
    fn a_download_in_progress_counts_but_is_not_removed() {
        let dir = tempfile::tempdir().unwrap();
        let partial = write_aged(dir.path(), "half.m4a.part", 900, Duration::from_secs(9000));
        let done = write_aged(dir.path(), "done.m4a", 900, Duration::from_secs(60));

        sweep(dir.path(), 1000, Path::new("nothing"));

        assert!(partial.exists(), "a part file was removed");
        assert!(!done.exists(), "the part file's bytes were not counted");
    }

    #[test]
    fn sweeping_somewhere_that_is_not_there_does_nothing() {
        sweep(Path::new("D:/no/such/place/at/all"), 0, Path::new("x"));
    }

    // -- what was found -----------------------------------------------------

    /// The point of the line is that the user can go and look at what they
    /// installed, so it has to name it.
    #[test]
    fn a_found_program_is_named_on_screen() {
        let status = ToolStatus {
            program: Some(PathBuf::from("C:/tools/yt-dlp.exe")),
            version: Some("2026.03.17".into()),
        };

        assert!(status.is_installed());
        assert!(status.summary().contains("yt-dlp.exe"));
        assert!(status.summary().contains("2026.03.17"));
    }

    /// Not being installed is an ordinary state, not an error, and the line
    /// should say what to do about it without implying the app will do it.
    #[test]
    fn a_missing_program_says_so_and_says_it_is_not_fetched() {
        let status = ToolStatus::default();

        assert!(!status.is_installed());
        assert!(status.summary().contains("not found"));
        assert!(status.summary().contains("never downloaded"));
    }

    #[test]
    fn a_program_that_will_not_say_its_version_is_still_reported() {
        let status = ToolStatus {
            program: Some(PathBuf::from("C:/tools/yt-dlp.exe")),
            version: None,
        };

        assert!(status.is_installed());
        assert!(status.summary().contains("too old"));
    }

    // -- the stage label ----------------------------------------------------

    /// A job that has been asked for but not yet picked up is working, and
    /// has to say so however idle its stage still looks.
    #[test]
    fn work_that_has_not_started_yet_still_has_a_label() {
        assert_eq!(working_label(Stage::Idle), "Working");
        assert_eq!(working_label(Stage::Resolving), "Looking up the link");
        assert_eq!(working_label(Stage::Fetching), "Fetching the audio");
    }

    #[test]
    fn every_stage_but_idle_says_what_is_happening() {
        assert_eq!(Stage::Idle.label(), None);
        assert!(Stage::Resolving.label().is_some());
        assert!(Stage::Fetching.label().is_some());
    }

    #[test]
    fn a_stage_survives_the_trip_through_an_atomic() {
        for stage in [Stage::Idle, Stage::Resolving, Stage::Fetching] {
            assert_eq!(Stage::from_code(stage.code()), stage);
        }
    }
}
