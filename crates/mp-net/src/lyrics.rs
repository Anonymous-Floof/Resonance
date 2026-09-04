//! Fetching lyrics from LRCLIB.
//!
//! The first thing on this branch that actually leaves the machine, and it was
//! chosen to go first for two reasons.
//!
//! It fills a hole nothing offline can. Words are not in the audio, so a track
//! without an `.lrc` beside it or a `USLT` frame inside it simply has none,
//! however clever the analysis gets.
//!
//! And it asks a question with one right answer. [`Query`] carries an artist,
//! a title, an album and a duration, and LRCLIB matches on all four — so there
//! is no "did you mean" list to build, no fuzzy scoring to get wrong, and no
//! way for the feature to confidently attach the wrong words to a song. That
//! is the difference between this and artwork, which needs a release picked
//! out of a search result before anything can be fetched at all.
//!
//! ## What leaves the machine
//!
//! Exactly what is in [`Query`]: the artist, title and album as they appear in
//! the file's own tags, and the track length in seconds. Nothing about the
//! user, the library, the file path, or what else is in it. One request per
//! track, once, and never again once the answer is cached.
//!
//! ## Failure
//!
//! Silently, and later. [`Client::fetch`] answers `None` for anything that is
//! not a clean hit, records the reason in the activity log, and leaves the
//! track without lyrics — exactly as it would have been on `main`. Nothing
//! here is on a path the user waits for, and nothing here can affect playback.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::activity::{Activity, Entry as LogEntry};
use crate::cache::{Cache, Entry as CacheEntry};
use crate::error::NetError;
use crate::http::Transport;
use crate::rate::Limiter;
use crate::source::{LRCLIB, Source};
use crate::{Outcome, cache};

/// Where cached lyrics are kept, under the application's cache directory.
pub const CACHE_NAMESPACE: &str = "lyrics";

// ---------------------------------------------------------------------------
// The question
// ---------------------------------------------------------------------------

/// What is being looked up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub artist: String,
    pub title: String,
    pub album: Option<String>,
    pub duration: Option<Duration>,
}

impl Query {
    pub fn new(artist: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            artist: artist.into(),
            title: title.into(),
            album: None,
            duration: None,
        }
    }

    pub fn with_album(mut self, album: impl Into<String>) -> Self {
        let album = album.into();
        if !album.trim().is_empty() {
            self.album = Some(album);
        }
        self
    }

    pub fn with_duration(mut self, duration: Duration) -> Self {
        self.duration = Some(duration);
        self
    }

    /// Whether there is enough here to be worth asking.
    ///
    /// A title recovered from a filename with no artist tag is not a question
    /// LRCLIB can answer, and asking anyway spends a request to be told so.
    pub fn is_answerable(&self) -> bool {
        !self.artist.trim().is_empty() && !self.title.trim().is_empty()
    }

    /// How this reads in the activity log.
    ///
    /// The user should recognise the track, so it is the same words they see
    /// in the player, not an opaque key.
    pub fn subject(&self) -> String {
        format!(
            "lyrics for \"{}\" by {}",
            self.title.trim(),
            self.artist.trim()
        )
    }

    /// The cache key. Album and duration are part of it because they are part
    /// of the question: the same title by the same artist on a single and on
    /// an album can be different recordings with different timings.
    pub fn cache_key(&self) -> String {
        let duration = self
            .duration
            .map_or_else(String::new, |d| d.as_secs().to_string());

        cache::key(&[
            self.artist.trim(),
            self.title.trim(),
            self.album.as_deref().unwrap_or("").trim(),
            &duration,
        ])
    }

    /// The request URL.
    ///
    /// `/api/get` is the exact-match endpoint: it matches on all four fields
    /// and answers 404 rather than guessing. That is the property this feature
    /// is built on, so the fuzzy `/api/search` endpoint is deliberately not
    /// used — a near-enough match is how the wrong words end up on a song.
    pub fn url(&self) -> String {
        let mut url = format!(
            "https://{}/api/get?artist_name={}&track_name={}",
            LRCLIB.host,
            encode(self.artist.trim()),
            encode(self.title.trim()),
        );

        if let Some(album) = &self.album {
            url.push_str("&album_name=");
            url.push_str(&encode(album.trim()));
        }

        if let Some(duration) = self.duration {
            url.push_str("&duration=");
            url.push_str(&duration.as_secs().to_string());
        }

        url
    }
}

/// Percent-encode a query parameter value.
///
/// Hand-rolled rather than pulled in: this escapes one kind of thing in one
/// place, and a dependency for it would be larger than the function. Anything
/// not unreserved per RFC 3986 is escaped, which is stricter than necessary
/// and cannot be wrong.
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());

    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }

    out
}

// ---------------------------------------------------------------------------
// The answer
// ---------------------------------------------------------------------------

/// What LRCLIB sends back.
///
/// Only the fields that are used. Serde ignores the rest, so the service
/// adding one does not break anything here.
#[derive(Debug, Clone, Deserialize)]
struct ApiResponse {
    #[serde(default)]
    #[serde(rename = "syncedLyrics")]
    synced_lyrics: Option<String>,
    #[serde(default)]
    #[serde(rename = "plainLyrics")]
    plain_lyrics: Option<String>,
    #[serde(default)]
    instrumental: bool,
}

/// Lyrics as fetched: the raw text, not yet parsed.
///
/// Parsing lives in `mp-core`, which already knows how to read `.lrc` — and
/// this crate deliberately does not depend on it, so the words travel back as
/// text and are parsed by the caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fetched {
    /// `.lrc` text, with a timestamp per line, when the service has it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synced: Option<String>,
    /// Plain text, with no timings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plain: Option<String>,
    /// The service says this track has no words at all.
    #[serde(default)]
    pub instrumental: bool,
}

impl Fetched {
    /// The best text available, and whether it carries timings.
    ///
    /// Synced is preferred because the interface can follow along with it; the
    /// plain version is the fallback and still worth showing.
    pub fn best(&self) -> Option<&str> {
        self.synced
            .as_deref()
            .or(self.plain.as_deref())
            .map(str::trim)
            .filter(|text| !text.is_empty())
    }

    pub fn is_synced(&self) -> bool {
        self.synced
            .as_deref()
            .is_some_and(|text| !text.trim().is_empty())
    }

    /// Whether this is worth showing. An instrumental track with no text is a
    /// real answer, and it is also nothing to display.
    pub fn is_empty(&self) -> bool {
        self.best().is_none()
    }
}

impl From<ApiResponse> for Fetched {
    fn from(response: ApiResponse) -> Self {
        Self {
            synced: response.synced_lyrics.filter(|t| !t.trim().is_empty()),
            plain: response.plain_lyrics.filter(|t| !t.trim().is_empty()),
            instrumental: response.instrumental,
        }
    }
}

// ---------------------------------------------------------------------------
// The client
// ---------------------------------------------------------------------------

/// Fetches lyrics, politely, and writes down that it did.
///
/// Owns the three things that must not be bypassed: the rate limiter, the
/// cache, and the activity log. There is no way to make a request through this
/// crate that skips any of them, which is the point.
pub struct Client {
    transport: Box<dyn Transport>,
    limiter: Limiter,
    cache: Cache,
    activity: Arc<Activity>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("cache", &self.cache.root())
            .finish_non_exhaustive()
    }
}

impl Client {
    /// A client fetching over the network.
    ///
    /// `cache_root` is the application's cache directory; the lyrics live in a
    /// [`CACHE_NAMESPACE`] subdirectory of it.
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
            limiter: Limiter::for_source(&LRCLIB),
            cache: Cache::new(cache_root.into().join(CACHE_NAMESPACE)),
            activity,
        }
    }

    pub fn source(&self) -> &'static Source {
        &LRCLIB
    }

    pub fn cache(&self) -> &Cache {
        &self.cache
    }

    /// Look up lyrics: cache first, then the network.
    ///
    /// **Blocks**, for up to the request timeout plus whatever the rate
    /// limiter owes. Background threads only.
    ///
    /// Answers `None` for a miss, a failure, or an unanswerable query. The
    /// activity log carries which of those it was.
    pub fn fetch(&self, query: &Query) -> Option<Fetched> {
        if !query.is_answerable() {
            // Not logged: nothing was going to be sent, and an entry per
            // untagged file would bury the requests that did happen.
            return None;
        }

        let key = query.cache_key();

        if let Some(entry) = self.cache.read::<Fetched>(&key) {
            self.log(query, Outcome::Cached, 0, None);
            return entry.found;
        }

        match self.request(query) {
            Ok((fetched, bytes)) => {
                self.limiter.note_success();
                self.store(&key, CacheEntry::found(fetched.clone()));
                self.log(query, Outcome::Ok, bytes, None);
                Some(fetched)
            }
            Err(NetError::NotFound) => {
                self.limiter.note_success();
                self.store(&key, CacheEntry::<Fetched>::missing());
                self.log(query, Outcome::NotFound, 0, None);
                None
            }
            Err(error) => {
                if error.is_failure() {
                    self.limiter.note_failure();
                }
                // Deliberately not cached. A failure says nothing about the
                // track, and recording it would turn one bad hour of
                // connectivity into a fortnight of missing lyrics.
                self.log(query, error.outcome(), 0, Some(error.to_string()));
                None
            }
        }
    }

    /// The request itself, once the cache has been missed.
    fn request(&self, query: &Query) -> Result<(Fetched, u64), NetError> {
        self.limiter.acquire();

        let response = self.transport.get(&query.url())?;

        let parsed: ApiResponse = serde_json::from_str(&response.body)
            .map_err(|err| NetError::Decode(err.to_string()))?;

        let fetched = Fetched::from(parsed);

        // A record that exists but carries no words is a miss as far as the
        // interface is concerned, and should be cached as one.
        if fetched.is_empty() && !fetched.instrumental {
            return Err(NetError::NotFound);
        }

        Ok((fetched, response.bytes))
    }

    fn store(&self, key: &str, entry: CacheEntry<Fetched>) {
        if let Err(error) = self.cache.write(key, &entry) {
            // Not fatal: the feature works without a cache, it is just ruder
            // to the service and slower for the user.
            tracing::warn!("could not cache lyrics: {error:#}");
        }
    }

    fn log(&self, query: &Query, outcome: Outcome, bytes: u64, detail: Option<String>) {
        let mut entry = LogEntry::new(&LRCLIB, outcome, query.subject()).with_bytes(bytes);

        if let Some(detail) = detail {
            entry = entry.with_detail(detail);
        }

        self.activity.record(entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Fetched as Body;
    use std::sync::Mutex;

    // -- a transport that answers from a script ----------------------------

    struct Fake {
        answers: Mutex<Vec<Result<Body, NetError>>>,
        urls: Mutex<Vec<String>>,
    }

    impl Fake {
        fn new(answers: Vec<Result<Body, NetError>>) -> Arc<Self> {
            Arc::new(Self {
                answers: Mutex::new(answers),
                urls: Mutex::new(Vec::new()),
            })
        }

        fn ok(body: &str) -> Result<Body, NetError> {
            Ok(Body {
                bytes: body.len() as u64,
                body: body.to_owned(),
            })
        }

        fn urls(&self) -> Vec<String> {
            self.urls.lock().unwrap().clone()
        }

        fn calls(&self) -> usize {
            self.urls.lock().unwrap().len()
        }
    }

    impl Transport for Fake {
        fn get(&self, url: &str) -> Result<Body, NetError> {
            self.urls.lock().unwrap().push(url.to_owned());

            let mut answers = self.answers.lock().unwrap();
            if answers.is_empty() {
                return Err(NetError::Transport("the script ran out".into()));
            }
            answers.remove(0)
        }
    }

    /// A transport that must never be reached.
    struct Forbidden;

    impl Transport for Forbidden {
        fn get(&self, url: &str) -> Result<Body, NetError> {
            panic!("a request was made when none should have been: {url}");
        }
    }

    const SYNCED: &str = r#"{"id":1,"trackName":"Creep","syncedLyrics":"[00:01.00]I","plainLyrics":"I","instrumental":false}"#;

    fn query() -> Query {
        Query::new("Radiohead", "Creep")
            .with_album("Pablo Honey")
            .with_duration(Duration::from_secs(239))
    }

    fn client(transport: Arc<Fake>, dir: &tempfile::TempDir) -> Client {
        Client::with_transport(
            Box::new(ScriptedRef(transport)),
            dir.path(),
            Arc::new(Activity::in_memory()),
        )
    }

    /// Lets a test keep a handle on the fake while the client owns it.
    struct ScriptedRef(Arc<Fake>);

    impl Transport for ScriptedRef {
        fn get(&self, url: &str) -> Result<Body, NetError> {
            self.0.get(url)
        }
    }

    // -- the query ---------------------------------------------------------

    #[test]
    fn the_url_carries_all_four_fields() {
        let url = query().url();

        assert!(url.starts_with("https://lrclib.net/api/get?"), "{url}");
        assert!(url.contains("artist_name=Radiohead"), "{url}");
        assert!(url.contains("track_name=Creep"), "{url}");
        assert!(url.contains("album_name=Pablo%20Honey"), "{url}");
        assert!(url.contains("duration=239"), "{url}");
    }

    /// A title with a space, an ampersand or a non-Latin character must not be
    /// able to alter the shape of the request.
    #[test]
    fn awkward_titles_are_encoded() {
        let url = Query::new("AC/DC", "Rock & Roll ain't noise").url();

        assert!(url.contains("artist_name=AC%2FDC"), "{url}");
        assert!(url.contains("Rock%20%26%20Roll"), "{url}");
        assert!(!url.contains(' '), "a raw space reached the url: {url}");

        let japanese = Query::new("宇多田ヒカル", "Automatic").url();
        assert!(japanese.is_ascii(), "the url should be escaped: {japanese}");
    }

    #[test]
    fn an_absent_album_or_duration_is_simply_left_out() {
        let url = Query::new("Radiohead", "Creep").url();

        assert!(!url.contains("album_name"), "{url}");
        assert!(!url.contains("duration"), "{url}");
    }

    /// Asking about a file whose title came from its filename and which has no
    /// artist tag spends a request to be told nothing.
    #[test]
    fn an_untagged_track_is_not_worth_asking_about() {
        assert!(query().is_answerable());
        assert!(!Query::new("", "Track 03").is_answerable());
        assert!(!Query::new("Radiohead", "   ").is_answerable());
    }

    /// The same song on a single and on an album can be different recordings
    /// with different timings, so they are different questions.
    #[test]
    fn the_cache_key_covers_the_whole_question() {
        let base = Query::new("Radiohead", "Creep");

        assert_ne!(
            base.cache_key(),
            base.clone().with_album("Pablo Honey").cache_key()
        );
        assert_ne!(
            base.cache_key(),
            base.clone()
                .with_duration(Duration::from_secs(239))
                .cache_key()
        );
        assert_eq!(query().cache_key(), query().cache_key(), "and it is stable");
    }

    #[test]
    fn the_log_subject_names_the_track_the_way_the_user_sees_it() {
        assert_eq!(query().subject(), "lyrics for \"Creep\" by Radiohead");
    }

    // -- the answer --------------------------------------------------------

    #[test]
    fn synced_lyrics_are_preferred_over_plain() {
        let fetched = Fetched {
            synced: Some("[00:01.00]I".to_owned()),
            plain: Some("I".to_owned()),
            instrumental: false,
        };

        assert_eq!(fetched.best(), Some("[00:01.00]I"));
        assert!(fetched.is_synced());
    }

    #[test]
    fn plain_lyrics_are_used_when_there_are_no_timings() {
        let fetched = Fetched {
            synced: None,
            plain: Some("I".to_owned()),
            instrumental: false,
        };

        assert_eq!(fetched.best(), Some("I"));
        assert!(!fetched.is_synced());
    }

    /// The service sends empty strings rather than nulls for absent lyrics,
    /// which would otherwise render as a blank pane that looks like a bug.
    #[test]
    fn empty_strings_count_as_nothing() {
        let response: ApiResponse =
            serde_json::from_str(r#"{"syncedLyrics":"","plainLyrics":"  ","instrumental":true}"#)
                .expect("parse");
        let fetched = Fetched::from(response);

        assert!(fetched.is_empty());
        assert!(fetched.instrumental);
        assert_eq!(fetched.best(), None);
    }

    /// The service adds fields over time; that must not break the client.
    #[test]
    fn unknown_fields_are_ignored() {
        let json = r#"{"syncedLyrics":"[00:01.00]I","somethingNew":{"a":1},"id":7}"#;
        let response: ApiResponse = serde_json::from_str(json).expect("parse");

        assert_eq!(Fetched::from(response).best(), Some("[00:01.00]I"));
    }

    // -- fetching ----------------------------------------------------------

    #[test]
    fn a_hit_comes_back_and_is_cached() {
        let dir = tempfile::tempdir().expect("temp dir");
        let fake = Fake::new(vec![Fake::ok(SYNCED)]);
        let client = client(Arc::clone(&fake), &dir);

        let first = client.fetch(&query()).expect("a hit");
        assert_eq!(first.best(), Some("[00:01.00]I"));

        let second = client.fetch(&query()).expect("served from the cache");
        assert_eq!(second.best(), Some("[00:01.00]I"));

        assert_eq!(fake.calls(), 1, "the second lookup should not have asked");
    }

    /// The whole reason the cache exists. Without this, every track the
    /// service has never heard of is a fresh request on every launch.
    #[test]
    fn a_miss_is_cached_so_it_is_not_asked_again() {
        let dir = tempfile::tempdir().expect("temp dir");
        let fake = Fake::new(vec![Err(NetError::NotFound)]);
        let client = client(Arc::clone(&fake), &dir);

        assert!(client.fetch(&query()).is_none());
        assert!(client.fetch(&query()).is_none());

        assert_eq!(fake.calls(), 1, "the miss should have been remembered");
    }

    /// One bad hour of connectivity must not become a fortnight of missing
    /// lyrics for every track tried during it.
    #[test]
    fn a_failure_is_not_cached() {
        let dir = tempfile::tempdir().expect("temp dir");
        let fake = Fake::new(vec![
            Err(NetError::Transport("no route".into())),
            Fake::ok(SYNCED),
        ]);
        let client = client(Arc::clone(&fake), &dir);

        assert!(client.fetch(&query()).is_none(), "the network was down");
        assert!(client.fetch(&query()).is_some(), "and then it came back");

        assert_eq!(fake.calls(), 2);
    }

    #[test]
    fn an_unanswerable_query_never_reaches_the_transport() {
        let dir = tempfile::tempdir().expect("temp dir");
        let client = Client::with_transport(
            Box::new(Forbidden),
            dir.path(),
            Arc::new(Activity::in_memory()),
        );

        assert!(client.fetch(&Query::new("", "Track 03")).is_none());
    }

    /// A record with no words in it is a miss, whatever the status code said.
    #[test]
    fn an_answer_with_no_words_counts_as_a_miss() {
        let dir = tempfile::tempdir().expect("temp dir");
        let fake = Fake::new(vec![Fake::ok(r#"{"id":1,"instrumental":false}"#)]);
        let client = client(Arc::clone(&fake), &dir);

        assert!(client.fetch(&query()).is_none());
    }

    /// An instrumental is a real answer, and worth remembering so the track is
    /// not asked about again every fortnight.
    #[test]
    fn an_instrumental_is_an_answer_rather_than_a_miss() {
        let dir = tempfile::tempdir().expect("temp dir");
        let fake = Fake::new(vec![Fake::ok(r#"{"id":1,"instrumental":true}"#)]);
        let client = client(Arc::clone(&fake), &dir);

        let answer = client.fetch(&query()).expect("instrumental is an answer");
        assert!(answer.instrumental);
        assert!(answer.is_empty(), "and there is nothing to show");

        client.fetch(&query());
        assert_eq!(fake.calls(), 1, "it should have been cached");
    }

    #[test]
    fn a_garbled_answer_is_a_failure_rather_than_a_panic() {
        let dir = tempfile::tempdir().expect("temp dir");
        let fake = Fake::new(vec![Fake::ok("<html>not json at all</html>")]);
        let client = client(Arc::clone(&fake), &dir);

        assert!(client.fetch(&query()).is_none());
    }

    // -- the log -----------------------------------------------------------

    #[test]
    fn every_lookup_is_written_down() {
        let dir = tempfile::tempdir().expect("temp dir");
        let activity = Arc::new(Activity::in_memory());
        let fake = Fake::new(vec![Fake::ok(SYNCED)]);

        let client = Client::with_transport(
            Box::new(ScriptedRef(Arc::clone(&fake))),
            dir.path(),
            Arc::clone(&activity),
        );

        client.fetch(&query());
        client.fetch(&query());

        let recent = activity.recent();
        assert_eq!(recent.len(), 2, "both lookups, not just the request");

        assert_eq!(recent[1].outcome, Outcome::Ok);
        assert_eq!(recent[1].source, "lrclib");
        assert_eq!(recent[1].host, "lrclib.net");
        assert!(recent[1].bytes > 0);
        assert_eq!(recent[1].subject, "lyrics for \"Creep\" by Radiohead");

        assert_eq!(recent[0].outcome, Outcome::Cached, "the second was cached");
        assert_eq!(
            activity.requests_made(),
            1,
            "only one of the two left the machine"
        );
    }

    #[test]
    fn a_failure_is_logged_with_its_reason() {
        let dir = tempfile::tempdir().expect("temp dir");
        let activity = Arc::new(Activity::in_memory());
        let fake = Fake::new(vec![Err(NetError::Status { status: 503 })]);

        let client = Client::with_transport(
            Box::new(ScriptedRef(fake)),
            dir.path(),
            Arc::clone(&activity),
        );

        client.fetch(&query());

        let recent = activity.recent();
        assert_eq!(recent[0].outcome, Outcome::Failed);
        assert_eq!(
            recent[0].detail.as_deref(),
            Some("the service answered 503")
        );
    }

    /// A track with no artist tag is not a request and should not look like
    /// one; a log full of them would bury the entries that matter.
    #[test]
    fn an_unanswerable_query_is_not_logged() {
        let dir = tempfile::tempdir().expect("temp dir");
        let activity = Arc::new(Activity::in_memory());

        let client = Client::with_transport(Box::new(Forbidden), dir.path(), Arc::clone(&activity));

        client.fetch(&Query::new("", ""));

        assert!(activity.is_empty());
    }

    /// Only `/api/get` is ever called. `/api/search` returns near-enough
    /// matches, which is how the wrong words end up on a song.
    #[test]
    fn only_the_exact_match_endpoint_is_used() {
        let dir = tempfile::tempdir().expect("temp dir");
        let fake = Fake::new(vec![Fake::ok(SYNCED)]);
        let client = client(Arc::clone(&fake), &dir);

        client.fetch(&query());

        for url in fake.urls() {
            assert!(url.contains("/api/get?"), "{url}");
            assert!(!url.contains("/api/search"), "{url}");
        }
    }

    /// Every request must go to the host the source declares, or the activity
    /// log is describing somewhere the traffic did not go.
    #[test]
    fn every_request_goes_to_the_declared_host() {
        let dir = tempfile::tempdir().expect("temp dir");
        let fake = Fake::new(vec![Fake::ok(SYNCED)]);
        let client = client(Arc::clone(&fake), &dir);

        client.fetch(&query());

        for url in fake.urls() {
            assert!(
                url.starts_with(&format!("https://{}/", LRCLIB.host)),
                "{url} is not the host the source declares"
            );
        }
    }
}
