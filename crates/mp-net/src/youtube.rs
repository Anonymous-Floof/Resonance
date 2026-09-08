//! Turning a link into a file this build can play.
//!
//! Three steps, deliberately separate so the caller can say which one it is
//! waiting on: [`Client::resolve`] asks what a link *is*,
//! [`Client::fetch_audio`] gets the sound, and [`Client::fetch_thumbnail`]
//! gets the picture. Only the third is an ordinary request; the first two are
//! run by `yt-dlp`, for the reasons set out in [`crate::tool`].
//!
//! # Two decisions that shape everything here
//!
//! **The audio is fetched to a file and then played.** Not streamed. Symphonia
//! would happily decode from a network reader, but every layer above it —
//! the queue, the seek bar, the crossfade, the gapless seam — is written in
//! terms of a path to a seekable file, and a stream would have to teach all of
//! them about latency, refused seeks and unknown length. Fetching first means
//! none of that changes: by the time the engine sees a track it is a file like
//! any other. The cost is a wait before the first note, and it is the right
//! trade on a slow connection anyway.
//!
//! **The stream asked for is AAC.** YouTube's best audio is Opus and this
//! build has no Opus decoder — that needs libopus, a C dependency the project
//! has refused, and the same refusal is why the library lists Opus files as
//! unplayable. So the better-sounding stream is the one that cannot be played.
//! See [`FORMAT`].
//!
//! # What is cached, and what cannot be
//!
//! What a link resolves to is cached like any other answer, misses included.
//! The *direct media URL* is not: those are signed and expire within hours, so
//! a remembered one would be a confident failure a few hours later. The audio
//! file on disk is the real cache, and finding one is what makes replaying a
//! link cost nothing at all.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::activity::{Activity, Entry as LogEntry, Outcome};
use crate::cache::{Cache, Entry as CacheEntry};
use crate::error::NetError;
use crate::http::Transport;
use crate::rate::Limiter;
use crate::source::{Source, YOUTUBE, YOUTUBE_THUMBNAIL};
use crate::tool::{Output, Runner};

/// Where cached answers about links are kept, under the cache root.
pub const CACHE_NAMESPACE: &str = "youtube";

/// Which stream to ask for.
///
/// `mp4a` is AAC. The alternative — and YouTube's own preference, and the
/// better-sounding one — is Opus, which this build cannot decode: Symphonia
/// ships no Opus decoder and adding one means depending on libopus, a C
/// library the project has deliberately stayed clear of. An Opus file in the
/// user's own library is listed as unplayable *with a reason*; asking for an
/// Opus stream here would produce the same failure with a much more confusing
/// explanation.
///
/// The second half is a fallback on container rather than codec, for the rare
/// video whose AAC stream is not tagged the way the first half expects. There
/// is deliberately **no** final fallback to plain `bestaudio`: that would
/// select Opus, download several megabytes, and fail at the decoder — which
/// looks like a broken player rather than a video this build cannot use.
pub const FORMAT: &str = "bestaudio[acodec^=mp4a]/bestaudio[ext=m4a]";

/// The largest audio file that will be fetched.
///
/// A refusal rather than a tuning knob, in the same spirit as the transport's
/// body limits. Generous for a song, and a guard against a link that turns out
/// to be a ten-hour upload.
pub const MAX_AUDIO_BYTES: u64 = 256 * 1024 * 1024;

/// How long resolving a link may take.
pub const RESOLVE_TIMEOUT: Duration = Duration::from_secs(90);

/// How long fetching the audio may take.
///
/// Much longer than anything else in this crate, because it is the only thing
/// here that moves megabytes, and on a slow connection that is minutes rather
/// than seconds. Nothing waits on it but the job that asked.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(900);

// ---------------------------------------------------------------------------
// The question
// ---------------------------------------------------------------------------

/// A link somebody pasted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    link: String,
}

impl Query {
    pub fn new(link: impl Into<String>) -> Self {
        Self {
            link: link.into().trim().to_owned(),
        }
    }

    pub fn link(&self) -> &str {
        &self.link
    }

    /// The video this names, if it names one.
    ///
    /// Everything downstream is built on this rather than on the link itself,
    /// and that is a safety property as much as a convenience: an id is
    /// eleven characters of `A-Z a-z 0-9 _ -` and nothing else, so it can be
    /// used as a filename without any further thought, and a link this does
    /// not recognise is never handed to `yt-dlp` at all.
    pub fn video_id(&self) -> Option<String> {
        let link = self.link.as_str();

        if is_video_id(link) {
            return Some(link.to_owned());
        }

        let rest = link
            .strip_prefix("https://")
            .or_else(|| link.strip_prefix("http://"))
            .unwrap_or(link);
        let rest = rest.strip_prefix("www.").unwrap_or(rest);

        if let Some(tail) = rest.strip_prefix("youtu.be/") {
            return leading_id(tail);
        }

        // `music.` is YouTube Music, which is the same video by another name.
        let rest = rest.strip_prefix("music.").unwrap_or(rest);
        let rest = rest.strip_prefix("m.").unwrap_or(rest);

        let tail = rest.strip_prefix("youtube.com/")?;

        for prefix in ["shorts/", "embed/", "live/", "v/"] {
            if let Some(rest) = tail.strip_prefix(prefix) {
                return leading_id(rest);
            }
        }

        let (_, query) = tail.split_once('?')?;
        query
            .split('&')
            .find_map(|pair| pair.strip_prefix("v="))
            .and_then(leading_id)
    }

    /// Whether this is worth asking about.
    pub fn is_answerable(&self) -> bool {
        self.video_id().is_some()
    }

    /// How this reads in the activity log, before anything is known about it.
    pub fn subject(&self) -> String {
        match self.video_id() {
            Some(id) => format!("the video {id}"),
            None => format!("the link {}", self.link),
        }
    }

    pub fn cache_key(&self) -> String {
        crate::cache::key(&["youtube", self.video_id().unwrap_or_default().as_str()])
    }
}

fn is_video_id(text: &str) -> bool {
    text.len() == 11
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The id at the front of `text`, up to whatever ends it.
fn leading_id(text: &str) -> Option<String> {
    let end = text.find(['?', '&', '/', '#']).unwrap_or(text.len());
    let candidate = &text[..end];

    is_video_id(candidate).then(|| candidate.to_owned())
}

// ---------------------------------------------------------------------------
// The answer
// ---------------------------------------------------------------------------

/// What `yt-dlp` said about a link.
///
/// Only what came back, and none of it tidied: a channel name is still a
/// channel name and a title still carries whatever decoration the uploader put
/// on it. Cleaning that up needs the library's own naming rules, which live in
/// `mp-core`, and this crate deliberately does not depend on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resolved {
    pub video_id: String,
    pub title: String,
    pub artist: String,
    /// Set only where the service actually knows a release, which in practice
    /// means YouTube Music rather than an ordinary upload.
    pub album: Option<String>,
    pub duration: Option<Duration>,
    pub thumbnail: Option<String>,
}

impl Resolved {
    /// How this reads in the activity log.
    pub fn subject(&self) -> String {
        format!("audio for \"{}\" by {}", self.title, self.artist)
    }
}

/// Why a link went nowhere, in words that point at the fix.
///
/// The distinction that matters is between a video that is genuinely gone and
/// a service that refused. They look identical from here — both are a failed
/// run with a message — and they need opposite responses: one is remembered so
/// it is never asked about again, and the other must not be, because the fix
/// is an update and a remembered miss would outlive it by a fortnight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trouble {
    /// The text named nothing this recognises. Nothing was run.
    NotALink,
    /// The video is private, removed, age-gated, or offers no audio this
    /// build can decode.
    Unavailable(String),
    /// `yt-dlp` could not be run, or did not finish in time.
    Tooling(String),
    /// YouTube answered and refused.
    ///
    /// Nearly always means the extractor is behind rather than anything being
    /// wrong with the video: the service changes what it demands of a client,
    /// and a copy of `yt-dlp` from a few months ago cannot produce it. Kept
    /// apart from the rest because it is the one failure here with an obvious
    /// fix, and telling somebody to update is only useful if it is said.
    Refused(String),
    /// Something else went wrong.
    Failed(String),
}

impl Trouble {
    /// One sentence for the user.
    pub fn message(&self) -> String {
        match self {
            Self::NotALink => {
                "That is not a link to a video this recognises. A YouTube or YouTube Music watch link, or a youtu.be one.".to_owned()
            }
            Self::Unavailable(_) => {
                "That video is not available - it may be private, removed, age-restricted, or offer no audio this build can play.".to_owned()
            }
            Self::Refused(_) => {
                "YouTube refused the download. This almost always means yt-dlp is out of date: the service changes what it asks of a program that downloads from it, and an old copy cannot answer. Updating yt-dlp is the fix.".to_owned()
            }
            Self::Tooling(_) => {
                "yt-dlp could not be run. Settings, Online says which copy was found.".to_owned()
            }
            Self::Failed(_) => "The download did not finish.".to_owned(),
        }
    }

    /// What actually went wrong, for the log's detail column.
    pub fn detail(&self) -> String {
        match self {
            Self::NotALink => "not a link to a video".to_owned(),
            Self::Unavailable(said)
            | Self::Tooling(said)
            | Self::Refused(said)
            | Self::Failed(said) => said.clone(),
        }
    }

    /// How this reads to the rate limiter and the log.
    ///
    /// A video that is gone is a miss and must not back the limiter off; a
    /// refusal is a real failure and should.
    fn as_error(&self) -> NetError {
        match self {
            Self::NotALink | Self::Unavailable(_) => NetError::NotFound,
            Self::Tooling(said) | Self::Refused(said) | Self::Failed(said) => {
                NetError::Transport(said.clone())
            }
        }
    }
}

/// A fetched audio file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Audio {
    pub path: PathBuf,
    pub bytes: u64,
    /// Whether this was already on disk, so the caller can say so.
    pub from_cache: bool,
}

/// What `--dump-single-json` gives back.
///
/// A small corner of a very large object. Every field is optional because a
/// missing one should mean a thinner answer rather than a failed parse.
#[derive(Debug, Deserialize)]
struct Dumped {
    id: Option<String>,
    title: Option<String>,
    /// Present for YouTube Music and topic channels, where the service knows
    /// the actual recording rather than just the upload. Much better than the
    /// channel name when it is there.
    track: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    uploader: Option<String>,
    channel: Option<String>,
    duration: Option<f64>,
    thumbnail: Option<String>,
    /// The direct media URL of the selected format. Never stored — these are
    /// signed and expire — but its host is what the log records.
    url: Option<String>,
    #[serde(default)]
    requested_downloads: Vec<RequestedDownload>,
}

#[derive(Debug, Deserialize)]
struct RequestedDownload {
    url: Option<String>,
}

impl Dumped {
    fn media_url(&self) -> Option<&str> {
        self.url
            .as_deref()
            .or_else(|| self.requested_downloads.first()?.url.as_deref())
    }

    fn into_resolved(self, fallback_id: &str) -> Resolved {
        let video_id = self.id.unwrap_or_else(|| fallback_id.to_owned());

        // `track` and `artist` are what the service knows about the recording;
        // `title` and `uploader` are what it knows about the upload. The first
        // pair is better whenever it exists, and on YouTube Music it usually
        // does.
        let title = self
            .track
            .filter(|text| !text.trim().is_empty())
            .or(self.title)
            .unwrap_or_else(|| video_id.clone());

        let artist = self
            .artist
            .filter(|text| !text.trim().is_empty())
            .or(self.uploader)
            .or(self.channel)
            .unwrap_or_else(|| "Unknown Artist".to_owned());

        Resolved {
            video_id,
            title,
            artist,
            album: self.album.filter(|text| !text.trim().is_empty()),
            // A live stream reports no duration, and so does an upload the
            // extractor only half understood.
            duration: self
                .duration
                .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
                .map(Duration::from_secs_f64),
            thumbnail: self.thumbnail.filter(|text| !text.trim().is_empty()),
        }
    }
}

// ---------------------------------------------------------------------------
// The client
// ---------------------------------------------------------------------------

/// Everything needed to turn a link into something playable.
///
/// Owns the three things that must not be bypassed — the rate limiter, the
/// cache and the activity log — for the same reason the other fetchers do.
/// The difference here is a fourth: the [`Runner`], which is the only way this
/// crate starts a process.
pub struct Client {
    runner: Box<dyn Runner>,
    transport: Box<dyn Transport>,
    youtube_limiter: Limiter,
    thumbnail_limiter: Limiter,
    cache: Cache,
    audio_dir: PathBuf,
    activity: Arc<Activity>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("audio_dir", &self.audio_dir)
            .finish_non_exhaustive()
    }
}

impl Client {
    /// A client over the real transport and a located `yt-dlp`.
    pub fn new(
        runner: Box<dyn Runner>,
        cache_root: impl Into<PathBuf>,
        audio_dir: impl Into<PathBuf>,
        activity: Arc<Activity>,
    ) -> Self {
        Self::with_parts(
            runner,
            Box::new(crate::http::Http::new()),
            cache_root,
            audio_dir,
            activity,
        )
    }

    /// A client over any runner and transport. The seam the tests use.
    pub fn with_parts(
        runner: Box<dyn Runner>,
        transport: Box<dyn Transport>,
        cache_root: impl Into<PathBuf>,
        audio_dir: impl Into<PathBuf>,
        activity: Arc<Activity>,
    ) -> Self {
        Self {
            runner,
            transport,
            youtube_limiter: Limiter::for_source(&YOUTUBE),
            thumbnail_limiter: Limiter::for_source(&YOUTUBE_THUMBNAIL),
            cache: Cache::new(cache_root.into().join(CACHE_NAMESPACE)),
            audio_dir: audio_dir.into(),
            activity,
        }
    }

    /// The services this reaches, for the settings screen.
    pub fn sources(&self) -> [&'static Source; 2] {
        [&YOUTUBE, &YOUTUBE_THUMBNAIL]
    }

    pub fn cache(&self) -> &Cache {
        &self.cache
    }

    pub fn audio_dir(&self) -> &Path {
        &self.audio_dir
    }

    /// Find out what a link is.
    ///
    /// **Blocks.** Background threads only.
    pub fn resolve(&self, query: &Query) -> Result<Resolved, Trouble> {
        let Some(video_id) = query.video_id() else {
            return Err(Trouble::NotALink);
        };

        let key = query.cache_key();

        if let Some(entry) = self.cache.read::<Resolved>(&key) {
            self.log(&YOUTUBE, Outcome::Cached, query.subject(), 0, None, None);
            return entry.found.ok_or_else(|| {
                Trouble::Unavailable("remembered from an earlier lookup".to_owned())
            });
        }

        let args = vec![
            "--dump-single-json",
            "--no-playlist",
            "--no-warnings",
            "--no-progress",
            "-f",
            FORMAT,
            query.link(),
        ];

        self.youtube_limiter.acquire();

        let output = match self.runner.run(&args, RESOLVE_TIMEOUT) {
            Ok(output) => output,
            Err(err) => {
                self.note(&err, &self.youtube_limiter);
                self.log(
                    &YOUTUBE,
                    err.outcome(),
                    query.subject(),
                    0,
                    Some(err.to_string()),
                    None,
                );
                return Err(Trouble::Tooling(err.to_string()));
            }
        };

        if !output.succeeded() {
            let trouble = interpret(&output);
            let err = trouble.as_error();
            self.note(&err, &self.youtube_limiter);

            // Only a video that is genuinely gone is remembered. A refusal is
            // the extractor being behind rather than anything about the video,
            // and remembering it would outlive the update that fixes it by a
            // fortnight.
            if matches!(trouble, Trouble::Unavailable(_)) {
                self.store(&key, CacheEntry::<Resolved>::missing());
            }

            self.log(
                &YOUTUBE,
                err.outcome(),
                query.subject(),
                0,
                Some(trouble.detail()),
                None,
            );
            return Err(trouble);
        }

        self.youtube_limiter.note_success();

        let dumped: Dumped = match serde_json::from_slice(&output.stdout) {
            Ok(dumped) => dumped,
            Err(err) => {
                let err = NetError::Decode(err.to_string());
                self.youtube_limiter.note_failure();
                self.log(
                    &YOUTUBE,
                    err.outcome(),
                    query.subject(),
                    output.stdout.len() as u64,
                    Some(err.to_string()),
                    None,
                );
                return Err(Trouble::Failed(err.to_string()));
            }
        };

        // Read before the value is consumed: this is the machine that will
        // actually serve the audio, and naming it is the whole point of
        // recording a host separately from the one that was addressed.
        let served_by = dumped.media_url().and_then(host_of);
        let resolved = dumped.into_resolved(&video_id);

        self.store(&key, CacheEntry::found(resolved.clone()));
        self.log(
            &YOUTUBE,
            Outcome::Ok,
            resolved.subject(),
            output.stdout.len() as u64,
            None,
            served_by,
        );

        Ok(resolved)
    }

    /// Get the audio itself, into the cache directory.
    ///
    /// **Blocks, for as long as the download takes.** Background threads only.
    pub fn fetch_audio(&self, resolved: &Resolved) -> Result<Audio, Trouble> {
        // The id has been validated to eleven characters of `A-Z a-z 0-9 _ -`,
        // which is what makes it safe to build a path from.
        if !is_video_id(&resolved.video_id) {
            return Err(Trouble::NotALink);
        }

        if let Some(existing) = self.existing_audio(&resolved.video_id) {
            self.log(
                &YOUTUBE,
                Outcome::Cached,
                resolved.subject(),
                existing.bytes,
                None,
                None,
            );
            return Ok(existing);
        }

        if let Err(err) = std::fs::create_dir_all(&self.audio_dir) {
            tracing::warn!("could not make room for fetched audio: {err}");
            return Err(Trouble::Failed(format!(
                "could not make room for fetched audio: {err}"
            )));
        }

        let template = self
            .audio_dir
            .join(format!("{}.%(ext)s", resolved.video_id));
        let template = template.to_string_lossy().into_owned();
        let ceiling = MAX_AUDIO_BYTES.to_string();

        // Deliberately absent: any cookie or account flag, so nothing about
        // the user's own YouTube account is ever involved in this; the
        // SponsorBlock post-processors, which re-encode the file and would
        // need ffmpeg, when segments are better skipped at playback where the
        // choice stays reversible; and the metadata and thumbnail embedders,
        // which also need ffmpeg for work this application already does with
        // `lofty` and its own art cache.
        //
        // None of this needs ffmpeg. Where it happens to be installed, yt-dlp
        // tidies up the container afterwards on its own; measured both ways,
        // the file decodes the same either way, so its absence costs nothing
        // and is never checked for.
        let args = vec![
            "--no-playlist",
            "--no-warnings",
            "--no-progress",
            "-f",
            FORMAT,
            "--max-filesize",
            ceiling.as_str(),
            "-o",
            template.as_str(),
            resolved.video_id.as_str(),
        ];

        self.youtube_limiter.acquire();

        let output = match self.runner.run(&args, FETCH_TIMEOUT) {
            Ok(output) => output,
            Err(err) => {
                self.note(&err, &self.youtube_limiter);
                self.log(
                    &YOUTUBE,
                    err.outcome(),
                    resolved.subject(),
                    0,
                    Some(err.to_string()),
                    None,
                );
                return Err(Trouble::Tooling(err.to_string()));
            }
        };

        if !output.succeeded() {
            let trouble = interpret(&output);
            let err = trouble.as_error();
            self.note(&err, &self.youtube_limiter);
            self.log(
                &YOUTUBE,
                err.outcome(),
                resolved.subject(),
                0,
                Some(trouble.detail()),
                None,
            );
            return Err(trouble);
        }

        self.youtube_limiter.note_success();

        let Some(audio) = self.existing_audio(&resolved.video_id) else {
            // The program said it worked and there is nothing there. Worth a
            // log line of its own rather than a silent nothing.
            self.log(
                &YOUTUBE,
                Outcome::Failed,
                resolved.subject(),
                0,
                Some("the download reported success and left no file".to_owned()),
                None,
            );
            return Err(Trouble::Failed(
                "the download reported success and left no file".to_owned(),
            ));
        };

        self.log(
            &YOUTUBE,
            Outcome::Ok,
            resolved.subject(),
            audio.bytes,
            None,
            None,
        );

        Ok(audio)
    }

    /// Get the picture that goes with a video.
    ///
    /// An ordinary request, made by this application rather than by `yt-dlp`,
    /// which is why it is a separate source in the log.
    pub fn fetch_thumbnail(&self, resolved: &Resolved) -> Option<Vec<u8>> {
        let url = resolved.thumbnail.as_deref()?;
        let subject = resolved.subject();

        self.thumbnail_limiter.acquire();

        match self.transport.get_bytes(url) {
            Ok(fetched) if fetched.body.is_empty() => {
                self.thumbnail_limiter.note_success();
                self.log(
                    &YOUTUBE_THUMBNAIL,
                    Outcome::NotFound,
                    subject,
                    0,
                    None,
                    fetched.served_by,
                );
                None
            }
            Ok(fetched) => {
                self.thumbnail_limiter.note_success();
                self.log(
                    &YOUTUBE_THUMBNAIL,
                    Outcome::Ok,
                    subject,
                    fetched.bytes,
                    None,
                    fetched.served_by,
                );
                Some(fetched.body)
            }
            Err(err) => {
                self.note(&err, &self.thumbnail_limiter);
                self.log(
                    &YOUTUBE_THUMBNAIL,
                    err.outcome(),
                    subject,
                    0,
                    Some(err.to_string()),
                    None,
                );
                None
            }
        }
    }

    /// The fetched file for a video, if one is already here.
    ///
    /// Matched by prefix rather than by an assumed extension: the format
    /// fallback can land on a different container, and a partial download
    /// leaves a `.part` beside the real thing which must never be mistaken for
    /// a finished file.
    fn existing_audio(&self, video_id: &str) -> Option<Audio> {
        let entries = std::fs::read_dir(&self.audio_dir).ok()?;

        for entry in entries.flatten() {
            let path = entry.path();

            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };

            if !name.starts_with(video_id) || name.ends_with(".part") {
                continue;
            }

            // `abcdefghijk.m4a` and not `abcdefghijk-something.m4a`.
            if name[video_id.len()..].starts_with('.') {
                let bytes = entry.metadata().ok()?.len();
                if bytes > 0 {
                    return Some(Audio {
                        path,
                        bytes,
                        from_cache: true,
                    });
                }
            }
        }

        None
    }

    fn note(&self, error: &NetError, limiter: &Limiter) {
        if error.is_failure() {
            limiter.note_failure();
        } else {
            limiter.note_success();
        }
    }

    fn store(&self, key: &str, entry: CacheEntry<Resolved>) {
        if let Err(err) = self.cache.write(key, &entry) {
            tracing::warn!("could not cache what a link resolved to: {err}");
        }
    }

    fn log(
        &self,
        source: &Source,
        outcome: Outcome,
        subject: String,
        bytes: u64,
        detail: Option<String>,
        served_by: Option<String>,
    ) {
        let mut entry = LogEntry::new(source, outcome, subject).with_bytes(bytes);

        if let Some(detail) = detail {
            entry = entry.with_detail(detail);
        }

        if let Some(host) = served_by {
            entry = entry.with_host(host);
        }

        self.activity.record(entry);
    }
}

/// What a failed run meant.
///
/// Exit codes here are the program's own convention rather than a standard, so
/// this reads what it said. The distinction being drawn is the same one the
/// rest of the crate draws everywhere: a video that is genuinely not available
/// is a miss, and must not back the limiter off or be retried forever, while a
/// run that broke is a failure and should.
fn interpret(output: &Output) -> Trouble {
    let complaint = output.complaint().unwrap_or("the program failed");
    let lowered = complaint.to_lowercase();

    // Things that are true of the video, and will still be true tomorrow.
    const GONE: &[&str] = &[
        "video unavailable",
        "private video",
        "is not available",
        "has been removed",
        "removed by the uploader",
        "does not exist",
        "members-only",
        "sign in to confirm your age",
        "this live event has ended",
    ];

    // Things that are true of the *program*, and stop being true when it is
    // updated. Every one of these has been seen from a `yt-dlp` a few months
    // behind the service, on videos that play perfectly well.
    //
    // "requested format is not available" belongs here rather than above,
    // which is a correction: it reads like the video offering nothing usable,
    // and in practice it is far more often the extractor getting no formats
    // at all back from a client the service no longer accepts. Treating it as
    // a property of the video meant remembering it for a fortnight, so an
    // update would not visibly fix anything.
    const BEHIND: &[&str] = &[
        "403",
        "forbidden",
        "requested format is not available",
        "the page needs to be reloaded",
        "sign in to confirm you're not a bot",
        "failed to extract any player response",
        "unable to extract",
        "nsig extraction failed",
        "please report this issue",
    ];

    if BEHIND.iter().any(|phrase| lowered.contains(phrase)) {
        return Trouble::Refused(complaint.to_owned());
    }

    if GONE.iter().any(|phrase| lowered.contains(phrase)) {
        return Trouble::Unavailable(complaint.to_owned());
    }

    Trouble::Failed(complaint.to_owned())
}

/// The host part of a URL, without a scheme, a port or a leading `www.`.
fn host_of(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;

    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];

    // Credentials in a URL are not something to copy into a log file.
    let authority = authority.rsplit('@').next()?;
    let host = authority.split(':').next()?;
    let host = host.strip_prefix("www.").unwrap_or(host);

    (!host.is_empty()).then(|| host.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{Fetched, FetchedBytes};
    use std::sync::Mutex;

    // -- fakes --------------------------------------------------------------

    #[derive(Debug, Default)]
    struct FakeRunner {
        scripted: Mutex<Vec<Result<Output, NetError>>>,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl FakeRunner {
        fn with(answers: Vec<Result<Output, NetError>>) -> Arc<Self> {
            Arc::new(Self {
                scripted: Mutex::new(answers),
                calls: Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Runner for FakeRunner {
        fn run(&self, args: &[&str], _timeout: Duration) -> Result<Output, NetError> {
            self.calls
                .lock()
                .unwrap()
                .push(args.iter().map(|arg| (*arg).to_owned()).collect());

            let mut scripted = self.scripted.lock().unwrap();
            assert!(!scripted.is_empty(), "unscripted run: {args:?}");
            scripted.remove(0)
        }
    }

    struct RunnerRef(Arc<FakeRunner>);

    impl Runner for RunnerRef {
        fn run(&self, args: &[&str], timeout: Duration) -> Result<Output, NetError> {
            self.0.run(args, timeout)
        }
    }

    #[derive(Debug, Default)]
    struct FakeTransport {
        binary: Mutex<Vec<Result<FetchedBytes, NetError>>>,
    }

    impl FakeTransport {
        fn with(answers: Vec<Result<FetchedBytes, NetError>>) -> Self {
            Self {
                binary: Mutex::new(answers),
            }
        }
    }

    impl Transport for FakeTransport {
        fn get(&self, url: &str) -> Result<Fetched, NetError> {
            unreachable!("nothing here fetches text: {url}")
        }

        fn get_bytes(&self, _url: &str) -> Result<FetchedBytes, NetError> {
            let mut binary = self.binary.lock().unwrap();
            assert!(!binary.is_empty(), "unscripted image request");
            binary.remove(0)
        }
    }

    fn ran(stdout: &str) -> Output {
        Output {
            status: Some(0),
            stdout: stdout.as_bytes().to_vec(),
            stderr: String::new(),
        }
    }

    fn failed(stderr: &str) -> Output {
        Output {
            status: Some(1),
            stdout: Vec::new(),
            stderr: stderr.to_owned(),
        }
    }

    struct Harness {
        client: Client,
        runner: Arc<FakeRunner>,
        activity: Arc<Activity>,
        _dir: tempfile::TempDir,
        audio_dir: PathBuf,
    }

    fn harness(
        runs: Vec<Result<Output, NetError>>,
        images: Vec<Result<FetchedBytes, NetError>>,
    ) -> Harness {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let audio_dir = dir.path().join("audio");
        let runner = FakeRunner::with(runs);
        let activity = Arc::new(Activity::in_memory());

        let client = Client::with_parts(
            Box::new(RunnerRef(Arc::clone(&runner))),
            Box::new(FakeTransport::with(images)),
            dir.path().join("cache"),
            audio_dir.clone(),
            Arc::clone(&activity),
        );

        Harness {
            client,
            runner,
            activity,
            _dir: dir,
            audio_dir,
        }
    }

    const DUMP: &str = r#"{
        "id": "dQw4w9WgXcQ",
        "title": "Never Gonna Give You Up (Official Video)",
        "uploader": "Rick Astley",
        "duration": 213.0,
        "thumbnail": "https://i.ytimg.com/vi/dQw4w9WgXcQ/maxres.jpg",
        "url": "https://rr3---sn-abcd.googlevideo.com/videoplayback?expire=1"
    }"#;

    // -- the question -------------------------------------------------------

    #[test]
    fn every_shape_of_link_names_the_same_video() {
        let links = [
            "dQw4w9WgXcQ",
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "http://youtube.com/watch?v=dQw4w9WgXcQ",
            "https://youtu.be/dQw4w9WgXcQ",
            "https://youtu.be/dQw4w9WgXcQ?t=43",
            "https://m.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://music.youtube.com/watch?v=dQw4w9WgXcQ&list=RDAMVM",
            "https://www.youtube.com/shorts/dQw4w9WgXcQ",
            "https://www.youtube.com/embed/dQw4w9WgXcQ",
            "https://www.youtube.com/live/dQw4w9WgXcQ",
            "https://www.youtube.com/watch?feature=share&v=dQw4w9WgXcQ",
        ];

        for link in links {
            assert_eq!(
                Query::new(link).video_id().as_deref(),
                Some("dQw4w9WgXcQ"),
                "{link}"
            );
        }
    }

    /// A link this does not recognise is never handed to the program at all,
    /// so what counts as recognised is a security boundary rather than a
    /// convenience.
    #[test]
    fn anything_else_names_no_video() {
        let links = [
            "",
            "   ",
            "not a link",
            "https://example.org/watch?v=dQw4w9WgXcQ",
            "https://youtube.com.evil.test/watch?v=dQw4w9WgXcQ",
            "https://www.youtube.com/watch?v=tooshort",
            "https://www.youtube.com/watch?v=waytoolongtobeanid",
            "https://www.youtube.com/results?search_query=music",
            "https://www.youtube.com/@somechannel",
            "../../etc/passwd",
        ];

        for link in links {
            let query = Query::new(link);
            assert_eq!(query.video_id(), None, "{link}");
            assert!(!query.is_answerable(), "{link}");
        }
    }

    /// An id is used as a filename, so nothing that is not one may pass.
    #[test]
    fn an_id_is_eleven_safe_characters() {
        assert!(is_video_id("dQw4w9WgXcQ"));
        assert!(is_video_id("_-aA0123456"));

        assert!(!is_video_id("short"));
        assert!(!is_video_id("dQw4w9WgXcQextra"));
        assert!(!is_video_id("dQw4w9WgXc/"));
        assert!(!is_video_id("dQw4w9WgXc."));
        assert!(!is_video_id(""));
    }

    #[test]
    fn two_links_to_one_video_share_a_cache_key() {
        let watch = Query::new("https://www.youtube.com/watch?v=dQw4w9WgXcQ");
        let short = Query::new("https://youtu.be/dQw4w9WgXcQ?t=43");

        assert_eq!(watch.cache_key(), short.cache_key());
    }

    // -- resolving ----------------------------------------------------------

    #[test]
    fn resolving_reads_what_came_back() {
        let harness = harness(vec![Ok(ran(DUMP))], vec![]);

        let resolved = harness
            .client
            .resolve(&Query::new("https://youtu.be/dQw4w9WgXcQ"))
            .expect("a resolved video");

        assert_eq!(resolved.video_id, "dQw4w9WgXcQ");
        assert_eq!(resolved.title, "Never Gonna Give You Up (Official Video)");
        assert_eq!(resolved.artist, "Rick Astley");
        assert_eq!(resolved.duration, Some(Duration::from_secs(213)));
        assert!(resolved.thumbnail.is_some());
    }

    /// The recording is a better answer than the upload, and YouTube Music
    /// knows the difference.
    #[test]
    fn a_known_recording_beats_the_channel_name() {
        let dump = r#"{
            "id": "dQw4w9WgXcQ",
            "title": "Song Title (Official Audio)",
            "track": "Song Title",
            "artist": "The Band",
            "album": "The Record",
            "uploader": "TheBand - Topic"
        }"#;
        let harness = harness(vec![Ok(ran(dump))], vec![]);

        let resolved = harness
            .client
            .resolve(&Query::new("dQw4w9WgXcQ"))
            .expect("a resolved video");

        assert_eq!(resolved.title, "Song Title");
        assert_eq!(resolved.artist, "The Band");
        assert_eq!(resolved.album.as_deref(), Some("The Record"));
    }

    /// The whole reason a host is recorded separately from the one addressed:
    /// the request goes to YouTube and the audio comes from somewhere else.
    #[test]
    fn the_log_names_the_machine_that_will_serve_the_audio() {
        let harness = harness(vec![Ok(ran(DUMP))], vec![]);

        let _ = harness.client.resolve(&Query::new("dQw4w9WgXcQ"));

        let entry = &harness.activity.recent()[0];
        assert_eq!(entry.source, "youtube");
        assert_eq!(entry.host, "rr3---sn-abcd.googlevideo.com");
        assert_eq!(entry.outcome, Outcome::Ok);
    }

    #[test]
    fn the_format_asked_for_is_aac_and_never_falls_back_to_opus() {
        let harness = harness(vec![Ok(ran(DUMP))], vec![]);

        let _ = harness.client.resolve(&Query::new("dQw4w9WgXcQ"));

        let args = harness.runner.calls()[0].join(" ");
        assert!(args.contains(FORMAT), "{args}");
        assert!(
            !args.contains("bestaudio "),
            "a bare bestaudio fallback would select Opus: {args}"
        );
    }

    #[test]
    fn a_second_ask_about_the_same_link_runs_nothing() {
        let harness = harness(vec![Ok(ran(DUMP))], vec![]);
        let query = Query::new("https://youtu.be/dQw4w9WgXcQ");

        let first = harness.client.resolve(&query).expect("resolved");
        let again = harness.client.resolve(&query).expect("from the cache");

        assert_eq!(first, again);
        assert_eq!(harness.runner.calls().len(), 1, "it ran twice");

        let outcomes: Vec<_> = harness
            .activity
            .recent()
            .iter()
            .map(|entry| entry.outcome)
            .collect();
        assert_eq!(outcomes, vec![Outcome::Cached, Outcome::Ok]);
    }

    #[test]
    fn a_video_that_is_gone_is_a_miss_and_is_remembered() {
        let harness = harness(
            vec![Ok(failed(
                "ERROR: [youtube] dQw4w9WgXcQ: Video unavailable",
            ))],
            vec![],
        );
        let query = Query::new("dQw4w9WgXcQ");

        assert!(harness.client.resolve(&query).is_err());
        assert!(harness.client.resolve(&query).is_err());

        assert_eq!(harness.runner.calls().len(), 1, "a miss was not remembered");
        assert_eq!(harness.activity.recent()[1].outcome, Outcome::NotFound);
    }

    /// This test used to assert the opposite, and asserting it is what let the
    /// bug ship.
    ///
    /// The reasoning was that "requested format is not available" means a
    /// video offering only Opus — real, but rare. What it means far more often
    /// is that the extractor asked a client the service no longer accepts and
    /// got no formats back at all, which is a property of the program and not
    /// of the video. Filed as a miss it was remembered for a fortnight, so
    /// updating `yt-dlp` appeared to fix nothing.
    ///
    /// The cost of being wrong the other way is one wasted lookup on a genuine
    /// Opus-only video, which is much the cheaper mistake.
    #[test]
    fn nothing_playable_on_offer_blames_the_program_not_the_video() {
        let harness = harness(
            vec![Ok(failed("ERROR: Requested format is not available"))],
            vec![],
        );

        assert!(harness.client.resolve(&Query::new("dQw4w9WgXcQ")).is_err());
        assert_eq!(harness.activity.recent()[0].outcome, Outcome::Failed);
    }

    /// One bad afternoon should not become a fortnight of a link refusing to
    /// play, so a broken run is never remembered as an answer.
    #[test]
    fn a_broken_run_is_not_remembered() {
        let harness = harness(
            vec![
                Ok(failed("ERROR: unable to download: connection reset")),
                Ok(ran(DUMP)),
            ],
            vec![],
        );
        let query = Query::new("dQw4w9WgXcQ");

        assert!(harness.client.resolve(&query).is_err());
        assert!(harness.client.resolve(&query).is_ok(), "it gave up");

        assert_eq!(harness.runner.calls().len(), 2);
        assert_eq!(harness.activity.recent()[1].outcome, Outcome::Failed);
    }

    #[test]
    fn output_that_is_not_json_is_a_failure_rather_than_a_panic() {
        let harness = harness(vec![Ok(ran("this is not json"))], vec![]);

        assert!(harness.client.resolve(&Query::new("dQw4w9WgXcQ")).is_err());
        assert_eq!(harness.activity.recent()[0].outcome, Outcome::Failed);
    }

    #[test]
    fn a_link_naming_no_video_runs_nothing_and_logs_nothing() {
        let harness = harness(vec![], vec![]);

        assert!(harness.client.resolve(&Query::new("not a link")).is_err());

        assert!(harness.runner.calls().is_empty());
        assert!(harness.activity.recent().is_empty());
    }

    /// The failure a user actually hits, and the reason this distinction
    /// exists. YouTube answers and refuses; the video is fine; the fix is to
    /// update the program. Remembering it as a missing video would mean the
    /// update fixed nothing for a fortnight.
    #[test]
    fn a_refusal_is_not_remembered_and_says_what_to_do() {
        let harness = harness(
            vec![
                Ok(failed(
                    "ERROR: unable to download video data: HTTP Error 403: Forbidden",
                )),
                Ok(ran(DUMP)),
            ],
            vec![],
        );
        let query = Query::new("dQw4w9WgXcQ");

        let trouble = harness.client.resolve(&query).expect_err("a refusal");
        assert!(matches!(trouble, Trouble::Refused(_)));
        assert!(
            trouble.message().contains("out of date"),
            "{}",
            trouble.message()
        );

        // Asked again rather than remembered, so an update takes effect at once.
        assert!(
            harness.client.resolve(&query).is_ok(),
            "the refusal was remembered"
        );
        assert_eq!(harness.runner.calls().len(), 2);
    }

    /// This one used to be filed as a property of the video, which meant an
    /// out-of-date extractor getting no formats back was remembered for a
    /// fortnight as "this video has nothing playable".
    #[test]
    fn no_formats_offered_reads_as_the_program_being_behind() {
        let harness = harness(
            vec![Ok(failed("ERROR: Requested format is not available"))],
            vec![],
        );

        let trouble = harness
            .client
            .resolve(&Query::new("dQw4w9WgXcQ"))
            .expect_err("a refusal");

        assert!(matches!(trouble, Trouble::Refused(_)), "{trouble:?}");
    }

    #[test]
    fn a_video_that_is_gone_says_so_rather_than_blaming_the_program() {
        let harness = harness(
            vec![Ok(failed("ERROR: [youtube] x: Private video"))],
            vec![],
        );

        let trouble = harness
            .client
            .resolve(&Query::new("dQw4w9WgXcQ"))
            .expect_err("unavailable");

        assert!(matches!(trouble, Trouble::Unavailable(_)), "{trouble:?}");
        assert!(trouble.message().contains("not available"));
        assert!(!trouble.message().contains("out of date"));
    }

    /// The log's detail column gets what the program actually said, however
    /// the message on screen is worded.
    #[test]
    fn the_log_keeps_the_programs_own_words() {
        let harness = harness(vec![Ok(failed("ERROR: HTTP Error 403: Forbidden"))], vec![]);

        let _ = harness.client.resolve(&Query::new("dQw4w9WgXcQ"));

        let detail = harness.activity.recent()[0]
            .detail
            .clone()
            .expect("a detail");
        assert!(detail.contains("403"), "{detail}");
    }

    // -- fetching -----------------------------------------------------------

    fn resolved() -> Resolved {
        Resolved {
            video_id: "dQw4w9WgXcQ".into(),
            title: "Never Gonna Give You Up".into(),
            artist: "Rick Astley".into(),
            album: None,
            duration: Some(Duration::from_secs(213)),
            thumbnail: Some("https://i.ytimg.com/vi/dQw4w9WgXcQ/maxres.jpg".into()),
        }
    }

    #[test]
    fn a_file_already_here_is_used_and_nothing_runs() {
        let harness = harness(vec![], vec![]);
        std::fs::create_dir_all(&harness.audio_dir).unwrap();
        let path = harness.audio_dir.join("dQw4w9WgXcQ.m4a");
        std::fs::write(&path, b"pretend this is audio").unwrap();

        let audio = harness.client.fetch_audio(&resolved()).expect("the file");

        assert_eq!(audio.path, path);
        assert!(audio.from_cache);
        assert!(harness.runner.calls().is_empty());
        assert_eq!(harness.activity.recent()[0].outcome, Outcome::Cached);
    }

    /// A partial download must never be mistaken for a finished one, or a
    /// track plays as a few seconds of silence and a decode error.
    #[test]
    fn a_partial_download_is_not_a_file() {
        let harness = harness(vec![], vec![]);
        std::fs::create_dir_all(&harness.audio_dir).unwrap();
        std::fs::write(harness.audio_dir.join("dQw4w9WgXcQ.m4a.part"), b"half").unwrap();

        assert!(harness.client.existing_audio("dQw4w9WgXcQ").is_none());
    }

    /// Prefix matching must not turn one video's id into another video's file.
    #[test]
    fn a_longer_name_is_a_different_video() {
        let harness = harness(vec![], vec![]);
        std::fs::create_dir_all(&harness.audio_dir).unwrap();
        std::fs::write(harness.audio_dir.join("dQw4w9WgXcQ-remix.m4a"), b"other").unwrap();

        assert!(harness.client.existing_audio("dQw4w9WgXcQ").is_none());
    }

    #[test]
    fn an_empty_file_does_not_count() {
        let harness = harness(vec![], vec![]);
        std::fs::create_dir_all(&harness.audio_dir).unwrap();
        std::fs::write(harness.audio_dir.join("dQw4w9WgXcQ.m4a"), b"").unwrap();

        assert!(harness.client.existing_audio("dQw4w9WgXcQ").is_none());
    }

    #[test]
    fn a_download_that_leaves_nothing_behind_is_a_failure() {
        let harness = harness(vec![Ok(ran(""))], vec![]);

        assert!(harness.client.fetch_audio(&resolved()).is_err());

        let entry = &harness.activity.recent()[0];
        assert_eq!(entry.outcome, Outcome::Failed);
        assert!(entry.detail.as_deref().unwrap().contains("left no file"));
    }

    #[test]
    fn the_download_is_capped_and_never_touches_an_account() {
        let harness = harness(vec![Ok(ran(""))], vec![]);

        let _ = harness.client.fetch_audio(&resolved());

        let args = harness.runner.calls()[0].join(" ");
        assert!(args.contains("--max-filesize"), "{args}");
        assert!(args.contains(&MAX_AUDIO_BYTES.to_string()), "{args}");
        assert!(args.contains("--no-playlist"), "{args}");
        for forbidden in ["--cookies", "--cookies-from-browser", "--username"] {
            assert!(!args.contains(forbidden), "{forbidden} in {args}");
        }
    }

    /// Re-encoding to cut sponsor segments needs ffmpeg and cannot be undone.
    /// They are skipped at playback instead, where the choice stays reversible.
    #[test]
    fn segments_are_not_cut_out_of_the_file() {
        let harness = harness(vec![Ok(ran(""))], vec![]);

        let _ = harness.client.fetch_audio(&resolved());

        let args = harness.runner.calls()[0].join(" ");
        assert!(!args.contains("--sponsorblock"), "{args}");
        assert!(!args.contains("--embed"), "{args}");
    }

    // -- thumbnails ---------------------------------------------------------

    #[test]
    fn a_thumbnail_is_its_own_source_in_the_log() {
        let harness = harness(
            vec![],
            vec![Ok(FetchedBytes {
                body: vec![0xFF, 0xD8, 0xFF, 0x00],
                bytes: 4,
                served_by: Some("i.ytimg.com".into()),
            })],
        );

        let bytes = harness
            .client
            .fetch_thumbnail(&resolved())
            .expect("an image");

        assert_eq!(bytes.len(), 4);

        let entry = &harness.activity.recent()[0];
        assert_eq!(entry.source, "youtube-thumbnail");
        assert_eq!(entry.host, "i.ytimg.com");
        assert_eq!(entry.bytes, 4);
    }

    #[test]
    fn a_video_with_no_picture_asks_for_nothing() {
        let harness = harness(vec![], vec![]);
        let mut resolved = resolved();
        resolved.thumbnail = None;

        assert!(harness.client.fetch_thumbnail(&resolved).is_none());
        assert!(harness.activity.recent().is_empty());
    }

    #[test]
    fn an_empty_image_is_a_miss_rather_than_a_cover() {
        let harness = harness(
            vec![],
            vec![Ok(FetchedBytes {
                body: Vec::new(),
                bytes: 0,
                served_by: None,
            })],
        );

        assert!(harness.client.fetch_thumbnail(&resolved()).is_none());
        assert_eq!(harness.activity.recent()[0].outcome, Outcome::NotFound);
    }

    // -- reading a host -----------------------------------------------------

    #[test]
    fn a_host_comes_out_bare() {
        assert_eq!(
            host_of("https://rr3---sn-abcd.googlevideo.com/videoplayback?expire=1").as_deref(),
            Some("rr3---sn-abcd.googlevideo.com")
        );
        assert_eq!(
            host_of("https://www.example.org/thing").as_deref(),
            Some("example.org")
        );
        assert_eq!(
            host_of("http://example.org:8080/thing").as_deref(),
            Some("example.org")
        );
    }

    /// Credentials in a URL are not something to copy into a file the user is
    /// invited to read and share.
    #[test]
    fn credentials_never_reach_the_log() {
        assert_eq!(
            host_of("https://user:secret@example.org/thing").as_deref(),
            Some("example.org")
        );
    }

    #[test]
    fn something_that_is_not_a_url_has_no_host() {
        assert_eq!(host_of("not a url"), None);
        assert_eq!(host_of(""), None);
        assert_eq!(host_of("https://"), None);
    }
}
