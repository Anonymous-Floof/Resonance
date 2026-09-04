//! Fetching cover art, for albums that have none.
//!
//! Harder than lyrics, and in a way worth being explicit about.
//!
//! LRCLIB answers one exact question with one right answer, so the lyrics
//! fetcher cannot attach the wrong words to a song. Cover art has no such
//! endpoint. Art is filed against a *release* — one particular pressing, with
//! a MusicBrainz identifier — and nothing in a music file says which release
//! it came from. The only way to find one is to search, and search returns
//! scored guesses.
//!
//! A wrong cover is also far more visible than a missing one. It sits at the
//! top of the now-playing view, it feeds the adaptive accent colour, and the
//! user will see it every time they play the album.
//!
//! ## So the strictness lives here rather than in the service
//!
//! The search is used only to *find candidates*. A candidate is accepted only
//! when its release title and artist are equal to what was asked for, compared
//! after case and spacing are normalised. The score MusicBrainz assigns is
//! deliberately ignored: a 100-point match on a different album is still a
//! different album, and the comparison is a fact where the score is an opinion.
//!
//! The result is the same property the lyrics fetcher gets for free — right,
//! or absent. An album whose tags do not name a real release simply keeps no
//! cover, exactly as it would have on `main`.
//!
//! ## Two hosts, both declared
//!
//! [`MUSICBRAINZ`] identifies the release; [`COVER_ART_ARCHIVE`] serves the
//! image. The second answers with a redirect and the file itself arrives from
//! the Internet Archive, which is why [`Source::redirected_to`] exists and why
//! the log records the host that actually answered rather than the one that
//! was addressed.
//!
//! ## What leaves the machine
//!
//! An album title and an artist name, both as they appear in the file's own
//! tags, and then a release identifier that came back from the first request.
//! Nothing about the user, the library, the file paths, or the rest of the
//! collection. One album is asked about once, and the answer — including "there
//! is no cover" — is cached.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::activity::{Activity, Entry as LogEntry};
use crate::cache::{Cache, Entry as CacheEntry};
use crate::http::Transport;
use crate::rate::Limiter;
use crate::source::{COVER_ART_ARCHIVE, MUSICBRAINZ, Source};
use crate::{Outcome, cache};

/// Where cached artwork answers are kept, under the app's cache directory.
pub const CACHE_NAMESPACE: &str = "artwork";

/// How many search results are considered before giving up.
///
/// The correct release, when there is one, is almost always first. A handful
/// covers the case where a compilation or a single scores higher than the
/// album itself; going deeper only widens the net for something that is
/// rejected by an exact comparison anyway.
const CANDIDATES: usize = 5;

/// The edge length requested from the archive.
///
/// The interface stores covers at 64, 256 and 800 pixels, so 500 is smaller
/// than the largest size it will draw. That is deliberate: the full-size
/// originals are frequently several thousand pixels and many megabytes, and
/// the difference is invisible behind a now-playing view while the download
/// is not.
const COVER_EDGE: u32 = 500;

// ---------------------------------------------------------------------------
// The question
// ---------------------------------------------------------------------------

/// The album a cover is wanted for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub artist: String,
    pub album: String,
}

impl Query {
    pub fn new(artist: impl Into<String>, album: impl Into<String>) -> Self {
        Self {
            artist: artist.into(),
            album: album.into(),
        }
    }

    /// Whether there is enough here to be worth asking.
    ///
    /// Both halves are required. An album title with no artist matches dozens
    /// of unrelated releases called "Greatest Hits", and the exact comparison
    /// below would have nothing to reject them with.
    pub fn is_answerable(&self) -> bool {
        !self.artist.trim().is_empty() && !self.album.trim().is_empty()
    }

    pub fn cache_key(&self) -> String {
        cache::key(&[self.artist.trim(), self.album.trim()])
    }

    /// How this reads in the activity log.
    pub fn subject(&self) -> String {
        format!(
            "cover for \"{}\" by {}",
            self.album.trim(),
            self.artist.trim()
        )
    }

    /// The release search.
    ///
    /// Fielded rather than free text, so the artist cannot be matched against
    /// a release title or the other way round. `limit` is small on purpose;
    /// see [`CANDIDATES`].
    pub fn search_url(&self) -> String {
        let lucene = format!(
            "release:{} AND artist:{}",
            quote(self.album.trim()),
            quote(self.artist.trim())
        );

        format!(
            "https://{}/ws/2/release/?query={}&fmt=json&limit={CANDIDATES}",
            MUSICBRAINZ.host,
            encode(&lucene)
        )
    }
}

/// A cover that was found, and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cover {
    /// The encoded image, as served. Handed straight to the art cache, which
    /// decodes it once and stores it pre-resized.
    pub bytes: Vec<u8>,
    /// The release it belongs to, so the log and the interface can say.
    pub release_id: String,
    /// The host that actually served the image.
    pub served_by: Option<String>,
}

/// What a previous lookup concluded, remembered on disk.
///
/// A found entry names the release. A missing one covers both "no release
/// matched" and "the release has no cover", because the interface does the
/// same thing in either case and a user asking why an album has no art is not
/// helped by the distinction — the activity log carries it if they want it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Resolved {
    pub release_id: String,
}

// ---------------------------------------------------------------------------
// The response
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    releases: Vec<Candidate>,
}

#[derive(Debug, Deserialize)]
struct Candidate {
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default, rename = "artist-credit")]
    artist_credit: Vec<ArtistCredit>,
}

#[derive(Debug, Deserialize)]
struct ArtistCredit {
    #[serde(default)]
    name: String,
}

impl Candidate {
    /// The full credited artist, joined as MusicBrainz presents it.
    ///
    /// A collaboration is credited as several parts which read as one name, so
    /// they are joined rather than compared one at a time — otherwise a
    /// release by two artists would never match a tag naming both.
    fn artist(&self) -> String {
        self.artist_credit
            .iter()
            .map(|credit| credit.name.trim())
            .filter(|name| !name.is_empty())
            .collect::<Vec<_>>()
            .join(" & ")
    }
}

// ---------------------------------------------------------------------------
// The client
// ---------------------------------------------------------------------------

pub struct Client {
    transport: Box<dyn Transport>,
    /// One limiter per service. MusicBrainz enforces one request a second and
    /// the archive does not, so sharing one would slow the archive to the pace
    /// of the database for no reason.
    musicbrainz: Limiter,
    archive: Limiter,
    cache: Cache,
    activity: Arc<Activity>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("artwork::Client").finish_non_exhaustive()
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
            musicbrainz: Limiter::for_source(&MUSICBRAINZ),
            archive: Limiter::for_source(&COVER_ART_ARCHIVE),
            cache: Cache::new(cache_root.into().join(CACHE_NAMESPACE)),
            activity,
        }
    }

    /// The services this can reach, for the settings screen to describe.
    pub fn sources(&self) -> [&'static Source; 2] {
        [&MUSICBRAINZ, &COVER_ART_ARCHIVE]
    }

    pub fn cache(&self) -> &Cache {
        &self.cache
    }

    /// Find a cover for an album.
    ///
    /// **Blocks**, for up to two request timeouts plus whatever the limiters
    /// owe. Background threads only.
    ///
    /// `None` for a miss, a failure, or an unanswerable query. The activity
    /// log carries which it was, and for which of the two services.
    pub fn fetch(&self, query: &Query) -> Option<Cover> {
        if !query.is_answerable() {
            // Not logged. Nothing was going to be sent, and an entry for every
            // untitled album would bury the requests that did happen.
            return None;
        }

        let key = query.cache_key();

        // A remembered "no cover" is the valuable half of this cache: it stops
        // an album with no match being searched for again on every launch.
        if let Some(entry) = self.cache.read::<Resolved>(&key) {
            self.log(&MUSICBRAINZ, query, Outcome::Cached, 0, None, None);
            let resolved = entry.found?;
            return self.image(query, &resolved.release_id);
        }

        let release_id = self.identify(query)?;

        match self.image(query, &release_id) {
            Some(cover) => {
                self.store(&key, CacheEntry::found(Resolved { release_id }));
                Some(cover)
            }
            None => {
                // The release exists but carries no cover. Remembered as a
                // miss so the pair of requests is not repeated every launch.
                self.store(&key, CacheEntry::<Resolved>::missing());
                None
            }
        }
    }

    /// Search MusicBrainz, and accept a candidate only if it actually matches.
    fn identify(&self, query: &Query) -> Option<String> {
        self.musicbrainz.acquire();

        let response = match self.transport.get(&query.search_url()) {
            Ok(response) => response,
            Err(error) => {
                if error.is_failure() {
                    self.musicbrainz.note_failure();
                }
                self.log(
                    &MUSICBRAINZ,
                    query,
                    error.outcome(),
                    0,
                    Some(error.to_string()),
                    None,
                );
                return None;
            }
        };

        self.musicbrainz.note_success();

        let parsed: SearchResponse = match serde_json::from_str(&response.body) {
            Ok(parsed) => parsed,
            Err(error) => {
                self.log(
                    &MUSICBRAINZ,
                    query,
                    Outcome::Failed,
                    response.bytes,
                    Some(error.to_string()),
                    None,
                );
                return None;
            }
        };

        let accepted = parsed
            .releases
            .iter()
            .take(CANDIDATES)
            .find(|candidate| matches(candidate, query));

        match accepted {
            Some(candidate) => {
                self.log(&MUSICBRAINZ, query, Outcome::Ok, response.bytes, None, None);
                Some(candidate.id.clone())
            }
            None => {
                // Deliberately says how many were rejected. "No results" and
                // "five results, none of them this album" are different
                // stories, and only one of them suggests fixing a tag.
                let detail = match parsed.releases.len() {
                    0 => "no releases matched".to_owned(),
                    1 => "1 release returned, but it is a different album".to_owned(),
                    n => format!("{n} releases returned, none matching exactly"),
                };

                self.log(
                    &MUSICBRAINZ,
                    query,
                    Outcome::NotFound,
                    response.bytes,
                    Some(detail),
                    None,
                );
                None
            }
        }
    }

    /// Fetch the front cover for a known release.
    fn image(&self, query: &Query, release_id: &str) -> Option<Cover> {
        self.archive.acquire();

        let url = format!(
            "https://{}/release/{release_id}/front-{COVER_EDGE}",
            COVER_ART_ARCHIVE.host
        );

        match self.transport.get_bytes(&url) {
            Ok(fetched) => {
                self.archive.note_success();

                if fetched.body.is_empty() {
                    self.log(
                        &COVER_ART_ARCHIVE,
                        query,
                        Outcome::NotFound,
                        0,
                        Some("the response carried no image".to_owned()),
                        fetched.served_by.clone(),
                    );
                    return None;
                }

                self.log(
                    &COVER_ART_ARCHIVE,
                    query,
                    Outcome::Ok,
                    fetched.bytes,
                    None,
                    fetched.served_by.clone(),
                );

                Some(Cover {
                    bytes: fetched.body,
                    release_id: release_id.to_owned(),
                    served_by: fetched.served_by,
                })
            }
            Err(error) => {
                if error.is_failure() {
                    self.archive.note_failure();
                } else {
                    // A 404 here is the ordinary answer for a release nobody
                    // has uploaded a cover for. The service worked.
                    self.archive.note_success();
                }

                self.log(
                    &COVER_ART_ARCHIVE,
                    query,
                    error.outcome(),
                    0,
                    Some(error.to_string()),
                    None,
                );
                None
            }
        }
    }

    fn store(&self, key: &str, entry: CacheEntry<Resolved>) {
        if let Err(error) = self.cache.write(key, &entry) {
            tracing::warn!("could not cache an artwork lookup: {error:#}");
        }
    }

    fn log(
        &self,
        source: &Source,
        query: &Query,
        outcome: Outcome,
        bytes: u64,
        detail: Option<String>,
        served_by: Option<String>,
    ) {
        let mut entry = LogEntry::new(source, outcome, query.subject()).with_bytes(bytes);

        if let Some(detail) = detail {
            entry = entry.with_detail(detail);
        }

        if let Some(host) = served_by {
            entry = entry.with_host(host);
        }

        self.activity.record(entry);
    }
}

// ---------------------------------------------------------------------------
// Matching
// ---------------------------------------------------------------------------

/// Whether a search result is actually the album that was asked for.
///
/// The whole safety property of this module. MusicBrainz's own score is not
/// consulted: it measures how well a record matched a query, which is not the
/// same question as whether it is this album, and a confident wrong answer is
/// the failure worth avoiding.
fn matches(candidate: &Candidate, query: &Query) -> bool {
    normalise(&candidate.title) == normalise(&query.album)
        && normalise(&candidate.artist()) == normalise(&query.artist)
}

/// Fold away the differences that are never meaningful.
///
/// Case and spacing only. Punctuation is left alone deliberately — dropping it
/// would make `Vol. 1` and `Vol 1` match, and also `Live` and `Live!`, and the
/// second of those is a different release.
fn normalise(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Wrap a term in quotes for the Lucene query, escaping what would break it.
fn quote(text: &str) -> String {
    let escaped: String = text
        .chars()
        .map(|c| match c {
            // A stray quote or backslash would end the term early and turn the
            // rest of an album title into query syntax.
            '"' | '\\' => ' ',
            other => other,
        })
        .collect();

    format!("\"{}\"", escaped.trim())
}

/// Percent-encode everything that is not unreserved.
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());

    for byte in text.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::NetError;
    use crate::http::{Fetched as Body, FetchedBytes};
    use std::sync::Mutex;

    // -- a scripted transport ----------------------------------------------

    /// Answers text and binary requests from two scripts, and records the URLs.
    struct Fake {
        text: Mutex<Vec<Result<Body, NetError>>>,
        binary: Mutex<Vec<Result<FetchedBytes, NetError>>>,
        urls: Mutex<Vec<String>>,
    }

    impl Fake {
        fn new(
            text: Vec<Result<Body, NetError>>,
            binary: Vec<Result<FetchedBytes, NetError>>,
        ) -> Arc<Self> {
            Arc::new(Self {
                text: Mutex::new(text),
                binary: Mutex::new(binary),
                urls: Mutex::new(Vec::new()),
            })
        }

        fn ok(body: &str) -> Result<Body, NetError> {
            Ok(Body {
                bytes: body.len() as u64,
                body: body.to_owned(),
            })
        }

        fn image(bytes: &[u8], served_by: Option<&str>) -> Result<FetchedBytes, NetError> {
            Ok(FetchedBytes {
                bytes: bytes.len() as u64,
                body: bytes.to_vec(),
                served_by: served_by.map(str::to_owned),
            })
        }

        fn urls(&self) -> Vec<String> {
            self.urls.lock().unwrap().clone()
        }

        fn calls(&self) -> usize {
            self.urls.lock().unwrap().len()
        }
    }

    struct Scripted(Arc<Fake>);

    impl Transport for Scripted {
        fn get(&self, url: &str) -> Result<Body, NetError> {
            self.0.urls.lock().unwrap().push(url.to_owned());
            let mut script = self.0.text.lock().unwrap();
            if script.is_empty() {
                return Err(NetError::Transport("the text script ran out".into()));
            }
            script.remove(0)
        }

        fn get_bytes(&self, url: &str) -> Result<FetchedBytes, NetError> {
            self.0.urls.lock().unwrap().push(url.to_owned());
            let mut script = self.0.binary.lock().unwrap();
            if script.is_empty() {
                return Err(NetError::Transport("the image script ran out".into()));
            }
            script.remove(0)
        }
    }

    fn client(fake: Arc<Fake>, dir: &tempfile::TempDir) -> Client {
        Client::with_transport(
            Box::new(Scripted(fake)),
            dir.path(),
            Arc::new(Activity::in_memory()),
        )
    }

    fn logged_client(fake: Arc<Fake>, dir: &tempfile::TempDir) -> (Client, Arc<Activity>) {
        let activity = Arc::new(Activity::in_memory());
        let client =
            Client::with_transport(Box::new(Scripted(fake)), dir.path(), Arc::clone(&activity));
        (client, activity)
    }

    fn query() -> Query {
        Query::new("Radiohead", "Kid A")
    }

    const MATCH: &str =
        r#"{"releases":[{"id":"abc-123","title":"Kid A","artist-credit":[{"name":"Radiohead"}]}]}"#;
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n-pretend-this-is-an-image";

    // -- the question ------------------------------------------------------

    #[test]
    fn the_search_is_fielded_so_the_halves_cannot_cross_over() {
        let url = query().search_url();

        assert!(
            url.starts_with("https://musicbrainz.org/ws/2/release/?query="),
            "{url}"
        );
        assert!(url.contains("fmt=json"), "{url}");
        assert!(url.contains("release%3A"), "{url}");
        assert!(url.contains("artist%3A"), "{url}");
        assert!(url.contains("Kid%20A"), "{url}");
        assert!(url.contains("Radiohead"), "{url}");
    }

    /// A quote in an album title would close the term and leave the remainder
    /// being read as query syntax.
    #[test]
    fn a_title_cannot_break_out_of_the_query() {
        let url = Query::new("Someone", "A \"Quoted\" Title").search_url();

        assert!(
            !url.contains("%22Quoted%22"),
            "the inner quotes survived: {url}"
        );
    }

    #[test]
    fn both_halves_are_needed_to_ask() {
        assert!(query().is_answerable());
        assert!(!Query::new("", "Kid A").is_answerable());
        assert!(!Query::new("Radiohead", "").is_answerable());
        assert!(!Query::new("  ", "  ").is_answerable());
    }

    #[test]
    fn the_subject_names_the_album_a_user_would_recognise() {
        assert_eq!(query().subject(), "cover for \"Kid A\" by Radiohead");
    }

    // -- matching ----------------------------------------------------------

    #[test]
    fn a_candidate_matching_both_fields_is_accepted() {
        let dir = tempfile::tempdir().expect("temp dir");
        let fake = Fake::new(vec![Fake::ok(MATCH)], vec![Fake::image(PNG, None)]);
        let client = client(Arc::clone(&fake), &dir);

        let cover = client.fetch(&query()).expect("a cover");
        assert_eq!(cover.release_id, "abc-123");
        assert_eq!(cover.bytes, PNG);
    }

    /// The property the whole module exists for: a confident wrong answer is
    /// worse than no answer, so a near miss is still a miss.
    #[test]
    fn a_release_that_is_not_this_album_is_rejected() {
        let dir = tempfile::tempdir().expect("temp dir");
        let wrong = r#"{"releases":[{"id":"x","title":"Amnesiac","artist-credit":[{"name":"Radiohead"}]}]}"#;
        let fake = Fake::new(vec![Fake::ok(wrong)], vec![]);
        let client = client(Arc::clone(&fake), &dir);

        assert!(client.fetch(&query()).is_none());
        assert_eq!(fake.calls(), 1, "no image should have been requested");
    }

    #[test]
    fn a_release_by_a_different_artist_is_rejected() {
        let dir = tempfile::tempdir().expect("temp dir");
        let wrong = r#"{"releases":[{"id":"x","title":"Kid A","artist-credit":[{"name":"Someone Else"}]}]}"#;
        let fake = Fake::new(vec![Fake::ok(wrong)], vec![]);
        let client = client(Arc::clone(&fake), &dir);

        assert!(client.fetch(&query()).is_none());
        assert_eq!(fake.calls(), 1);
    }

    /// The right release is not always first, and the score is not consulted.
    #[test]
    fn a_match_further_down_the_list_is_still_found() {
        let dir = tempfile::tempdir().expect("temp dir");
        let body = r#"{"releases":[
            {"id":"1","title":"Kid A Mnesia","artist-credit":[{"name":"Radiohead"}]},
            {"id":"2","title":"Kid A","artist-credit":[{"name":"Nobody"}]},
            {"id":"3","title":"Kid A","artist-credit":[{"name":"Radiohead"}]}
        ]}"#;
        let fake = Fake::new(vec![Fake::ok(body)], vec![Fake::image(PNG, None)]);
        let client = client(Arc::clone(&fake), &dir);

        assert_eq!(client.fetch(&query()).expect("a cover").release_id, "3");
    }

    #[test]
    fn case_and_spacing_do_not_prevent_a_match() {
        let dir = tempfile::tempdir().expect("temp dir");
        let body =
            r#"{"releases":[{"id":"1","title":"kid   a","artist-credit":[{"name":"RADIOHEAD"}]}]}"#;
        let fake = Fake::new(vec![Fake::ok(body)], vec![Fake::image(PNG, None)]);
        let client = client(Arc::clone(&fake), &dir);

        assert!(client.fetch(&query()).is_some());
    }

    /// A collaboration is credited in parts that read as one name.
    #[test]
    fn a_joint_credit_is_compared_as_one_name() {
        let dir = tempfile::tempdir().expect("temp dir");
        let body = r#"{"releases":[{"id":"1","title":"Watch the Throne","artist-credit":[{"name":"Jay-Z"},{"name":"Kanye West"}]}]}"#;
        let fake = Fake::new(vec![Fake::ok(body)], vec![Fake::image(PNG, None)]);
        let client = client(Arc::clone(&fake), &dir);

        let query = Query::new("Jay-Z & Kanye West", "Watch the Throne");
        assert!(client.fetch(&query).is_some());
    }

    /// Punctuation is left alone: these are genuinely different releases.
    #[test]
    fn punctuation_is_not_folded_away() {
        assert_ne!(normalise("Live"), normalise("Live!"));
        assert_ne!(normalise("Vol. 1"), normalise("Vol 1"));
    }

    #[test]
    fn an_empty_result_set_is_a_clean_miss() {
        let dir = tempfile::tempdir().expect("temp dir");
        let fake = Fake::new(vec![Fake::ok(r#"{"releases":[]}"#)], vec![]);
        let client = client(Arc::clone(&fake), &dir);

        assert!(client.fetch(&query()).is_none());
    }

    #[test]
    fn a_query_with_nothing_to_ask_reaches_no_service() {
        let dir = tempfile::tempdir().expect("temp dir");
        let fake = Fake::new(vec![], vec![]);
        let client = client(Arc::clone(&fake), &dir);

        assert!(client.fetch(&Query::new("", "")).is_none());
        assert_eq!(fake.calls(), 0);
    }

    // -- the image ---------------------------------------------------------

    #[test]
    fn the_image_is_asked_for_by_release_id() {
        let dir = tempfile::tempdir().expect("temp dir");
        let fake = Fake::new(vec![Fake::ok(MATCH)], vec![Fake::image(PNG, None)]);
        let client = client(Arc::clone(&fake), &dir);

        client.fetch(&query());

        let urls = fake.urls();
        assert_eq!(urls.len(), 2);
        assert_eq!(
            urls[1],
            "https://coverartarchive.org/release/abc-123/front-500"
        );
    }

    /// A release with no uploaded cover is the ordinary case, not a failure.
    #[test]
    fn a_release_without_a_cover_is_a_miss_not_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let fake = Fake::new(vec![Fake::ok(MATCH)], vec![Err(NetError::NotFound)]);
        let (client, activity) = logged_client(Arc::clone(&fake), &dir);

        assert!(client.fetch(&query()).is_none());

        let recent = activity.recent();
        assert_eq!(recent[0].outcome, Outcome::NotFound);
        assert_eq!(recent[0].source, "coverartarchive");
    }

    // -- caching -----------------------------------------------------------

    /// The valuable half: an album with no cover must not be searched for
    /// again on every launch.
    #[test]
    fn a_miss_is_remembered() {
        let dir = tempfile::tempdir().expect("temp dir");
        let fake = Fake::new(
            vec![Fake::ok(MATCH), Fake::ok(MATCH)],
            vec![Err(NetError::NotFound)],
        );
        let client = client(Arc::clone(&fake), &dir);

        assert!(client.fetch(&query()).is_none());
        assert_eq!(fake.calls(), 2);

        assert!(client.fetch(&query()).is_none());
        assert_eq!(fake.calls(), 2, "the second lookup should have been cached");
    }

    /// A cached hit still fetches the image, because the bytes are not kept
    /// here — they go to the library's own content-addressed art store.
    #[test]
    fn a_remembered_release_skips_the_search_but_not_the_image() {
        let dir = tempfile::tempdir().expect("temp dir");
        let fake = Fake::new(
            vec![Fake::ok(MATCH)],
            vec![Fake::image(PNG, None), Fake::image(PNG, None)],
        );
        let client = client(Arc::clone(&fake), &dir);

        assert!(client.fetch(&query()).is_some());
        assert_eq!(fake.calls(), 2);

        assert!(client.fetch(&query()).is_some());
        assert_eq!(
            fake.calls(),
            3,
            "only the image should have been re-fetched"
        );
        assert!(fake.urls()[2].contains("coverartarchive.org"));
    }

    /// A failure says nothing about the album and must not be cached, or one
    /// bad hour of connectivity becomes a fortnight of missing covers.
    #[test]
    fn a_failure_is_not_remembered() {
        let dir = tempfile::tempdir().expect("temp dir");
        let fake = Fake::new(
            vec![Err(NetError::Transport("no route".into())), Fake::ok(MATCH)],
            vec![Fake::image(PNG, None)],
        );
        let client = client(Arc::clone(&fake), &dir);

        assert!(client.fetch(&query()).is_none());
        assert!(client.fetch(&query()).is_some(), "and then it came back");
    }

    // -- the log -----------------------------------------------------------

    /// The point of declaring a redirect: the log must name the machine that
    /// answered, not the one that was addressed.
    #[test]
    fn the_log_names_the_host_that_actually_served_the_image() {
        let dir = tempfile::tempdir().expect("temp dir");
        let fake = Fake::new(
            vec![Fake::ok(MATCH)],
            vec![Fake::image(PNG, Some("ia800207.us.archive.org"))],
        );
        let (client, activity) = logged_client(Arc::clone(&fake), &dir);

        client.fetch(&query());

        let recent = activity.recent();
        assert_eq!(recent.len(), 2);

        assert_eq!(recent[0].source, "coverartarchive");
        assert_eq!(
            recent[0].host, "ia800207.us.archive.org",
            "the log should record where the bytes came from"
        );

        assert_eq!(recent[1].source, "musicbrainz");
        assert_eq!(recent[1].host, "musicbrainz.org");
    }

    /// "Nothing came back" and "five came back, none of them this album" are
    /// different problems, and only the second is a tagging problem.
    #[test]
    fn a_rejected_search_says_how_many_were_considered() {
        let dir = tempfile::tempdir().expect("temp dir");
        let wrong = r#"{"releases":[{"id":"x","title":"Amnesiac","artist-credit":[{"name":"Radiohead"}]}]}"#;
        let fake = Fake::new(vec![Fake::ok(wrong)], vec![]);
        let (client, activity) = logged_client(Arc::clone(&fake), &dir);

        client.fetch(&query());

        let detail = activity.recent()[0].detail.clone().expect("a reason");
        assert!(detail.contains("a different album"), "{detail}");
    }

    /// The plural form, and the reason it is worth distinguishing: a user
    /// reading this is trying to work out whether their tags are wrong.
    #[test]
    fn several_rejected_candidates_are_counted() {
        let dir = tempfile::tempdir().expect("temp dir");
        let wrong = r#"{"releases":[
            {"id":"a","title":"Amnesiac","artist-credit":[{"name":"Radiohead"}]},
            {"id":"b","title":"Kid A Mnesia","artist-credit":[{"name":"Radiohead"}]}
        ]}"#;
        let fake = Fake::new(vec![Fake::ok(wrong)], vec![]);
        let (client, activity) = logged_client(Arc::clone(&fake), &dir);

        client.fetch(&query());

        let detail = activity.recent()[0].detail.clone().expect("a reason");
        assert!(detail.contains("2 releases returned"), "{detail}");
    }

    #[test]
    fn malformed_json_is_a_failure_rather_than_a_panic() {
        let dir = tempfile::tempdir().expect("temp dir");
        let fake = Fake::new(vec![Fake::ok("not json at all")], vec![]);
        let (client, activity) = logged_client(Arc::clone(&fake), &dir);

        assert!(client.fetch(&query()).is_none());
        assert_eq!(activity.recent()[0].outcome, Outcome::Failed);
    }

    #[test]
    fn both_services_are_declared() {
        let dir = tempfile::tempdir().expect("temp dir");
        let client = client(Fake::new(vec![], vec![]), &dir);

        let ids: Vec<_> = client.sources().iter().map(|s| s.id).collect();
        assert_eq!(ids, vec!["musicbrainz", "coverartarchive"]);
    }
}
