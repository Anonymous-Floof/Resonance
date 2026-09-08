//! Which parts of a video are not the song.
//!
//! A community database of sponsor reads, intros, outros and the talking
//! before the music starts. Resonance asks it what to skip and skips it at
//! playback, rather than cutting the file.
//!
//! # Asking without saying what you are listening to
//!
//! The obvious endpoint takes a video identifier. This one does not: it takes
//! the **first four characters of a SHA-256 of the identifier**, and answers
//! with the segments for every video sharing that prefix. The answer is
//! filtered here, on this machine.
//!
//! That is the whole reason this feature is shaped the way it is. A lookup
//! that named the video would tell a third party what somebody is listening
//! to, one track at a time, and no amount of it being a nice third party makes
//! that a thing to do quietly. Four characters is one in 65,536 — enough that
//! the answer is small, far too little to identify anything.
//!
//! It also makes the sentence on the opt-in screen an easy one to write, which
//! is usually the sign of a design worth keeping.
//!
//! # Skipped, not cut
//!
//! `yt-dlp` can remove these segments from the file as it downloads. This does
//! not use that, for three reasons: it re-encodes, which needs ffmpeg; it
//! throws away audio that cannot be got back without downloading again; and it
//! bakes in a choice the user might want to change. Skipping at playback keeps
//! all three open.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::activity::{Activity, Entry as LogEntry, Outcome};
use crate::cache::{Cache, Entry as CacheEntry};
use crate::error::NetError;
use crate::http::Transport;
use crate::rate::Limiter;
use crate::source::{SPONSORBLOCK, Source};

/// Where cached answers are kept, under the cache root.
pub const CACHE_NAMESPACE: &str = "sponsorblock";

/// How much of the hash to send.
///
/// Four is what the service recommends, and the trade is legible: fewer
/// characters means a bigger answer and less to go on, more means a smaller
/// answer and more. Four returns the segments for one video in 65,536.
pub const HASH_PREFIX_CHARS: usize = 4;

/// What counts as not the song.
///
/// `music_offtopic` is the one that earns this feature in a music player: it
/// is the label for the part of a music video that is not the music. The rest
/// are the ordinary ones.
///
/// `filler` is deliberately absent. It marks tangential material that is still
/// part of what was uploaded, and cutting it is an editorial opinion rather
/// than the removal of an advert.
pub const CATEGORIES: &[&str] = &[
    "sponsor",
    "selfpromo",
    "interaction",
    "intro",
    "outro",
    "preview",
    "music_offtopic",
];

/// The only kind of segment this can act on.
///
/// The service also describes segments to mute, points of interest and chapter
/// marks. None of those are skips, and treating them as one would cut audio
/// nobody asked to lose.
const SKIP: &str = "skip";

// ---------------------------------------------------------------------------
// The question
// ---------------------------------------------------------------------------

/// One video, asked about without being named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    video_id: String,
}

impl Query {
    pub fn new(video_id: impl Into<String>) -> Self {
        Self {
            video_id: video_id.into(),
        }
    }

    pub fn video_id(&self) -> &str {
        &self.video_id
    }

    pub fn is_answerable(&self) -> bool {
        !self.video_id.trim().is_empty()
    }

    /// The first [`HASH_PREFIX_CHARS`] of the SHA-256 of the identifier.
    ///
    /// This, and never the identifier, is what leaves the machine.
    pub fn hash_prefix(&self) -> String {
        let digest = ring::digest::digest(&ring::digest::SHA256, self.video_id.as_bytes());

        digest
            .as_ref()
            .iter()
            .flat_map(|byte| {
                let hex = b"0123456789abcdef";
                [
                    hex[(byte >> 4) as usize] as char,
                    hex[(byte & 0x0f) as usize] as char,
                ]
            })
            .take(HASH_PREFIX_CHARS)
            .collect()
    }

    pub fn url(&self) -> String {
        format!(
            "https://{}/api/skipSegments/{}?categories={}",
            SPONSORBLOCK.host,
            self.hash_prefix(),
            encode(&serde_json::to_string(CATEGORIES).unwrap_or_default())
        )
    }

    pub fn cache_key(&self) -> String {
        crate::cache::key(&["sponsorblock", &self.video_id])
    }

    /// How this reads in the activity log.
    ///
    /// Names the prefix rather than the video, because that is what was
    /// actually sent and the log is a record of what happened.
    pub fn subject(&self) -> String {
        format!("segments for videos matching {}", self.hash_prefix())
    }
}

/// Percent-encode everything but the unreserved set.
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());

    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }

    out
}

// ---------------------------------------------------------------------------
// The answer
// ---------------------------------------------------------------------------

/// A stretch of a video that is not the song.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub start: f64,
    pub end: f64,
}

impl Segment {
    pub fn length(&self) -> f64 {
        self.end - self.start
    }
}

#[derive(Debug, Deserialize)]
struct WireVideo {
    #[serde(rename = "videoID", default)]
    video_id: String,
    #[serde(default)]
    segments: Vec<WireSegment>,
}

#[derive(Debug, Deserialize)]
struct WireSegment {
    /// `[start, end]` in seconds. A `Vec` rather than an array so a malformed
    /// pair is one skipped segment instead of a failed parse of the whole
    /// answer.
    #[serde(default)]
    segment: Vec<f64>,
    #[serde(default)]
    category: String,
    #[serde(rename = "actionType", default)]
    action_type: String,
}

/// Pick out the segments for one video, and make them safe to act on.
///
/// The answer covers every video sharing the prefix, so the first job is
/// finding the right one. After that: only skips, only categories that were
/// asked for, nothing of zero or negative length, in order, and with overlaps
/// merged — two overlapping sponsor reads would otherwise be two seeks, the
/// second of them backwards.
fn usable(videos: &[WireVideo], video_id: &str) -> Vec<Segment> {
    let Some(video) = videos.iter().find(|video| video.video_id == video_id) else {
        return Vec::new();
    };

    let mut segments: Vec<Segment> = video
        .segments
        .iter()
        .filter(|wire| wire.action_type == SKIP)
        .filter(|wire| CATEGORIES.contains(&wire.category.as_str()))
        .filter_map(|wire| {
            let [start, end] = wire.segment.as_slice() else {
                return None;
            };

            let (start, end) = (*start, *end);

            (start.is_finite() && end.is_finite() && start >= 0.0 && end > start)
                .then_some(Segment { start, end })
        })
        .collect();

    segments.sort_by(|a, b| a.start.total_cmp(&b.start));

    let mut merged: Vec<Segment> = Vec::with_capacity(segments.len());
    for segment in segments {
        match merged.last_mut() {
            Some(last) if segment.start <= last.end => last.end = last.end.max(segment.end),
            _ => merged.push(segment),
        }
    }

    merged
}

// ---------------------------------------------------------------------------
// The client
// ---------------------------------------------------------------------------

/// Asks what to skip.
pub struct Client {
    transport: Box<dyn Transport>,
    limiter: Limiter,
    cache: Cache,
    activity: Arc<Activity>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").finish_non_exhaustive()
    }
}

impl Client {
    pub fn new(cache_root: impl Into<std::path::PathBuf>, activity: Arc<Activity>) -> Self {
        Self::with_transport(Box::new(crate::http::Http::new()), cache_root, activity)
    }

    /// A client over any transport. The seam the tests use.
    pub fn with_transport(
        transport: Box<dyn Transport>,
        cache_root: impl Into<std::path::PathBuf>,
        activity: Arc<Activity>,
    ) -> Self {
        Self {
            transport,
            limiter: Limiter::for_source(&SPONSORBLOCK),
            cache: Cache::new(cache_root.into().join(CACHE_NAMESPACE)),
            activity,
        }
    }

    pub fn source(&self) -> &'static Source {
        &SPONSORBLOCK
    }

    pub fn cache(&self) -> &Cache {
        &self.cache
    }

    /// What to skip in this video.
    ///
    /// **Blocks.** Background threads only. An empty list and a failure are
    /// both `None` at the call site — nothing is skipped either way, and the
    /// activity log carries which it was.
    pub fn fetch(&self, query: &Query) -> Option<Vec<Segment>> {
        if !query.is_answerable() {
            return None;
        }

        let key = query.cache_key();

        if let Some(entry) = self.cache.read::<Vec<Segment>>(&key) {
            self.log(Outcome::Cached, query, 0, None);

            // Filtered the same way the fresh path filters below. A remembered
            // empty list and a remembered miss both mean nothing to skip, and
            // the caller should not have to know which kind of nothing it is.
            return entry.found.filter(|segments| !segments.is_empty());
        }

        self.limiter.acquire();

        let fetched = match self.transport.get(&query.url()) {
            Ok(fetched) => fetched,
            Err(err) => {
                if err.is_failure() {
                    self.limiter.note_failure();
                } else {
                    self.limiter.note_success();
                }

                // A 404 here means nothing shares the prefix, which is a real
                // answer and worth remembering.
                if matches!(err, NetError::NotFound) {
                    self.store(&key, CacheEntry::<Vec<Segment>>::missing());
                }

                self.log(err.outcome(), query, 0, Some(err.to_string()));
                return None;
            }
        };

        self.limiter.note_success();

        let videos: Vec<WireVideo> = match serde_json::from_str(&fetched.body) {
            Ok(videos) => videos,
            Err(err) => {
                self.log(Outcome::Failed, query, fetched.bytes, Some(err.to_string()));
                return None;
            }
        };

        let segments = usable(&videos, query.video_id());

        // Remembered either way. "This video has nothing to skip" is an answer
        // worth keeping, and without it every replay asks again.
        self.store(&key, CacheEntry::found(segments.clone()));
        self.log(
            if segments.is_empty() {
                Outcome::NotFound
            } else {
                Outcome::Ok
            },
            query,
            fetched.bytes,
            Some(format!("{} to skip", segments.len())),
        );

        (!segments.is_empty()).then_some(segments)
    }

    fn store(&self, key: &str, entry: CacheEntry<Vec<Segment>>) {
        if let Err(err) = self.cache.write(key, &entry) {
            tracing::warn!("could not cache what to skip: {err}");
        }
    }

    fn log(&self, outcome: Outcome, query: &Query, bytes: u64, detail: Option<String>) {
        let mut entry = LogEntry::new(&SPONSORBLOCK, outcome, query.subject()).with_bytes(bytes);

        if let Some(detail) = detail {
            entry = entry.with_detail(detail);
        }

        self.activity.record(entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{Fetched, FetchedBytes};
    use std::sync::Mutex;

    // -- the hash -----------------------------------------------------------

    /// Checked against the published SHA-256 of these inputs. If this is
    /// wrong, every lookup asks about the wrong bucket and silently finds
    /// nothing, which is indistinguishable from the service having no data.
    #[test]
    fn the_hash_is_really_sha256() {
        // SHA-256("abc") = ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
        assert_eq!(Query::new("abc").hash_prefix(), "ba78");

        // SHA-256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        assert_eq!(Query::new("").hash_prefix(), "e3b0");

        // SHA-256("dQw4w9WgXcQ") = 5f6b0b4e... - checked against an
        // independent implementation, and against the bucket the live service
        // actually returns for that prefix.
        assert_eq!(Query::new("dQw4w9WgXcQ").hash_prefix(), "5f6b");
    }

    /// The whole privacy claim in one assertion: what goes out names a bucket
    /// of 65,536 videos, and never the one being played.
    #[test]
    fn the_video_identifier_never_leaves() {
        let query = Query::new("dQw4w9WgXcQ");
        let url = query.url();

        assert!(
            !url.contains("dQw4w9WgXcQ"),
            "the video id went out in the url: {url}"
        );
        assert!(url.contains(&query.hash_prefix()), "{url}");
        assert!(url.contains("sponsor.ajay.app"), "{url}");
    }

    #[test]
    fn the_prefix_is_short_enough_to_be_shared() {
        // Four hex characters is one bucket in 16^4.
        assert_eq!(HASH_PREFIX_CHARS, 4);
        assert_eq!(Query::new("anything").hash_prefix().len(), 4);
    }

    #[test]
    fn the_categories_asked_for_are_the_ones_acted_on() {
        let url = Query::new("dQw4w9WgXcQ").url();

        // Sent url-encoded, so check the one that matters most for music.
        assert!(url.contains("music_offtopic"), "{url}");
        assert!(!url.contains("filler"), "filler is not skipped: {url}");
    }

    // -- reading the answer -------------------------------------------------

    fn wire(video_id: &str, segments: &[(f64, f64, &str, &str)]) -> WireVideo {
        WireVideo {
            video_id: video_id.to_owned(),
            segments: segments
                .iter()
                .map(|(start, end, category, action)| WireSegment {
                    segment: vec![*start, *end],
                    category: (*category).to_owned(),
                    action_type: (*action).to_owned(),
                })
                .collect(),
        }
    }

    /// The answer covers every video sharing the prefix. Acting on somebody
    /// else's segments would cut the middle out of the wrong song.
    #[test]
    fn only_the_video_that_was_asked_about_is_acted_on() {
        let videos = vec![
            wire("someoneelse", &[(0.0, 30.0, "sponsor", "skip")]),
            wire("dQw4w9WgXcQ", &[(5.0, 10.0, "sponsor", "skip")]),
        ];

        let segments = usable(&videos, "dQw4w9WgXcQ");

        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].start, 5.0);
    }

    #[test]
    fn a_video_that_is_not_in_the_answer_has_nothing_to_skip() {
        let videos = vec![wire("someoneelse", &[(0.0, 30.0, "sponsor", "skip")])];

        assert!(usable(&videos, "dQw4w9WgXcQ").is_empty());
    }

    /// Muting and marking a point of interest are not skipping, and treating
    /// them as one would cut audio nobody asked to lose.
    #[test]
    fn only_skips_are_skipped() {
        let videos = vec![wire(
            "v",
            &[
                (0.0, 5.0, "sponsor", "mute"),
                (10.0, 12.0, "poi_highlight", "poi"),
                (20.0, 25.0, "sponsor", "skip"),
            ],
        )];

        let segments = usable(&videos, "v");

        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].start, 20.0);
    }

    #[test]
    fn a_category_that_was_not_asked_for_is_left_alone() {
        let videos = vec![wire(
            "v",
            &[
                (0.0, 5.0, "filler", "skip"),
                (10.0, 15.0, "music_offtopic", "skip"),
            ],
        )];

        let segments = usable(&videos, "v");

        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].start, 10.0);
    }

    /// Two overlapping segments would be two seeks, the second of them
    /// backwards into audio that was just skipped.
    #[test]
    fn overlapping_segments_become_one() {
        let videos = vec![wire(
            "v",
            &[
                (0.0, 30.0, "sponsor", "skip"),
                (20.0, 45.0, "selfpromo", "skip"),
                (50.0, 60.0, "outro", "skip"),
            ],
        )];

        let segments = usable(&videos, "v");

        assert_eq!(segments.len(), 2);
        assert_eq!(
            segments[0],
            Segment {
                start: 0.0,
                end: 45.0
            }
        );
        assert_eq!(
            segments[1],
            Segment {
                start: 50.0,
                end: 60.0
            }
        );
    }

    #[test]
    fn segments_come_back_in_order() {
        let videos = vec![wire(
            "v",
            &[
                (100.0, 110.0, "sponsor", "skip"),
                (5.0, 10.0, "intro", "skip"),
            ],
        )];

        let segments = usable(&videos, "v");

        assert_eq!(segments[0].start, 5.0);
        assert_eq!(segments[1].start, 100.0);
    }

    /// A segment that runs backwards, or takes no time, or arrives malformed,
    /// is a seek to nowhere.
    #[test]
    fn nonsense_is_discarded_rather_than_acted_on() {
        let mut video = wire(
            "v",
            &[
                (30.0, 10.0, "sponsor", "skip"),
                (5.0, 5.0, "sponsor", "skip"),
                (-4.0, 8.0, "sponsor", "skip"),
            ],
        );

        // A pair that is not a pair at all.
        video.segments.push(WireSegment {
            segment: vec![1.0],
            category: "sponsor".into(),
            action_type: SKIP.into(),
        });
        video.segments.push(WireSegment {
            segment: vec![f64::NAN, 9.0],
            category: "sponsor".into(),
            action_type: SKIP.into(),
        });

        assert!(usable(&[video], "v").is_empty());
    }

    // -- the client ---------------------------------------------------------

    #[derive(Debug, Default)]
    struct Fake {
        scripted: Mutex<Vec<Result<Fetched, NetError>>>,
        urls: Mutex<Vec<String>>,
    }

    impl Fake {
        fn with(answers: Vec<Result<Fetched, NetError>>) -> Arc<Self> {
            Arc::new(Self {
                scripted: Mutex::new(answers),
                urls: Mutex::new(Vec::new()),
            })
        }
    }

    struct Scripted(Arc<Fake>);

    impl Transport for Scripted {
        fn get(&self, url: &str) -> Result<Fetched, NetError> {
            self.0.urls.lock().unwrap().push(url.to_owned());

            let mut scripted = self.0.scripted.lock().unwrap();
            assert!(!scripted.is_empty(), "unscripted request: {url}");
            scripted.remove(0)
        }

        fn get_bytes(&self, url: &str) -> Result<FetchedBytes, NetError> {
            unreachable!("segments are text: {url}")
        }
    }

    fn body(text: &str) -> Fetched {
        Fetched {
            bytes: text.len() as u64,
            body: text.to_owned(),
        }
    }

    struct Harness {
        client: Client,
        fake: Arc<Fake>,
        activity: Arc<Activity>,
        _dir: tempfile::TempDir,
    }

    fn harness(answers: Vec<Result<Fetched, NetError>>) -> Harness {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let fake = Fake::with(answers);
        let activity = Arc::new(Activity::in_memory());

        let client = Client::with_transport(
            Box::new(Scripted(Arc::clone(&fake))),
            dir.path(),
            Arc::clone(&activity),
        );

        Harness {
            client,
            fake,
            activity,
            _dir: dir,
        }
    }

    const ANSWER: &str = r#"[
        {"videoID":"dQw4w9WgXcQ","segments":[
            {"segment":[0.0,12.5],"category":"intro","actionType":"skip","UUID":"a"},
            {"segment":[180.0,200.0],"category":"outro","actionType":"skip","UUID":"b"}
        ]},
        {"videoID":"otherVideoX","segments":[
            {"segment":[0.0,99.0],"category":"sponsor","actionType":"skip","UUID":"c"}
        ]}
    ]"#;

    #[test]
    fn a_lookup_returns_only_this_videos_segments() {
        let harness = harness(vec![Ok(body(ANSWER))]);

        let segments = harness
            .client
            .fetch(&Query::new("dQw4w9WgXcQ"))
            .expect("segments");

        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].start, 0.0);
        assert_eq!(segments[0].end, 12.5);
        assert_eq!(segments[1].start, 180.0);
    }

    #[test]
    fn the_url_that_went_out_carries_a_prefix_and_not_an_id() {
        let harness = harness(vec![Ok(body(ANSWER))]);

        harness.client.fetch(&Query::new("dQw4w9WgXcQ"));

        let url = &harness.fake.urls.lock().unwrap()[0];
        assert!(!url.contains("dQw4w9WgXcQ"), "{url}");
    }

    #[test]
    fn a_second_ask_about_the_same_video_makes_no_request() {
        let harness = harness(vec![Ok(body(ANSWER))]);
        let query = Query::new("dQw4w9WgXcQ");

        let first = harness.client.fetch(&query).expect("segments");
        let again = harness.client.fetch(&query).expect("from the cache");

        assert_eq!(first, again);
        assert_eq!(harness.fake.urls.lock().unwrap().len(), 1);
        assert_eq!(harness.activity.recent()[0].outcome, Outcome::Cached);
    }

    /// "Nothing to skip" is an answer, and without remembering it every replay
    /// asks again for the life of the library.
    #[test]
    fn having_nothing_to_skip_is_remembered() {
        let harness = harness(vec![Ok(body(r#"[{"videoID":"v","segments":[]}]"#))]);
        let query = Query::new("v");

        assert!(harness.client.fetch(&query).is_none());
        assert!(harness.client.fetch(&query).is_none());

        assert_eq!(harness.fake.urls.lock().unwrap().len(), 1);
        assert_eq!(harness.activity.recent()[1].outcome, Outcome::NotFound);
    }

    #[test]
    fn a_failure_is_not_remembered() {
        let harness = harness(vec![
            Err(NetError::Transport("down".into())),
            Ok(body(ANSWER)),
        ]);
        let query = Query::new("dQw4w9WgXcQ");

        assert!(harness.client.fetch(&query).is_none());
        assert!(harness.client.fetch(&query).is_some(), "it gave up");

        assert_eq!(harness.fake.urls.lock().unwrap().len(), 2);
    }

    #[test]
    fn nothing_sharing_the_prefix_is_a_remembered_miss() {
        let harness = harness(vec![Err(NetError::NotFound)]);
        let query = Query::new("v");

        assert!(harness.client.fetch(&query).is_none());
        assert!(harness.client.fetch(&query).is_none());

        assert_eq!(harness.fake.urls.lock().unwrap().len(), 1);
    }

    #[test]
    fn rubbish_is_a_failure_rather_than_a_panic() {
        let harness = harness(vec![Ok(body("not json at all"))]);

        assert!(harness.client.fetch(&Query::new("v")).is_none());
        assert_eq!(harness.activity.recent()[0].outcome, Outcome::Failed);
    }

    #[test]
    fn an_empty_video_id_asks_nothing_and_logs_nothing() {
        let harness = harness(vec![]);

        assert!(harness.client.fetch(&Query::new("  ")).is_none());
        assert!(harness.activity.recent().is_empty());
    }

    /// The log is a record of what happened, so it names what was actually
    /// sent rather than what it was about.
    #[test]
    fn the_log_names_the_prefix_rather_than_the_video() {
        let harness = harness(vec![Ok(body(ANSWER))]);
        let query = Query::new("dQw4w9WgXcQ");

        harness.client.fetch(&query);

        let entry = &harness.activity.recent()[0];
        assert_eq!(entry.source, "sponsorblock");
        assert!(!entry.subject.contains("dQw4w9WgXcQ"), "{}", entry.subject);
        assert!(entry.subject.contains(&query.hash_prefix()));
    }
}
