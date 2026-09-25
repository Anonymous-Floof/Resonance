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
//! A playlist does not change that. Reading the list is one job, and each
//! track is a job of its own, asked for by the caller one track ahead of what
//! is playing — see `link_queue`. The worker never knows it is working
//! through a list, which is what keeps it this simple.
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
use mp_net::sponsorblock;
use mp_net::tool::YtDlp;
use mp_net::youtube::{Client, Listing, Query, Resolved, Trouble};

use crate::player::StreamFacts;

/// How much fetched audio to keep.
///
/// A ceiling rather than a setting: the right number is "enough that replaying
/// something recent is instant, and not so much that a music player quietly
/// eats a disk", and no user wants to be asked. Two gigabytes is several
/// hundred tracks at the bitrate this fetches.
pub const CACHE_BUDGET: u64 = 2 * 1024 * 1024 * 1024;

/// How old a reported version may be before it is worth mentioning.
///
/// yt-dlp releases are dated, which makes this checkable without asking
/// anybody anything. Three months is well past its release cadence and about
/// where the trouble starts.
pub const STALE_AFTER_DAYS: i64 = 90;

/// When a dated version was released.
///
/// `2026.03.17`, and nightlies carry a fourth part which is ignored. Anything
/// that is not a date is `None` rather than a guess.
fn released_on(version: &str) -> Option<i64> {
    let mut parts = version.trim().split('.');
    let year = parts.next()?;
    let month = parts.next()?;
    let day = parts.next()?;

    mp_net::timestamp::parse(&format!("{year}-{month}-{day}T00:00:00Z"))
}

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

    /// How many days old the reported version is, where it reads as a date.
    pub fn age_days(&self) -> Option<i64> {
        let released = released_on(self.version.as_deref()?)?;

        Some((mp_net::timestamp::now_unix() - released) / 86_400)
    }

    pub fn is_stale(&self) -> bool {
        self.age_days().is_some_and(|days| days >= STALE_AFTER_DAYS)
    }

    /// A warning, when the copy that was found is old enough to be the reason
    /// links are not playing.
    ///
    /// Worth saying out loud rather than leaving to the version string,
    /// because the failure it causes does not look like an out-of-date
    /// program: the link resolves, the title comes back, and only the audio is
    /// refused. Somebody reading that reasonably concludes the app is broken.
    pub fn staleness(&self) -> Option<String> {
        let days = self.age_days()?;

        (days >= STALE_AFTER_DAYS).then(|| {
            format!(
                "That copy is about {} months old. YouTube changes what it demands of a downloader every few weeks, and an old yt-dlp will find a link and then be refused the audio for it. If links are not playing, update it before looking anywhere else.",
                (days / 30).max(1)
            )
        })
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
    Listing,
}

impl Stage {
    fn from_code(code: u8) -> Self {
        match code {
            1 => Self::Resolving,
            2 => Self::Fetching,
            3 => Self::Listing,
            _ => Self::Idle,
        }
    }

    fn code(self) -> u8 {
        match self {
            Self::Idle => 0,
            Self::Resolving => 1,
            Self::Fetching => 2,
            Self::Listing => 3,
        }
    }

    /// What to say while this is happening.
    pub fn label(self) -> Option<&'static str> {
        match self {
            Self::Idle => None,
            Self::Resolving => Some("Looking up the link"),
            Self::Fetching => Some("Fetching the audio"),
            Self::Listing => Some("Reading the playlist"),
        }
    }
}

/// What to say while the next track of a playlist is being made ready.
///
/// Its own label because it is not what the user asked for just now: a box
/// opened while it runs should say the wait is for the playlist, not suggest
/// the new link has already been taken.
pub const GETTING_NEXT_READY: &str = "Getting the next track in the playlist ready";

/// Why a link could not be sent at all, when something is already running.
pub const BUSY: &str =
    "Something is already being fetched. The box will take a link again as soon as it finishes.";

/// What came back.
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    /// A file on disk, ready to play, and what is known about it.
    Ready {
        path: PathBuf,
        facts: Box<StreamFacts>,
    },
    /// What is in a playlist. Nothing in it has been fetched.
    Listed {
        listing: Box<Listing>,
        /// The video the link named alongside the list, to start from.
        start_at: Option<String>,
    },
    /// Nothing playable, and why not in words the user can act on.
    Nothing {
        why: String,
        /// Whether the reason is the program rather than the video, so that
        /// a playlist should stop instead of trying the next track.
        blocks_the_rest: bool,
    },
}

/// An answer, and whom it is for.
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub answer: Answer,
    /// Whether this was the next track of a playlist rather than something
    /// the user pasted. Such an answer can outlive its list - the user played
    /// something else while it was being fetched - and must then be dropped,
    /// never mistaken for a link that has just been asked for.
    pub for_a_list: bool,
}

/// What the worker is asked to do.
///
/// Preferences travel with the job rather than being read on the worker, so a
/// link already in flight finishes under the setting it was sent with.
#[derive(Debug, Clone)]
enum Job {
    /// One video: resolve it, fetch it, and find its picture and segments.
    Video {
        query: Query,
        skip_segments: bool,
        /// An album name the list knew and the video may not.
        album: Option<String>,
    },
    /// What is in a playlist, and nothing more.
    List { query: Query },
}

/// A worker thread that turns links into files.
pub struct YoutubeJob {
    jobs: Sender<Job>,
    answers: Receiver<Answer>,
    stage: Arc<AtomicU8>,
    /// Set while something is in flight, so a second paste does not queue up
    /// behind the first without saying so.
    busy: bool,
    /// Whether what is in flight is the next track of a playlist rather than
    /// something the user has just asked for.
    background: bool,
}

impl std::fmt::Debug for YoutubeJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("YoutubeJob")
            .field("stage", &self.stage())
            .field("busy", &self.busy)
            .field("background", &self.background)
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
        let (jobs, incoming) = std::sync::mpsc::channel::<Job>();
        let (outgoing, answers) = std::sync::mpsc::channel::<Answer>();

        let stage = Arc::new(AtomicU8::new(Stage::Idle.code()));
        let worker_stage = Arc::clone(&stage);
        let sweep_dir = audio_dir.clone();

        let spawned = std::thread::Builder::new()
            .name("resonance-youtube".into())
            .spawn(move || {
                // Built on this thread: it owns the transport and the runner,
                // and nothing on the UI side should be able to reach either.
                let client = Client::new(
                    Box::new(tool),
                    cache_root.clone(),
                    audio_dir,
                    Arc::clone(&activity),
                );
                let segments = sponsorblock::Client::new(cache_root, activity);
                let art = ArtCache::new(art_root);

                // Ends when the sender is dropped, which is when the app quits
                // or the setting is switched off.
                while let Ok(job) = incoming.recv() {
                    let answer = run(
                        &client,
                        &segments,
                        &art,
                        &sweep_dir,
                        &job,
                        &worker_stage,
                        &ctx,
                    );

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
            background: false,
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

        if self.background {
            return Some(GETTING_NEXT_READY);
        }

        Some(working_label(self.stage()))
    }

    /// Ask for a link the user has just given.
    ///
    /// A link naming both a video and a playlist is the playlist when
    /// `whole_list` is set and the video otherwise; one naming only one of them
    /// is that. The error is a sentence for the user. A link that names
    /// nothing is refused here rather than sent, so nothing unrecognised is
    /// ever handed to the program.
    pub fn want(
        &mut self,
        link: &str,
        skip_segments: bool,
        whole_list: bool,
    ) -> Result<(), String> {
        if self.busy {
            return Err(BUSY.to_owned());
        }

        let job = job_for(Query::new(link), skip_segments, whole_list)?;
        self.send(job, false)
    }

    /// Fetch one video of a playlist that is already playing.
    ///
    /// `false` when something is in flight; the caller asks again later.
    pub fn fetch_listed(
        &mut self,
        video_id: &str,
        skip_segments: bool,
        album: Option<String>,
    ) -> bool {
        if self.busy {
            return false;
        }

        let job = Job::Video {
            query: Query::new(video_id),
            skip_segments,
            album,
        };

        self.send(job, true).is_ok()
    }

    fn send(&mut self, job: Job, background: bool) -> Result<(), String> {
        if self.jobs.send(job).is_err() {
            return Err("The link worker has stopped. Switching Play from a link off and on again starts it afresh.".to_owned());
        }

        self.busy = true;
        self.background = background;
        Ok(())
    }

    /// Take whatever has landed.
    pub fn poll(&mut self) -> Option<Reply> {
        match self.answers.try_recv() {
            Ok(answer) => {
                let for_a_list = self.background;
                self.busy = false;
                self.background = false;
                Some(Reply { answer, for_a_list })
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                // The worker is gone, so nothing is coming. Clearing this is
                // what stops the interface waiting forever on a dead thread.
                self.busy = false;
                self.background = false;
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

/// Which job a pasted link is.
///
/// Apart from [`YoutubeJob::want`] so the choice can be tested without a
/// thread. The one decision in it that is not obvious: a link to a video
/// inside one of the user's own lists — Watch Later, say — is the video,
/// because the list cannot be read and the video can.
fn job_for(query: Query, skip_segments: bool, whole_list: bool) -> Result<Job, String> {
    let has_video = query.video_id().is_some();
    let has_list = query.playlist_link().is_some();

    if has_list && (whole_list || !has_video) {
        return Ok(Job::List { query });
    }

    if has_video {
        return Ok(Job::Video {
            query,
            skip_segments,
            album: None,
        });
    }

    if query.names_account_list() {
        return Err(Trouble::NeedsAccount.message());
    }

    Err(Trouble::NotALink.message())
}

/// Whether a failure is about the program rather than the video, so that
/// working through the rest of a playlist would only fail the same way.
fn blocks_the_rest(trouble: &Trouble) -> bool {
    matches!(trouble, Trouble::Refused(_) | Trouble::Tooling(_))
}

/// One job, start to finish.
#[allow(clippy::too_many_arguments)]
fn run(
    client: &Client,
    segments: &sponsorblock::Client,
    art: &ArtCache,
    audio_dir: &Path,
    job: &Job,
    stage: &AtomicU8,
    ctx: &egui::Context,
) -> Answer {
    match job {
        Job::List { query } => list(client, query, stage, ctx),
        Job::Video {
            query,
            skip_segments,
            album,
        } => video(
            client,
            segments,
            art,
            audio_dir,
            query,
            *skip_segments,
            album.as_deref(),
            stage,
            ctx,
        ),
    }
}

/// What is in a playlist.
fn list(client: &Client, query: &Query, stage: &AtomicU8, ctx: &egui::Context) -> Answer {
    stage.store(Stage::Listing.code(), Ordering::Relaxed);
    ctx.request_repaint();

    match client.list(query) {
        Ok(listing) => Answer::Listed {
            listing: Box::new(listing),
            start_at: query.video_id(),
        },
        Err(trouble) => Answer::Nothing {
            why: trouble.message(),
            blocks_the_rest: blocks_the_rest(&trouble),
        },
    }
}

/// One video, start to finish.
#[allow(clippy::too_many_arguments)]
fn video(
    client: &Client,
    segments: &sponsorblock::Client,
    art: &ArtCache,
    audio_dir: &Path,
    query: &Query,
    skip_segments: bool,
    album: Option<&str>,
    stage: &AtomicU8,
    ctx: &egui::Context,
) -> Answer {
    stage.store(Stage::Resolving.code(), Ordering::Relaxed);
    ctx.request_repaint();

    let resolved = match client.resolve(query) {
        Ok(resolved) => resolved,
        // The reason travels with the failure now. Saying only that
        // nothing happened sent people hunting through the log for a line
        // that already knew the answer.
        Err(trouble) => {
            return Answer::Nothing {
                why: trouble.message(),
                blocks_the_rest: blocks_the_rest(&trouble),
            };
        }
    };

    stage.store(Stage::Fetching.code(), Ordering::Relaxed);
    ctx.request_repaint();

    let audio = match client.fetch_audio(&resolved) {
        Ok(audio) => audio,
        Err(trouble) => {
            // Two sentences rather than one joined clause: the reasons
            // name YouTube and yt-dlp, and lowercasing either to fit after
            // a comma reads worse than a full stop does.
            return Answer::Nothing {
                why: format!("Found {:?}. {}", resolved.title, trouble.message()),
                blocks_the_rest: blocks_the_rest(&trouble),
            };
        }
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

    // Asked for last, and never a reason to fail: a track with nothing
    // skipped still plays. Only asked at all when the user said so.
    let skips = if skip_segments {
        segments
            .fetch(&sponsorblock::Query::new(&resolved.video_id))
            .unwrap_or_default()
            .into_iter()
            .map(|segment| (segment.start, segment.end))
            .collect()
    } else {
        Vec::new()
    };

    // After the fetch rather than before, so the file just downloaded is
    // already on disk and counted.
    sweep(audio_dir, CACHE_BUDGET, &audio.path);

    let mut facts = facts_from(&resolved, art_id, skips);

    // The service's own answer wins where it has one; a list that is an album
    // fills the gap an ordinary upload leaves.
    if facts.album.is_none() {
        facts.album = album.map(str::to_owned);
    }

    Answer::Ready {
        path: audio.path,
        facts: Box::new(facts),
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
fn facts_from(resolved: &Resolved, art_id: Option<String>, skips: Vec<(f64, f64)>) -> StreamFacts {
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
        skips,
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

    /// The cleaning is what these tests are about, so they keep the shape
    /// they had before segments were carried alongside it.
    fn facts_from_test(resolved: &Resolved, art_id: Option<String>) -> StreamFacts {
        facts_from(resolved, art_id, Vec::new())
    }

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
        let facts = facts_from_test(&resolved("Some Song", "Nightgrove - Topic"), None);
        assert_eq!(facts.artist, "Nightgrove");

        let facts = facts_from_test(&resolved("Some Song", "HalcyonVEVO"), None);
        assert_eq!(facts.artist, "Halcyon");
    }

    #[test]
    fn video_decoration_comes_off_the_title() {
        let facts = facts_from_test(
            &resolved("Die in a Fire (Official Video)", "The Living Tombstone"),
            None,
        );

        assert_eq!(facts.title, "Die in a Fire");
    }

    /// A video title very often repeats the artist, and the player bar shows
    /// the artist on the next line anyway.
    #[test]
    fn a_title_does_not_repeat_its_own_artist() {
        let facts = facts_from_test(
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
            let facts = facts_from_test(&resolved(&title, "A Band"), None);

            assert_eq!(facts.title, title, "{kept} should have been kept");
        }
    }

    /// Cleaning must never leave nothing behind. A title that is entirely
    /// decoration keeps what it had.
    #[test]
    fn a_title_that_is_all_decoration_is_left_alone() {
        let facts = facts_from_test(&resolved("(Official Video)", "A Band"), None);
        assert_eq!(facts.title, "(Official Video)");
    }

    #[test]
    fn an_artist_that_is_all_suffix_is_left_alone() {
        let facts = facts_from_test(&resolved("Some Song", "VEVO"), None);
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

    /// The version is a date, which is what makes any of this checkable.
    #[test]
    fn a_dated_version_reads_as_a_date() {
        let released = released_on("2026.03.17").expect("a date");
        assert_eq!(mp_net::timestamp::format(released), "2026-03-17T00:00:00Z");

        // Nightlies carry a fourth part, which says nothing extra about the day.
        assert_eq!(released_on("2026.03.17.232301"), Some(released));
    }

    #[test]
    fn something_that_is_not_a_date_is_not_guessed_at() {
        for version in ["", "unknown", "2026", "2026.03", "nightly", "x.y.z"] {
            assert_eq!(released_on(version), None, "{version}");
        }
    }

    /// The exact case that sent somebody to report a broken feature: a copy
    /// five months behind, resolving links and then being refused the audio.
    #[test]
    fn a_copy_from_months_ago_is_called_out() {
        let status = ToolStatus {
            program: Some(PathBuf::from("C:/tools/yt-dlp.exe")),
            version: Some("2020.01.01".into()),
        };

        assert!(status.is_stale());

        let warning = status.staleness().expect("a warning");
        assert!(warning.contains("update it"), "{warning}");
        assert!(warning.contains("refused"), "{warning}");
    }

    #[test]
    fn a_current_copy_is_left_alone() {
        // Built from the clock, so this stays true tomorrow.
        let today = mp_net::timestamp::format(mp_net::timestamp::now_unix());
        let version = today[..10].replace('-', ".");

        let status = ToolStatus {
            program: Some(PathBuf::from("C:/tools/yt-dlp.exe")),
            version: Some(version.clone()),
        };

        assert_eq!(status.age_days(), Some(0), "{version}");
        assert!(!status.is_stale());
        assert!(status.staleness().is_none());
    }

    /// No version means no opinion, rather than an alarming guess.
    #[test]
    fn a_program_with_no_version_is_not_called_old() {
        let status = ToolStatus {
            program: Some(PathBuf::from("C:/tools/yt-dlp.exe")),
            version: None,
        };

        assert_eq!(status.age_days(), None);
        assert!(!status.is_stale());
        assert!(status.staleness().is_none());
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
        assert!(Stage::Listing.label().is_some());
    }

    #[test]
    fn a_stage_survives_the_trip_through_an_atomic() {
        for stage in [
            Stage::Idle,
            Stage::Resolving,
            Stage::Fetching,
            Stage::Listing,
        ] {
            assert_eq!(Stage::from_code(stage.code()), stage);
        }
    }

    // -- which job a link is ----------------------------------------------------

    const BOTH: &str =
        "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI";

    fn is_list(job: &Result<Job, String>) -> bool {
        matches!(job, Ok(Job::List { .. }))
    }

    fn is_video(job: &Result<Job, String>) -> bool {
        matches!(job, Ok(Job::Video { .. }))
    }

    #[test]
    fn a_link_naming_both_is_whichever_was_asked_for() {
        assert!(is_list(&job_for(Query::new(BOTH), false, true)));
        assert!(is_video(&job_for(Query::new(BOTH), false, false)));
    }

    #[test]
    fn a_link_naming_one_thing_is_that_thing() {
        let list = "https://www.youtube.com/playlist?list=PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI";
        assert!(is_list(&job_for(Query::new(list), false, false)));

        let video = "https://youtu.be/dQw4w9WgXcQ";
        assert!(is_video(&job_for(Query::new(video), false, true)));
    }

    /// The list cannot be read without an account and the video can, so the
    /// video is what plays.
    #[test]
    fn a_video_inside_the_users_own_list_is_the_video() {
        let link = "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=WL";
        assert!(is_video(&job_for(Query::new(link), false, true)));
    }

    #[test]
    fn the_users_own_list_on_its_own_says_it_needs_an_account() {
        let job = job_for(
            Query::new("https://www.youtube.com/playlist?list=LL"),
            false,
            true,
        );

        assert_eq!(job.unwrap_err(), Trouble::NeedsAccount.message());
    }

    #[test]
    fn something_that_is_not_a_link_says_so() {
        let job = job_for(Query::new("hello"), false, true);

        assert_eq!(job.unwrap_err(), Trouble::NotALink.message());
    }

    #[test]
    fn the_segment_preference_travels_with_the_video() {
        match job_for(Query::new("https://youtu.be/dQw4w9WgXcQ"), true, false) {
            Ok(Job::Video { skip_segments, .. }) => assert!(skip_segments),
            other => panic!("expected a video, got {other:?}"),
        }
    }

    /// An old program fails every track the same way; a gone video is only
    /// itself.
    #[test]
    fn only_a_failure_of_the_program_stops_a_list() {
        assert!(blocks_the_rest(&Trouble::Refused("403".into())));
        assert!(blocks_the_rest(&Trouble::Tooling("timed out".into())));
        assert!(!blocks_the_rest(&Trouble::Unavailable("private".into())));
        assert!(!blocks_the_rest(&Trouble::Failed("odd".into())));
    }
}
