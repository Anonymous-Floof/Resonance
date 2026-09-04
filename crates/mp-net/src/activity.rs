//! The record of every outbound request, written where the user can read it.
//!
//! This is the feature that makes "open about it" a fact rather than a claim.
//! A networked build that is vague about what it sends is worse than an
//! offline one, and the only way to be un-vague is to write it all down
//! somewhere the user can check without taking the application's word for it.
//!
//! So the log is a plain text file, not a table in the index. Tab-separated,
//! one line per request, with a header explaining the columns. It can be read
//! in Notepad, grepped, pasted into a spreadsheet, or handed to someone else —
//! none of which is true of a row in SQLite, and all of which is the point.
//! The file is the record; the in-memory copy is only there so a view inside
//! the app can be drawn without re-reading the disk every frame.
//!
//! ## What counts as an entry
//!
//! Everything that *was going to be* a request, including the ones that never
//! left the machine. A cache hit is logged as [`Outcome::Cached`] and a
//! request held back by the rate limiter as [`Outcome::Skipped`]. Logging only
//! the successful requests would produce a tidier file and a less honest one:
//! "Resonance wanted to look this up, and here is why it did not" is exactly
//! the kind of thing a user is entitled to see.
//!
//! [`Outcome::made_a_request`] draws the line between the two, so a summary
//! can say how many requests actually happened without the reader having to
//! know which outcomes mean what.
//!
//! ## What it costs
//!
//! Nothing that matters. Entries arrive at most as fast as the rate limiter
//! allows — around one a second — so each is written straight to the file
//! rather than buffered. A buffer would lose the last few entries in a crash,
//! and an audit log that quietly drops its most interesting records is not
//! worth having.
//!
//! [`Activity::record`] never fails and never panics. A log that could take
//! down an enrichment worker would be a bug traded for a bug; if the file
//! cannot be written it is reported once through `tracing` and the entry is
//! still kept in memory.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};

use crate::source::Source;
use crate::timestamp;

/// The log's file name. The directory is the caller's decision.
///
/// It belongs in the data directory rather than the cache. A cache is
/// something an application may clear whenever it likes, and a record the app
/// can silently discard is a weaker promise than one it cannot.
pub const LOG_FILE_NAME: &str = "network-activity.log";

/// How many entries are kept in memory for the interface to draw.
pub const RECENT_CAPACITY: usize = 200;

/// How large the file grows before it is rotated.
///
/// One megabyte is roughly ten thousand entries — months of ordinary use, and
/// still small enough to open in any editor without it complaining.
pub const MAX_LOG_BYTES: u64 = 1024 * 1024;

/// The longest a subject or detail is allowed to be, in characters.
///
/// A tag can contain an entire tracklist. One pathological file should not be
/// able to fill the log by itself.
const MAX_TEXT_CHARS: usize = 200;

// ---------------------------------------------------------------------------
// Entries
// ---------------------------------------------------------------------------

/// How a request turned out.
///
/// Deliberately small and `Copy`: it is one column in a text file and one
/// filter in a view, not a place to carry detail. Detail goes in
/// [`Entry::detail`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The request was made and answered.
    Ok,
    /// The request was made, and the source had nothing. Not a failure: for an
    /// obscure track this is the ordinary answer, and the service worked.
    NotFound,
    /// No request was made — the answer was already cached.
    Cached,
    /// No request was made — held back by rate limiting, backoff, or the
    /// feature being switched off.
    Skipped,
    /// The request was made and went wrong: unreachable, refused, timed out.
    Failed,
}

impl Outcome {
    /// The word written into the file.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::NotFound => "not-found",
            Self::Cached => "cached",
            Self::Skipped => "skipped",
            Self::Failed => "failed",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "ok" => Some(Self::Ok),
            "not-found" => Some(Self::NotFound),
            "cached" => Some(Self::Cached),
            "skipped" => Some(Self::Skipped),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }

    /// Whether anything actually left the machine.
    ///
    /// The distinction the log exists to draw, and the one a summary line
    /// needs: a hundred entries of which two were requests is a very different
    /// story from a hundred requests.
    pub fn made_a_request(self) -> bool {
        match self {
            Self::Ok | Self::NotFound | Self::Failed => true,
            Self::Cached | Self::Skipped => false,
        }
    }
}

/// One line of the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Unix seconds. Formatted as UTC when written; see [`crate::timestamp`].
    pub at: i64,
    /// The [`Source::id`] this concerned.
    ///
    /// Owned rather than borrowed from [`crate::SOURCES`], so that an entry
    /// read back from the file survives its source leaving the build. The
    /// record of what happened does not stop being true because a later
    /// version dropped the feature, and an audit log that quietly discards
    /// history whenever the code changes is not one.
    pub source: String,
    /// The [`Source::host`] that was, or would have been, contacted.
    pub host: String,
    pub outcome: Outcome,
    /// Bytes received. Zero when nothing was.
    pub bytes: u64,
    /// What was being looked up, in words a user would recognise — a track and
    /// artist, not an opaque identifier.
    pub subject: String,
    /// Why, when the outcome alone does not say. The error, usually.
    pub detail: Option<String>,
}

impl Entry {
    /// An entry timestamped now.
    pub fn new(source: &Source, outcome: Outcome, subject: impl Into<String>) -> Self {
        Self {
            at: timestamp::now_unix(),
            source: source.id.to_owned(),
            host: source.host.to_owned(),
            outcome,
            bytes: 0,
            subject: sanitise(subject.into()),
            detail: None,
        }
    }

    /// Record how much was received.
    pub fn with_bytes(mut self, bytes: u64) -> Self {
        self.bytes = bytes;
        self
    }

    /// Explain an outcome the reader would otherwise have to guess at.
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(sanitise(detail.into()));
        self
    }

    /// Override the timestamp. Tests, and replaying an entry read from disk.
    pub fn at_time(mut self, unix_seconds: i64) -> Self {
        self.at = unix_seconds;
        self
    }

    /// The line as it appears in the file, without its newline.
    fn to_line(&self) -> String {
        let mut line = format!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            timestamp::format(self.at),
            self.source,
            self.host,
            self.outcome.as_str(),
            self.bytes,
            self.subject,
        );

        if let Some(detail) = &self.detail {
            line.push('\t');
            line.push_str(detail);
        }

        line
    }

    /// Read a line back. `None` for a comment, a blank, or anything malformed.
    ///
    /// Fields are taken as the file has them, not looked up in
    /// [`crate::SOURCES`]. A line naming a source this build no longer has is
    /// still a true record of something that happened, and is kept.
    fn parse_line(line: &str) -> Option<Self> {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() || line.starts_with('#') {
            return None;
        }

        let mut fields = line.split('\t');
        let at = timestamp::parse(fields.next()?)?;
        let source = fields.next()?;
        let host = fields.next()?;
        let outcome = Outcome::parse(fields.next()?)?;
        let bytes = fields.next()?.parse().ok()?;
        let subject = fields.next()?.to_owned();
        let detail = fields.next().map(str::to_owned);

        if source.is_empty() || host.is_empty() {
            return None;
        }

        Some(Self {
            at,
            source: source.to_owned(),
            host: host.to_owned(),
            outcome,
            bytes,
            subject,
            detail,
        })
    }
}

/// Flatten anything that would break the one-entry-per-line format, and cap
/// the length.
fn sanitise(text: String) -> String {
    let mut cleaned: String = text
        .chars()
        .map(|c| {
            if c == '\t' || c == '\n' || c == '\r' {
                ' '
            } else {
                c
            }
        })
        .collect();

    if cleaned.chars().count() > MAX_TEXT_CHARS {
        // Truncating by bytes would split a multi-byte character and panic.
        let end = cleaned
            .char_indices()
            .nth(MAX_TEXT_CHARS)
            .map_or(cleaned.len(), |(index, _)| index);
        cleaned.truncate(end);
        cleaned.push('…');
    }

    cleaned
}

// ---------------------------------------------------------------------------
// The log
// ---------------------------------------------------------------------------

/// The header written at the top of a fresh file.
///
/// Written as separate lines rather than one literal with continuations: a
/// backslash-continued string in this codebase has twice been collapsed into
/// runs of literal spaces by tooling, and it shipped both times.
///
/// Deliberately pure ASCII. Everything below it is UTF-8 because tags are, but
/// the part that explains the file should stay legible whatever a given editor
/// decides the encoding is.
const HEADER: &[&str] = &[
    "# Resonance network activity.",
    "#",
    "# Every request this application makes to the internet is recorded here,",
    "# one per line, including the ones it decided not to send.",
    "#",
    "# Columns, separated by tabs:",
    "#   time (UTC)  source  host  outcome  bytes  subject  [detail]",
    "#",
    "# Outcomes:",
    "#   ok         the request was made and answered",
    "#   not-found  the request was made; the source had no answer",
    "#   cached     no request was made; the answer was already on this machine",
    "#   skipped    no request was made; rate limiting, backoff, or switched off",
    "#   failed     the request was made and went wrong; see the detail column",
    "#",
    "# This file is yours. Read it, copy it, or delete it. Deleting it starts a",
    "# new one rather than switching logging off.",
];

/// The activity log: an append-only file, plus the tail of it in memory.
///
/// Shared by reference across threads. Every method takes `&self`.
#[derive(Debug)]
pub struct Activity {
    inner: Mutex<Inner>,
}

#[derive(Debug)]
struct Inner {
    recent: VecDeque<Entry>,
    capacity: usize,
    /// `None` for a memory-only log, or once writing has failed.
    file: Option<File>,
    path: Option<PathBuf>,
    written: u64,
    max_bytes: u64,
    /// So a failing disk produces one warning rather than one per entry.
    reported_failure: bool,
}

impl Activity {
    /// A log that keeps entries in memory and writes nothing.
    ///
    /// For tests, and for the case where the data directory cannot be written:
    /// losing the file is a reason to carry on without it, not to disable the
    /// feature it describes.
    pub fn in_memory() -> Self {
        Self::build(None, RECENT_CAPACITY, MAX_LOG_BYTES)
    }

    /// A memory-only log holding a specific number of entries.
    pub fn in_memory_with_capacity(capacity: usize) -> Self {
        Self::build(None, capacity, MAX_LOG_BYTES)
    }

    /// Open the log at `path`, creating it if needed.
    ///
    /// An existing file is read for its most recent entries first, then
    /// rotated if it has grown past [`MAX_LOG_BYTES`].
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with(path, RECENT_CAPACITY, MAX_LOG_BYTES)
    }

    /// [`open`](Self::open) with both limits given explicitly.
    pub fn open_with(path: impl AsRef<Path>, capacity: usize, max_bytes: u64) -> Result<Self> {
        let path = path.as_ref().to_path_buf();

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }

        // Read before rotating, so the entries on screen survive a rotation
        // that happens the moment the app starts.
        let recent = read_tail(&path, capacity);

        let oversized = std::fs::metadata(&path).is_ok_and(|meta| meta.len() >= max_bytes);
        if oversized {
            rotate(&path)?;
        }

        let log = Self::build(Some(path.clone()), capacity, max_bytes);
        {
            let mut inner = log.lock();
            inner.recent = recent;
            inner.open_file(&path)?;
        }

        Ok(log)
    }

    fn build(path: Option<PathBuf>, capacity: usize, max_bytes: u64) -> Self {
        Self {
            inner: Mutex::new(Inner {
                recent: VecDeque::new(),
                // A zero-capacity log would panic on the first eviction.
                capacity: capacity.max(1),
                file: None,
                path,
                written: 0,
                max_bytes,
                reported_failure: false,
            }),
        }
    }

    /// Add an entry. Never fails, never panics, never blocks on anything but
    /// the write itself.
    ///
    /// The lock is held across that write, so a view calling
    /// [`recent`](Self::recent) on the UI thread shares it with whichever
    /// worker is recording. At one entry per second and a hundred bytes a
    /// time that is not worth an extra thread and a channel to avoid — but it
    /// is worth knowing before the first view is built on top of it.
    pub fn record(&self, entry: Entry) {
        let mut inner = self.lock();

        inner.write(&entry);

        if inner.recent.len() == inner.capacity {
            inner.recent.pop_front();
        }
        inner.recent.push_back(entry);
    }

    /// The most recent entries, newest first.
    pub fn recent(&self) -> Vec<Entry> {
        self.lock().recent.iter().rev().cloned().collect()
    }

    /// How many entries are held in memory.
    pub fn len(&self) -> usize {
        self.lock().recent.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().recent.is_empty()
    }

    /// Where the file is, for a "show me the log" button.
    pub fn path(&self) -> Option<PathBuf> {
        self.lock().path.clone()
    }

    /// How many of the entries in memory were actual requests.
    pub fn requests_made(&self) -> usize {
        self.lock()
            .recent
            .iter()
            .filter(|entry| entry.outcome.made_a_request())
            .count()
    }

    /// See [`Limiter::lock`](crate::Limiter) for why a poisoned lock is
    /// recovered from rather than propagated.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Inner {
    /// Open the file for appending, writing the header if it is new.
    fn open_file(&mut self, path: &Path) -> Result<()> {
        let fresh = !path.exists() || std::fs::metadata(path).is_ok_and(|meta| meta.len() == 0);

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))?;

        self.written = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);

        if fresh {
            for line in HEADER {
                writeln!(file, "{line}").with_context(|| format!("writing {}", path.display()))?;
                self.written += line.len() as u64 + 1;
            }
        }

        self.file = Some(file);
        Ok(())
    }

    fn write(&mut self, entry: &Entry) {
        let Some(file) = self.file.as_mut() else {
            return;
        };

        let line = entry.to_line();
        if let Err(error) = writeln!(file, "{line}") {
            // One warning, then stop trying. A disk that is full or read-only
            // will not become writable because we asked again, and a warning
            // per request would bury the log that still works in memory.
            if !self.reported_failure {
                tracing::warn!("network activity log is not writable: {error}");
                self.reported_failure = true;
            }
            self.file = None;
            return;
        }

        self.written += line.len() as u64 + 1;

        if self.written >= self.max_bytes {
            self.rotate_now();
        }
    }

    fn rotate_now(&mut self) {
        let Some(path) = self.path.clone() else {
            return;
        };

        // Windows will not rename over an open handle, so let go of it first.
        self.file = None;

        if let Err(error) = rotate(&path).and_then(|()| self.open_file(&path)) {
            tracing::warn!("could not rotate the network activity log: {error}");
            self.file = None;
        }
    }
}

/// Move the log aside, replacing any previous one.
///
/// Exactly one generation is kept. Two would be a retention policy, and this
/// is a disclosure record rather than an archive — the recent past is what
/// anyone actually checks.
fn rotate(path: &Path) -> Result<()> {
    let mut name = path.as_os_str().to_os_string();
    name.push(".old");
    let previous = PathBuf::from(name);

    // `rename` replaces an existing file on both platforms, but removing it
    // first keeps the failure modes down to one.
    let _ = std::fs::remove_file(&previous);

    std::fs::rename(path, &previous).with_context(|| format!("rotating {} aside", path.display()))
}

/// Read the last `capacity` readable entries from an existing log.
///
/// The file is capped at [`MAX_LOG_BYTES`], so reading all of it costs a
/// megabyte at worst and happens once at startup. Unreadable lines are
/// skipped: a truncated final line after a crash should cost that line and
/// nothing else.
fn read_tail(path: &Path, capacity: usize) -> VecDeque<Entry> {
    let mut entries = VecDeque::new();

    let Ok(text) = std::fs::read_to_string(path) else {
        return entries;
    };

    for line in text.lines() {
        let Some(entry) = Entry::parse_line(line) else {
            continue;
        };

        if entries.len() == capacity.max(1) {
            entries.pop_front();
        }
        entries.push_back(entry);
    }

    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const EXAMPLE: Source = Source {
        id: "example",
        label: "Example",
        host: "example.org",
        purpose: "A test fixture.",
        sends: "Nothing.",
        terms: "https://example.org/terms",
        min_interval: Duration::from_secs(1),
    };

    fn entry(outcome: Outcome, subject: &str) -> Entry {
        Entry::new(&EXAMPLE, outcome, subject).at_time(1_788_480_000)
    }

    // -- entries ------------------------------------------------------------

    #[test]
    fn a_line_carries_every_field_in_order() {
        let line = entry(Outcome::Ok, "artwork for Kid A")
            .with_bytes(4_096)
            .to_line();

        assert_eq!(
            line,
            "2026-09-04T00:00:00Z\texample\texample.org\tok\t4096\tartwork for Kid A"
        );
    }

    #[test]
    fn a_detail_is_appended_as_a_last_column() {
        let line = entry(Outcome::Failed, "artwork for Kid A")
            .with_detail("connection refused")
            .to_line();

        assert!(
            line.ends_with("\tfailed\t0\tartwork for Kid A\tconnection refused"),
            "unexpected line: {line}"
        );
    }

    /// A tab or a newline inside a tag would silently invent a column or an
    /// entry, so a malformed file could be produced by nothing worse than an
    /// unusual track title.
    #[test]
    fn a_subject_cannot_break_the_format() {
        let entry = entry(Outcome::Ok, "a\ttitle\nwith\r\nseparators in it");
        let line = entry.to_line();

        assert_eq!(
            line.matches('\t').count(),
            5,
            "the separators leaked into the columns: {line}"
        );
        assert!(!line.contains('\n'));
        assert_eq!(entry.subject, "a title with  separators in it");
    }

    #[test]
    fn an_enormous_subject_is_cut_short() {
        let entry = entry(Outcome::Ok, &"x".repeat(5_000));

        assert_eq!(entry.subject.chars().count(), MAX_TEXT_CHARS + 1);
        assert!(entry.subject.ends_with('…'));
    }

    /// Truncation counts characters, so a subject full of multi-byte text must
    /// not be cut through the middle of one.
    #[test]
    fn truncation_does_not_split_a_character() {
        let entry = entry(Outcome::Ok, &"日本語".repeat(500));

        assert_eq!(entry.subject.chars().count(), MAX_TEXT_CHARS + 1);
        assert!(entry.subject.starts_with("日本語"));
    }

    #[test]
    fn outcomes_name_whether_anything_left_the_machine() {
        assert!(Outcome::Ok.made_a_request());
        assert!(Outcome::NotFound.made_a_request());
        assert!(Outcome::Failed.made_a_request());

        assert!(!Outcome::Cached.made_a_request());
        assert!(!Outcome::Skipped.made_a_request());
    }

    #[test]
    fn every_outcome_survives_being_written_and_read_back() {
        for outcome in [
            Outcome::Ok,
            Outcome::NotFound,
            Outcome::Cached,
            Outcome::Skipped,
            Outcome::Failed,
        ] {
            assert_eq!(Outcome::parse(outcome.as_str()), Some(outcome));
        }
    }

    // -- the in-memory log --------------------------------------------------

    #[test]
    fn recent_entries_come_back_newest_first() {
        let log = Activity::in_memory();
        log.record(entry(Outcome::Ok, "first"));
        log.record(entry(Outcome::Ok, "second"));
        log.record(entry(Outcome::Ok, "third"));

        let recent = log.recent();
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0].subject, "third");
        assert_eq!(recent[2].subject, "first");
    }

    #[test]
    fn the_memory_copy_is_bounded() {
        let log = Activity::in_memory_with_capacity(3);
        for n in 0..10 {
            log.record(entry(Outcome::Ok, &format!("request {n}")));
        }

        let recent = log.recent();
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0].subject, "request 9");
        assert_eq!(recent[2].subject, "request 7");
    }

    #[test]
    fn a_summary_counts_only_the_actual_requests() {
        let log = Activity::in_memory();
        log.record(entry(Outcome::Ok, "one"));
        log.record(entry(Outcome::Cached, "two"));
        log.record(entry(Outcome::Skipped, "three"));
        log.record(entry(Outcome::Failed, "four"));

        assert_eq!(log.len(), 4, "everything is logged");
        assert_eq!(log.requests_made(), 2, "only two left the machine");
    }

    #[test]
    fn an_empty_log_says_so() {
        let log = Activity::in_memory();
        assert!(log.is_empty());
        assert!(log.recent().is_empty());
        assert_eq!(log.requests_made(), 0);
        assert!(log.path().is_none());
    }

    // -- the file -----------------------------------------------------------

    #[test]
    fn a_new_file_explains_itself_before_the_first_entry() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(LOG_FILE_NAME);

        let log = Activity::open(&path).expect("open");
        log.record(entry(Outcome::Ok, "artwork for Kid A"));
        drop(log);

        let text = std::fs::read_to_string(&path).expect("read back");
        assert!(text.starts_with("# Resonance network activity."));
        assert!(
            text.contains("# This file is yours."),
            "the header should tell the reader what the file is for"
        );
        assert!(text.contains("\tartwork for Kid A\n"));
    }

    /// The entries below the header are UTF-8 because tags are. The header
    /// itself explains the file, and should survive an editor guessing wrong.
    #[test]
    fn the_header_is_plain_ascii() {
        for line in HEADER {
            assert!(
                line.is_ascii(),
                "the header should not depend on an encoding: {line}"
            );
        }
    }

    #[test]
    fn entries_are_on_disk_the_moment_they_are_recorded() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(LOG_FILE_NAME);

        let log = Activity::open(&path).expect("open");
        log.record(entry(Outcome::Ok, "written immediately"));

        // Deliberately without dropping the log: a crash here must not lose it.
        let text = std::fs::read_to_string(&path).expect("read back");
        assert!(text.contains("written immediately"));
    }

    #[test]
    fn the_directory_is_created_if_it_is_missing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("nested").join("deeper").join(LOG_FILE_NAME);

        let log = Activity::open(&path).expect("open");
        log.record(entry(Outcome::Ok, "nested"));

        assert!(path.exists());
    }

    #[test]
    fn a_log_reopened_still_shows_what_came_before() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(LOG_FILE_NAME);

        {
            let log = Activity::open(&path).expect("open");
            log.record(entry(Outcome::Ok, "before the restart").with_bytes(64));
            log.record(entry(Outcome::Failed, "also before").with_detail("timed out"));
        }

        let log = Activity::open(&path).expect("reopen");
        let recent = log.recent();

        assert_eq!(recent.len(), 2, "entries survived the restart");
        assert_eq!(recent[0].subject, "also before");
        assert_eq!(recent[0].outcome, Outcome::Failed);
        assert_eq!(recent[0].detail.as_deref(), Some("timed out"));
        assert_eq!(recent[1].subject, "before the restart");
        assert_eq!(recent[1].bytes, 64);
    }

    #[test]
    fn reopening_appends_rather_than_starting_again() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(LOG_FILE_NAME);

        {
            let log = Activity::open(&path).expect("open");
            log.record(entry(Outcome::Ok, "first session"));
        }
        {
            let log = Activity::open(&path).expect("reopen");
            log.record(entry(Outcome::Ok, "second session"));
        }

        let text = std::fs::read_to_string(&path).expect("read back");
        assert!(text.contains("first session"));
        assert!(text.contains("second session"));
        assert_eq!(
            text.matches("# Resonance network activity.").count(),
            1,
            "the header should be written once, not once per run"
        );
    }

    /// A crash mid-write leaves a partial last line. It should cost that line.
    #[test]
    fn a_damaged_line_is_skipped_rather_than_poisoning_the_log() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(LOG_FILE_NAME);

        {
            let log = Activity::open(&path).expect("open");
            log.record(entry(Outcome::Ok, "a good entry"));
        }

        let mut file = OpenOptions::new().append(true).open(&path).expect("append");
        writeln!(file, "this is not a log line at all").expect("write");
        writeln!(file, "2026-09-04T00:00:00Z\texample").expect("write, too few fields");
        writeln!(file, "not-a-time\texample\texample.org\tok\t0\tsubject").expect("write");
        writeln!(
            file,
            "2026-09-04T00:00:00Z\texample\texample.org\tsideways\t0\ts"
        )
        .expect("write, unknown outcome");
        writeln!(
            file,
            "2026-09-04T00:00:00Z\texample\texample.org\tok\tlots\ts"
        )
        .expect("write, unparseable byte count");
        drop(file);

        let log = Activity::open(&path).expect("reopen");
        let recent = log.recent();

        assert_eq!(recent.len(), 1, "only the good entry should survive");
        assert_eq!(recent[0].subject, "a good entry");
    }

    /// The log is a record of what happened, so an entry naming a source this
    /// build no longer has is still true and is still shown. Dropping it would
    /// let removing a feature quietly rewrite the history of using it.
    #[test]
    fn an_entry_from_a_source_this_build_lacks_is_still_kept() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(LOG_FILE_NAME);

        {
            let log = Activity::open(&path).expect("open");
            log.record(entry(Outcome::Ok, "from a since-removed source"));
        }

        assert!(
            crate::source::find("example").is_none(),
            "the fixture is deliberately not in the registry"
        );

        let log = Activity::open(&path).expect("reopen");
        let recent = log.recent();

        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].source, "example");
        assert_eq!(recent[0].host, "example.org");
    }

    #[test]
    fn an_oversized_log_is_rotated_aside_rather_than_growing_without_end() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(LOG_FILE_NAME);

        let log = Activity::open_with(&path, RECENT_CAPACITY, 1_500).expect("open");
        for n in 0..200 {
            log.record(entry(Outcome::Ok, &format!("request number {n}")));
        }
        drop(log);

        let mut rotated = path.as_os_str().to_os_string();
        rotated.push(".old");
        let rotated = PathBuf::from(rotated);

        assert!(rotated.exists(), "the previous generation should be kept");

        let live = std::fs::metadata(&path).expect("live log").len();
        assert!(
            live < 1_500 * 2,
            "the live log should have been cut back, not left at {live} bytes"
        );
        assert!(
            std::fs::read_to_string(&path)
                .expect("read")
                .starts_with("# Resonance network activity."),
            "a rotated log should explain itself again"
        );
    }

    /// The rotated generation is replaced, not accumulated into `.old.old`.
    #[test]
    fn rotating_twice_keeps_exactly_one_previous_generation() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(LOG_FILE_NAME);

        let log = Activity::open_with(&path, RECENT_CAPACITY, 1_000).expect("open");
        for n in 0..400 {
            log.record(entry(Outcome::Ok, &format!("request number {n}")));
        }
        drop(log);

        let generations = std::fs::read_dir(dir.path())
            .expect("list")
            .filter_map(Result::ok)
            .filter(|item| {
                item.file_name()
                    .to_string_lossy()
                    .starts_with(LOG_FILE_NAME)
            })
            .count();

        assert_eq!(generations, 2, "one live log and one rotated one");
    }

    /// Rotation empties the file, not the view: the entries on screen should
    /// not vanish because the log happened to fill up.
    #[test]
    fn rotation_leaves_the_entries_in_memory_alone() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(LOG_FILE_NAME);

        let log = Activity::open_with(&path, 50, 1_000).expect("open");
        for n in 0..200 {
            log.record(entry(Outcome::Ok, &format!("request number {n}")));
        }

        assert_eq!(log.len(), 50);
        assert_eq!(log.recent()[0].subject, "request number 199");
    }

    /// Losing the file is a reason to carry on without it, not to lose the
    /// entries or to take the feature down with it.
    #[test]
    fn a_memory_only_log_keeps_working_with_nowhere_to_write() {
        let log = Activity::in_memory();

        log.record(entry(Outcome::Ok, "goes nowhere"));

        assert_eq!(log.len(), 1);
        assert_eq!(log.recent()[0].subject, "goes nowhere");
        assert!(log.path().is_none());
    }
}
