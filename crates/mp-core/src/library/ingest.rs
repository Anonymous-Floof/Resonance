//! Turning folders of files into an index.
//!
//! The scan has three phases, and they are separate on purpose:
//!
//! 1. **Walk** — cheap, single-threaded, touches only directory entries.
//! 2. **Read** — expensive, parallel, opens files and decodes cover art. Only
//!    files whose `(mtime, size)` fingerprint changed are read at all, which is
//!    what makes a rescan of an unchanged library finish in milliseconds
//!    instead of minutes.
//! 3. **Write** — one transaction. Thousands of small autocommitted inserts
//!    would each pay an fsync; batching them is the difference between a scan
//!    that takes seconds and one that takes minutes.
//!
//! Nothing here writes to the user's audio files. The scanner opens them
//! read-only and never renames, moves or retags anything.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use rayon::prelude::*;
use rusqlite::{Connection, OptionalExtension, params};

use crate::format::{self, Support};
use crate::library::art::{self, ArtCache};
use crate::library::model::UNKNOWN_ARTIST;
use crate::library::names;

/// Bump whenever the *interpretation* of a file changes.
///
/// The fingerprint cache asks "has this file changed?", which is the wrong
/// question after the parser changes: the bytes are identical but the metadata
/// we would now derive from them is not. Recording the version that produced
/// the current rows lets a build with better parsing re-read everything once,
/// instead of leaving old mistakes in the library until each file happens to be
/// touched. Cheap insurance — the cost is one full rescan per upgrade.
const PARSER_VERSION: u32 = 2;

/// Key under which [`PARSER_VERSION`] is stored.
const PARSER_VERSION_KEY: &str = "parser_version";

/// How deep to walk before assuming something is wrong.
const MAX_DEPTH: usize = 24;

/// Directories that never hold music worth indexing.
const IGNORED_DIRS: &[&str] = &["$recycle.bin", "system volume information", "__macosx"];

/// What a scan should do.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Folders walked recursively.
    pub roots: Vec<PathBuf>,
    /// Sort "The Wandering Hours" under B.
    pub ignore_articles: bool,
    /// Files shorter than this are skipped; strips interstitials and silence.
    pub min_duration: Duration,
    /// Extract and cache embedded cover art.
    pub extract_art: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            ignore_articles: true,
            min_duration: Duration::from_secs(5),
            extract_art: true,
        }
    }
}

impl ScanOptions {
    /// Build from the user's settings.
    pub fn from_config(config: &crate::config::Library) -> Self {
        Self {
            roots: config.watched_folders.clone(),
            ignore_articles: config.ignore_leading_articles,
            min_duration: Duration::from_secs(u64::from(config.min_track_seconds)),
            extract_art: true,
        }
    }
}

/// Which stage a running scan is in, for the progress indicator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Walking,
    Reading,
    Writing,
    Done,
}

impl Phase {
    fn code(self) -> u8 {
        match self {
            Self::Idle => 0,
            Self::Walking => 1,
            Self::Reading => 2,
            Self::Writing => 3,
            Self::Done => 4,
        }
    }

    fn from_code(code: u8) -> Self {
        match code {
            1 => Self::Walking,
            2 => Self::Reading,
            3 => Self::Writing,
            4 => Self::Done,
            _ => Self::Idle,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "Idle",
            Self::Walking => "Looking for music",
            Self::Reading => "Reading tags",
            Self::Writing => "Updating the library",
            Self::Done => "Done",
        }
    }
}

/// Live state of a running scan, shared with the UI.
///
/// Atomics rather than a channel: the UI samples this once a frame and does not
/// care about any value it missed, so a queue would only build up work.
#[derive(Debug, Default)]
pub struct Progress {
    phase: AtomicU8,
    found: AtomicU64,
    read: AtomicU64,
    to_read: AtomicU64,
    cancel: AtomicBool,
}

impl Progress {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn phase(&self) -> Phase {
        Phase::from_code(self.phase.load(Ordering::Relaxed))
    }

    /// Playable files seen so far by the walk.
    pub fn found(&self) -> u64 {
        self.found.load(Ordering::Relaxed)
    }

    /// Files whose tags have been read.
    pub fn read(&self) -> u64 {
        self.read.load(Ordering::Relaxed)
    }

    /// How many files this scan needs to read; 0 until the walk finishes.
    pub fn to_read(&self) -> u64 {
        self.to_read.load(Ordering::Relaxed)
    }

    /// 0.0 to 1.0, or `None` while the total is still unknown.
    pub fn fraction(&self) -> Option<f32> {
        let total = self.to_read();
        (total > 0).then(|| (self.read() as f32 / total as f32).clamp(0.0, 1.0))
    }

    /// Ask a running scan to stop at the next checkpoint.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn set_phase(&self, phase: Phase) {
        self.phase.store(phase.code(), Ordering::Relaxed);
    }

    /// Clear the counters so the same handle can drive another scan.
    ///
    /// Deliberately leaves `cancel` alone: a scan is started by handing over a
    /// fresh `Progress`, and silently clearing a cancellation here would make
    /// "stop, then rescan" race against itself.
    pub fn reset(&self) {
        self.phase.store(Phase::Idle.code(), Ordering::Relaxed);
        self.found.store(0, Ordering::Relaxed);
        self.read.store(0, Ordering::Relaxed);
        self.to_read.store(0, Ordering::Relaxed);
    }
}

/// What a finished scan did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    pub added: u32,
    pub updated: u32,
    pub removed: u32,
    /// Files whose fingerprint was unchanged, so they were never opened.
    pub unchanged: u32,
    /// Audio files this build has no decoder for.
    pub unplayable: u32,
    /// Directories that could not be read.
    pub unreadable: u32,
    /// Files that looked playable but whose tags could not be read.
    pub failed: u32,
    /// Files skipped for being shorter than the minimum.
    pub too_short: u32,
    /// Tracks whose artist was recovered from elsewhere in the library.
    pub artists_recovered: u32,
    pub elapsed: Duration,
    pub cancelled: bool,
}

impl Summary {
    /// Whether anything about the library actually changed.
    pub fn changed_anything(self) -> bool {
        self.added > 0 || self.updated > 0 || self.removed > 0
    }

    /// A one-line description for the UI.
    pub fn describe(self) -> String {
        if self.cancelled {
            return "Scan cancelled".to_owned();
        }
        if !self.changed_anything() {
            return format!("Library up to date — {} tracks", self.unchanged);
        }

        let mut parts = Vec::new();
        if self.added > 0 {
            parts.push(format!("{} added", self.added));
        }
        if self.updated > 0 {
            parts.push(format!("{} updated", self.updated));
        }
        if self.removed > 0 {
            parts.push(format!("{} removed", self.removed));
        }
        parts.join(", ")
    }
}

// ---------------------------------------------------------------------------
// Phase 1: walking
// ---------------------------------------------------------------------------

/// A playable file found on disk, with its change fingerprint.
#[derive(Debug, Clone)]
struct Found {
    path: PathBuf,
    folder: PathBuf,
    mtime: i64,
    size: i64,
}

#[derive(Debug, Default)]
struct Walked {
    found: Vec<Found>,
    unplayable: Vec<(PathBuf, PathBuf, &'static str)>,
    unreadable: Vec<PathBuf>,
}

fn walk(options: &ScanOptions, progress: &Progress) -> Walked {
    let mut out = Walked::default();
    progress.set_phase(Phase::Walking);

    for root in &options.roots {
        if !root.is_dir() {
            // An unplugged drive or a folder the user deleted. Nothing beneath
            // it is pruned, because "not visible right now" is not "gone".
            tracing::warn!("watched folder is not available: {}", root.display());
            out.unreadable.push(root.clone());
            continue;
        }
        let walker = walkdir::WalkDir::new(root)
            .max_depth(MAX_DEPTH)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| !is_ignored_dir(entry.path()));

        for entry in walker {
            if progress.is_cancelled() {
                return out;
            }

            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    if let Some(path) = err.path() {
                        out.unreadable.push(path.to_path_buf());
                    }
                    continue;
                }
            };

            if !entry.file_type().is_file() {
                continue;
            }

            let path = entry.path();
            let folder = path.parent().unwrap_or(root).to_path_buf();

            match format::classify(path) {
                Support::Supported => {
                    let Ok(metadata) = entry.metadata() else {
                        continue;
                    };
                    out.found.push(Found {
                        path: path.to_path_buf(),
                        folder,
                        mtime: unix_seconds(metadata.modified().ok()),
                        size: metadata.len() as i64,
                    });
                    progress.found.fetch_add(1, Ordering::Relaxed);
                }
                Support::Unsupported { reason } => {
                    out.unplayable.push((path.to_path_buf(), folder, reason));
                }
                Support::NotAudio => {}
            }
        }
    }

    out
}

fn is_ignored_dir(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    // The root itself may legitimately start with a dot; only skip hidden
    // entries that are nested.
    if name.starts_with('.') && name.len() > 1 {
        return true;
    }
    IGNORED_DIRS.contains(&name.to_ascii_lowercase().as_str())
}

fn unix_seconds(time: Option<SystemTime>) -> i64 {
    time.and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64)
}

fn now_seconds() -> i64 {
    unix_seconds(Some(SystemTime::now()))
}

// ---------------------------------------------------------------------------
// Phase 2: reading
// ---------------------------------------------------------------------------

/// Everything read out of one file.
#[derive(Debug, Clone)]
struct Scanned {
    path: PathBuf,
    folder: PathBuf,
    file_name: String,
    mtime: i64,
    size: i64,

    title: String,
    artist: String,
    /// The album's credited artist, which differs from the track artist on
    /// compilations. Falls back to the track artist.
    album_artist: String,
    album: Option<String>,
    genres: Vec<String>,
    track_no: Option<u32>,
    disc_no: Option<u32>,
    year: Option<i32>,
    duration: Option<Duration>,
    sample_rate: Option<u32>,
    channels: Option<u32>,
    bitrate: Option<u32>,
    gain_track: Option<f64>,
    gain_album: Option<f64>,
    art_id: Option<String>,
    /// False when title and artist came from the filename.
    tagged: bool,
}

/// Read one file's metadata. Never fails the scan: a file that cannot be
/// parsed still enters the library under its filename, because the user can
/// see it in the folder and would rightly expect to see it here.
fn read_file(found: &Found, options: &ScanOptions, cache: Option<&ArtCache>) -> Scanned {
    let stem = found
        .path
        .file_stem()
        .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    let file_name = found
        .path
        .file_name()
        .map_or_else(String::new, |s| s.to_string_lossy().into_owned());

    let parsed = names::parse(&stem);

    let mut scanned = Scanned {
        path: found.path.clone(),
        folder: found.folder.clone(),
        file_name,
        mtime: found.mtime,
        size: found.size,
        title: parsed.title.clone(),
        artist: parsed
            .artist
            .clone()
            .unwrap_or_else(|| UNKNOWN_ARTIST.to_owned()),
        album_artist: parsed
            .artist
            .clone()
            .unwrap_or_else(|| UNKNOWN_ARTIST.to_owned()),
        album: None,
        genres: Vec::new(),
        track_no: parsed.track_no,
        disc_no: None,
        year: None,
        duration: None,
        sample_rate: None,
        channels: None,
        bitrate: None,
        gain_track: None,
        gain_album: None,
        art_id: None,
        tagged: false,
    };

    read_tags(&mut scanned, options, cache);
    tidy(&mut scanned);

    // A folder cover stands in when nothing is embedded — but only inside a
    // folder that plausibly *is* an album.
    //
    // A watched root is a catch-all. A downloaded collection routinely has
    // hundreds of unrelated tracks sitting loose in one, beside whatever
    // `Folder.jpg` Windows left there. Pasting that single image onto every one
    // of those rows is worse than showing no artwork at all, because it asserts
    // something false about each of them.
    let folder_is_a_root = options
        .roots
        .iter()
        .any(|root| path_key(root) == path_key(&found.folder));

    if scanned.art_id.is_none()
        && options.extract_art
        && !folder_is_a_root
        && let Some(cache) = cache
        && let Some(sidecar) = art::sidecar_in(&found.folder)
        && let Ok(bytes) = std::fs::read(&sidecar)
    {
        match cache.store(&bytes) {
            Ok(id) => scanned.art_id = Some(id),
            Err(err) => {
                tracing::debug!("unusable cover {}: {err:#}", sidecar.display());
            }
        }
    }

    scanned
}

/// Fill in whatever the file's own tags provide, leaving filename-derived
/// values in place where a tag is absent or blank.
fn read_tags(scanned: &mut Scanned, options: &ScanOptions, cache: Option<&ArtCache>) {
    use lofty::config::ParseOptions;
    use lofty::file::AudioFile;
    use lofty::picture::PictureType;
    use lofty::prelude::{Accessor, ItemKey, TaggedFileExt};
    use lofty::probe::Probe;

    let parse = ParseOptions::new()
        .read_properties(true)
        .read_tags(true)
        .read_cover_art(options.extract_art);

    let tagged_file = match Probe::open(&scanned.path).and_then(|p| p.options(parse).read()) {
        Ok(file) => file,
        Err(err) => {
            // Not fatal: the filename-derived values already in `scanned` are
            // what the user would have seen in Explorer anyway.
            tracing::debug!("no readable tags in {}: {err}", scanned.path.display());
            return;
        }
    };

    let properties = tagged_file.properties();
    let duration = properties.duration();
    if !duration.is_zero() {
        scanned.duration = Some(duration);
    }
    scanned.sample_rate = properties.sample_rate();
    scanned.channels = properties.channels().map(u32::from);
    scanned.bitrate = properties.audio_bitrate();

    // Prefer the format's primary tag; fall back to any tag present, because a
    // file can carry only an APEv2 or only an ID3v1 block.
    let Some(tag) = tagged_file
        .primary_tag()
        .or_else(|| tagged_file.first_tag())
    else {
        return;
    };

    if let Some(title) = clean(tag.title().as_deref()) {
        scanned.title = title;
        scanned.tagged = true;
    }
    if let Some(artist) = clean(tag.artist().as_deref()) {
        scanned.artist = artist.clone();
        scanned.album_artist = artist;
        scanned.tagged = true;
    }
    if let Some(album_artist) = clean(tag.get_string(ItemKey::AlbumArtist)) {
        scanned.album_artist = album_artist;
    }
    scanned.album = clean(tag.album().as_deref());

    if let Some(genre) = clean(tag.genre().as_deref()) {
        scanned.genres = split_genres(&genre);
    }

    scanned.track_no = tag.track().or(scanned.track_no);
    scanned.disc_no = tag.disk();

    scanned.year = tag
        .get_string(ItemKey::Year)
        .and_then(year_from)
        // A `Timestamp` carries a raw `year` field with no validity guarantee,
        // and taggers do write 0. Held to the same range as the text paths so a
        // junk value cannot reach the UI as "· 0".
        .or_else(|| {
            tag.date()
                .map(|d| i32::from(d.year))
                .filter(is_plausible_year)
        })
        .or_else(|| tag.get_string(ItemKey::RecordingDate).and_then(year_from))
        .or_else(|| {
            tag.get_string(ItemKey::OriginalReleaseDate)
                .and_then(year_from)
        });

    scanned.gain_track = tag
        .get_string(ItemKey::ReplayGainTrackGain)
        .and_then(parse_gain_db);
    scanned.gain_album = tag
        .get_string(ItemKey::ReplayGainAlbumGain)
        .and_then(parse_gain_db);

    if !options.extract_art {
        return;
    }
    let Some(cache) = cache else { return };

    // The front cover is the one to show. Failing that, any picture at all is
    // better than an empty square.
    let picture = tag
        .get_picture_type(PictureType::CoverFront)
        .or_else(|| tag.pictures().first());

    if let Some(picture) = picture {
        match cache.store(picture.data()) {
            Ok(id) => scanned.art_id = Some(id),
            Err(err) => {
                tracing::debug!(
                    "unusable embedded cover in {}: {err:#}",
                    scanned.path.display()
                );
            }
        }
    }
}

/// Clean up the decoration that survives both tags and filenames.
///
/// Runs after every other source has had its say, so it applies equally to a
/// title that came from an ID3 frame and one recovered from a filename - both
/// carry the same ripper noise.
fn tidy(scanned: &mut Scanned) {
    scanned.artist = names::decode_entities(&scanned.artist);
    scanned.album_artist = names::decode_entities(&scanned.album_artist);
    scanned.title = names::decode_entities(&scanned.title);

    scanned.artist = names::strip_channel_suffix(&scanned.artist);
    scanned.album_artist = names::strip_channel_suffix(&scanned.album_artist);

    scanned.title = names::strip_watermarks(&scanned.title);
    scanned.title = names::strip_redundant_artist_prefix(&scanned.title, &scanned.artist);

    if let Some(album) = &scanned.album {
        let cleaned = names::strip_watermarks(&names::decode_entities(album));
        scanned.album = (!cleaned.is_empty()).then_some(cleaned);
    }

    for genre in &mut scanned.genres {
        *genre = names::decode_entities(genre);
    }
}

/// Trim a tag value, treating whitespace-only and common placeholders as absent.
///
/// Taggers write "Unknown", "N/A" and empty strings freely; taking those
/// literally would fill the artist list with junk entries.
fn clean(value: Option<&str>) -> Option<String> {
    let text = value?.trim();
    if text.is_empty() {
        return None;
    }
    let lowered = text.to_ascii_lowercase();
    if matches!(
        lowered.as_str(),
        "unknown" | "unknown artist" | "unknown album" | "n/a" | "na" | "none" | "<unknown>"
    ) {
        return None;
    }
    Some(text.to_owned())
}

/// Split a genre field into individual genres.
///
/// Only `;` and `/` are treated as separators. Commas are deliberately left
/// alone: real genre names contain them ("Folk, World, & Country"), and
/// splitting on them invents genres that do not exist.
fn split_genres(value: &str) -> Vec<String> {
    value
        .split([';', '/'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Pull a four-digit year out of whatever date format the tagger used.
fn year_from(value: &str) -> Option<i32> {
    let digits: String = value.chars().take_while(char::is_ascii_digit).collect();
    let year: i32 = digits.parse().ok()?;
    is_plausible_year(&year).then_some(year)
}

/// Reject obvious nonsense, so a corrupt tag cannot sort above everything.
fn is_plausible_year(year: &i32) -> bool {
    (1000..=9999).contains(year)
}

/// ReplayGain values are written as `-7.25 dB`.
fn parse_gain_db(value: &str) -> Option<f64> {
    value
        .trim()
        .trim_end_matches(|c: char| c.is_ascii_alphabetic() || c.is_whitespace())
        .trim()
        .parse()
        .ok()
}

/// Recover artists for tracks that ended up as "Unknown", using the rest of the
/// library as evidence.
///
/// A filename like `TryHardNinja-We Know What Scares You.mp3` has no space
/// around its dash, so splitting it on sight would also wreck `Winds-of-Fjord`.
/// Waiting until every file has been read removes the guesswork: a split is
/// accepted only when the proposed artist is one that other files in the
/// library already name, and the containing folder is accepted on the same
/// terms. Both are evidence from the user's own collection rather than a rule
/// imposed on it.
///
/// Returns how many tracks were rescued, for the log.
fn recover_unknown_artists(scanned: &mut [Scanned]) -> u32 {
    // Every artist the library is confident about, keyed for case-insensitive
    // lookup and mapped back to the spelling to display.
    let mut known: HashMap<String, String> = HashMap::new();
    for track in scanned.iter() {
        if track.artist != UNKNOWN_ARTIST {
            known
                .entry(track.artist.to_lowercase())
                .or_insert_with(|| track.artist.clone());
        }
    }

    if known.is_empty() {
        return 0;
    }

    let mut rescued = 0;
    for track in scanned.iter_mut() {
        if track.artist != UNKNOWN_ARTIST {
            continue;
        }

        // A tight `Artist-Title` split, accepted only if the artist is real.
        if let Some((candidate, rest)) = names::propose_tight_split(&track.title)
            && let Some(display) = known.get(&candidate.to_lowercase())
        {
            track.artist = display.clone();
            track.album_artist = display.clone();
            track.title = rest.trim().to_owned();
            rescued += 1;
            continue;
        }

        // Otherwise the folder, when it is named after an artist the library
        // already knows. Mood folders like "Calming" never match.
        let folder_name = track
            .folder
            .file_name()
            .map(|name| name.to_string_lossy().to_lowercase());

        if let Some(display) = folder_name.as_deref().and_then(|name| known.get(name)) {
            track.artist = display.clone();
            track.album_artist = display.clone();
            rescued += 1;
        }
    }

    rescued
}

// ---------------------------------------------------------------------------
// Phase 3: writing
// ---------------------------------------------------------------------------

/// Run a full scan against `connection`.
pub fn scan(
    connection: &mut Connection,
    cache: Option<&ArtCache>,
    options: &ScanOptions,
    progress: &Progress,
) -> Result<Summary> {
    let started = std::time::Instant::now();
    progress.reset();

    if progress.is_cancelled() {
        progress.set_phase(Phase::Done);
        return Ok(Summary {
            cancelled: true,
            elapsed: started.elapsed(),
            ..Summary::default()
        });
    }

    let walked = walk(options, progress);
    let mut summary = Summary {
        unplayable: walked.unplayable.len() as u32,
        unreadable: walked.unreadable.len() as u32,
        ..Summary::default()
    };

    if progress.is_cancelled() {
        summary.cancelled = true;
        summary.elapsed = started.elapsed();
        progress.set_phase(Phase::Done);
        return Ok(summary);
    }

    // Existing fingerprints, so unchanged files are never opened.
    let known = load_fingerprints(connection)?;

    // ...unless this build reads metadata differently from the one that wrote
    // them, in which case every file has to be looked at again.
    let reparse = stored_parser_version(connection)? != Some(PARSER_VERSION);
    if reparse && !known.is_empty() {
        tracing::info!("metadata parser changed; re-reading every file once");
    }

    let mut to_read = Vec::new();
    let mut seen: HashSet<String> = HashSet::with_capacity(walked.found.len());

    for found in &walked.found {
        let key = path_key(&found.path);
        seen.insert(key.clone());

        match known.get(&key) {
            Some(&(_, mtime, size)) if !reparse && mtime == found.mtime && size == found.size => {
                summary.unchanged += 1;
            }
            _ => to_read.push(found.clone()),
        }
    }

    progress
        .to_read
        .store(to_read.len() as u64, Ordering::Relaxed);
    progress.set_phase(Phase::Reading);

    // The expensive part, and the only part worth parallelising: each file is
    // an independent open-parse-decode with no shared state beyond the art
    // cache, which is content addressed and safe to write concurrently.
    let scanned: Vec<Scanned> = to_read
        .par_iter()
        .map(|found| {
            let result = read_file(found, options, cache);
            progress.read.fetch_add(1, Ordering::Relaxed);
            result
        })
        .collect();

    if progress.is_cancelled() {
        summary.cancelled = true;
        summary.elapsed = started.elapsed();
        progress.set_phase(Phase::Done);
        return Ok(summary);
    }

    progress.set_phase(Phase::Writing);

    // Needs the whole batch in hand, so it cannot happen during the parallel
    // read above.
    let mut scanned = scanned;
    summary.artists_recovered = recover_unknown_artists(&mut scanned);
    write_batch(
        connection,
        &scanned,
        &walked,
        &seen,
        options,
        &known,
        &mut summary,
    )?;

    summary.elapsed = started.elapsed();
    progress.set_phase(Phase::Done);

    tracing::info!(
        "scan finished in {:.2}s: {}",
        summary.elapsed.as_secs_f32(),
        summary.describe()
    );

    Ok(summary)
}

/// Path as stored in the database.
///
/// Windows paths are case-insensitive, so the same file reached through two
/// different capitalisations must map to one row. The verbatim path is kept in
/// the `path` column; this is only the lookup key.
fn path_key(path: &Path) -> String {
    let text = path.to_string_lossy();
    if cfg!(windows) {
        text.to_lowercase().replace('/', "\\")
    } else {
        text.into_owned()
    }
}

type Fingerprints = HashMap<String, (i64, i64, i64)>;

fn load_fingerprints(connection: &Connection) -> Result<Fingerprints> {
    let mut statement = connection.prepare("SELECT id, path, mtime, size FROM tracks")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
        ))
    })?;

    let mut out = Fingerprints::new();
    for row in rows {
        let (id, path, mtime, size) = row?;
        out.insert(path_key(Path::new(&path)), (id, mtime, size));
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn write_batch(
    connection: &mut Connection,
    scanned: &[Scanned],
    walked: &Walked,
    seen: &HashSet<String>,
    options: &ScanOptions,
    known: &Fingerprints,
    summary: &mut Summary,
) -> Result<()> {
    let transaction = connection.transaction()?;
    let now = now_seconds();

    {
        let mut interner = Interner::new(options.ignore_articles);

        for track in scanned {
            if let Some(duration) = track.duration
                && duration < options.min_duration
            {
                summary.too_short += 1;
                continue;
            }

            let existing = known.get(&path_key(&track.path)).map(|(id, _, _)| *id);
            match upsert_track(&transaction, &mut interner, track, existing, now) {
                Ok(()) if existing.is_some() => summary.updated += 1,
                Ok(()) => summary.added += 1,
                Err(err) => {
                    tracing::warn!("could not index {}: {err:#}", track.path.display());
                    summary.failed += 1;
                }
            }
        }
    }

    // Files that vanished. Anything under a folder we could not read is left
    // alone, so an unplugged drive does not erase its half of the library.
    summary.removed = prune_missing(&transaction, seen, &walked.unreadable)?;

    record_problems(&transaction, walked, now)?;
    prune_orphans(&transaction)?;

    // Written inside the same transaction as the rows it describes, so an
    // interrupted scan cannot claim to have re-parsed files it never reached.
    transaction.execute(
        "INSERT INTO meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![PARSER_VERSION_KEY, PARSER_VERSION.to_string()],
    )?;

    transaction.commit()?;
    Ok(())
}

/// The parser version that produced the rows currently in the index.
fn stored_parser_version(connection: &Connection) -> Result<Option<u32>> {
    let value: Option<String> = connection
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![PARSER_VERSION_KEY],
            |row| row.get(0),
        )
        .optional()?;
    Ok(value.and_then(|text| text.parse().ok()))
}

/// Caches the id of every artist, album and genre touched by this scan.
///
/// Without it a 20-track album performs 20 identical album lookups; with it,
/// one. The cache lives only for the duration of one transaction, so it cannot
/// go stale.
struct Interner {
    ignore_articles: bool,
    artists: HashMap<String, i64>,
    albums: HashMap<(String, i64), i64>,
    genres: HashMap<String, i64>,
}

impl Interner {
    fn new(ignore_articles: bool) -> Self {
        Self {
            ignore_articles,
            artists: HashMap::new(),
            albums: HashMap::new(),
            genres: HashMap::new(),
        }
    }

    fn artist(&mut self, tx: &rusqlite::Transaction<'_>, name: &str) -> Result<i64> {
        if let Some(id) = self.artists.get(name) {
            return Ok(*id);
        }
        let sort = names::sort_key(name, self.ignore_articles);
        let id: i64 = tx.query_row(
            "INSERT INTO artists(name, sort_name) VALUES (?1, ?2)
             ON CONFLICT(name) DO UPDATE SET sort_name = excluded.sort_name
             RETURNING id",
            params![name, sort],
            |row| row.get(0),
        )?;
        self.artists.insert(name.to_owned(), id);
        Ok(id)
    }

    fn album(
        &mut self,
        tx: &rusqlite::Transaction<'_>,
        title: &str,
        artist_id: i64,
        year: Option<i32>,
        art_id: Option<&str>,
    ) -> Result<i64> {
        let key = (title.to_owned(), artist_id);
        if let Some(id) = self.albums.get(&key) {
            return Ok(*id);
        }

        let sort = names::sort_key(title, self.ignore_articles);
        // `COALESCE` so the first track that happens to carry a year or a cover
        // fills them in, and a later track missing them does not blank them out.
        let id: i64 = tx.query_row(
            "INSERT INTO albums(title, sort_title, artist_id, year, art_id)
                 VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(title, artist_id) DO UPDATE SET
                 sort_title = excluded.sort_title,
                 year       = COALESCE(albums.year, excluded.year),
                 art_id     = COALESCE(albums.art_id, excluded.art_id)
             RETURNING id",
            params![title, sort, artist_id, year, art_id],
            |row| row.get(0),
        )?;
        self.albums.insert(key, id);
        Ok(id)
    }

    fn genre(&mut self, tx: &rusqlite::Transaction<'_>, name: &str) -> Result<i64> {
        if let Some(id) = self.genres.get(name) {
            return Ok(*id);
        }
        let sort = names::sort_key(name, false);
        let id: i64 = tx.query_row(
            "INSERT INTO genres(name, sort_name) VALUES (?1, ?2)
             ON CONFLICT(name) DO UPDATE SET sort_name = excluded.sort_name
             RETURNING id",
            params![name, sort],
            |row| row.get(0),
        )?;
        self.genres.insert(name.to_owned(), id);
        Ok(id)
    }
}

fn upsert_track(
    tx: &rusqlite::Transaction<'_>,
    interner: &mut Interner,
    track: &Scanned,
    existing: Option<i64>,
    now: i64,
) -> Result<()> {
    let artist_id = interner.artist(tx, &track.artist)?;

    // Albums are keyed by their credited artist, not the track artist, so a
    // compilation stays one album instead of splitting per featured performer.
    let album_id = match &track.album {
        Some(title) => {
            let album_artist_id = interner.artist(tx, &track.album_artist)?;
            Some(interner.album(
                tx,
                title,
                album_artist_id,
                track.year,
                track.art_id.as_deref(),
            )?)
        }
        None => None,
    };

    let sort_title = names::sort_key(&track.title, interner.ignore_articles);
    let duration_ms = track.duration.map(|d| d.as_millis() as i64);
    let path = track.path.to_string_lossy().into_owned();
    let folder = track.folder.to_string_lossy().into_owned();

    let id: i64 = if let Some(id) = existing {
        tx.execute(
            "UPDATE tracks SET
                path = ?2, folder = ?3, file_name = ?4, mtime = ?5, size = ?6,
                title = ?7, sort_title = ?8, artist_id = ?9, album_id = ?10,
                track_no = ?11, disc_no = ?12, year = ?13, duration_ms = ?14,
                sample_rate = ?15, channels = ?16, bitrate = ?17, art_id = ?18,
                gain_track = ?19, gain_album = ?20, tagged = ?21, last_seen_at = ?22
             WHERE id = ?1",
            params![
                id,
                path,
                folder,
                track.file_name,
                track.mtime,
                track.size,
                track.title,
                sort_title,
                artist_id,
                album_id,
                track.track_no,
                track.disc_no,
                track.year,
                duration_ms,
                track.sample_rate,
                track.channels,
                track.bitrate,
                track.art_id,
                track.gain_track,
                track.gain_album,
                i32::from(track.tagged),
                now,
            ],
        )?;
        id
    } else {
        tx.query_row(
            "INSERT INTO tracks(
                path, folder, file_name, mtime, size, title, sort_title,
                artist_id, album_id, track_no, disc_no, year, duration_ms,
                sample_rate, channels, bitrate, art_id, gain_track, gain_album,
                tagged, added_at, last_seen_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?21)
             RETURNING id",
            params![
                path,
                folder,
                track.file_name,
                track.mtime,
                track.size,
                track.title,
                sort_title,
                artist_id,
                album_id,
                track.track_no,
                track.disc_no,
                track.year,
                duration_ms,
                track.sample_rate,
                track.channels,
                track.bitrate,
                track.art_id,
                track.gain_track,
                track.gain_album,
                i32::from(track.tagged),
                now,
            ],
            |row| row.get(0),
        )?
    };

    // Genres are replaced wholesale rather than merged: a retagged file that
    // dropped a genre should lose it here too.
    tx.execute("DELETE FROM track_genres WHERE track_id = ?1", params![id])?;
    for genre in &track.genres {
        let genre_id = interner.genre(tx, genre)?;
        tx.execute(
            "INSERT OR IGNORE INTO track_genres(track_id, genre_id) VALUES (?1, ?2)",
            params![id, genre_id],
        )?;
    }

    index_for_search(tx, id, track)?;
    Ok(())
}

/// Keep the search index in step with the row it describes.
fn index_for_search(tx: &rusqlite::Transaction<'_>, id: i64, track: &Scanned) -> Result<()> {
    tx.execute("DELETE FROM tracks_fts WHERE rowid = ?1", params![id])?;
    tx.execute(
        "INSERT INTO tracks_fts(rowid, title, artist, album, genre)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            id,
            track.title,
            track.artist,
            track.album.clone().unwrap_or_default(),
            track.genres.join(" "),
        ],
    )?;
    Ok(())
}

/// Delete tracks whose files are gone.
///
/// "Gone" is narrower than "not seen this time". A folder that could not be
/// read — an unplugged drive, a permissions change, a network share that is
/// down — makes its tracks invisible without making them missing, and wiping
/// them would silently destroy play counts and playlist membership for a
/// condition that fixes itself when the drive comes back. So anything beneath
/// an unreadable path is left exactly as it was.
///
/// Everything else that was not seen is removed, including tracks under a
/// folder the user has taken out of their watch list.
fn prune_missing(
    tx: &rusqlite::Transaction<'_>,
    seen: &HashSet<String>,
    protected: &[PathBuf],
) -> Result<u32> {
    let protected: Vec<String> = protected.iter().map(|path| path_key(path)).collect();

    let mut statement = tx.prepare("SELECT id, path FROM tracks")?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(statement);

    let mut doomed = Vec::new();
    for (id, path) in rows {
        let key = path_key(Path::new(&path));
        if seen.contains(&key) {
            continue;
        }
        if protected.iter().any(|root| is_under(&key, root)) {
            continue;
        }
        doomed.push(id);
    }

    for id in &doomed {
        tx.execute("DELETE FROM tracks_fts WHERE rowid = ?1", params![id])?;
        tx.execute("DELETE FROM tracks WHERE id = ?1", params![id])?;
    }

    Ok(doomed.len() as u32)
}

/// Whether `key` names something inside `root`, both already normalised by
/// [`path_key`].
///
/// A plain `starts_with` would treat `D:/Music2` as being inside `D:/Music`,
/// so the character after the prefix has to be a separator.
fn is_under(key: &str, root: &str) -> bool {
    if key == root {
        return true;
    }
    let Some(rest) = key.strip_prefix(root) else {
        return false;
    };
    rest.starts_with(std::path::is_separator) || root.ends_with(std::path::is_separator)
}

/// Replace the record of what could not be played or read.
fn record_problems(tx: &rusqlite::Transaction<'_>, walked: &Walked, now: i64) -> Result<()> {
    tx.execute("DELETE FROM unplayable", [])?;
    for (path, folder, reason) in &walked.unplayable {
        tx.execute(
            "INSERT OR REPLACE INTO unplayable(path, folder, reason, seen_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                path.to_string_lossy(),
                folder.to_string_lossy(),
                reason,
                now
            ],
        )?;
    }

    tx.execute("DELETE FROM unreadable", [])?;
    for path in &walked.unreadable {
        tx.execute(
            "INSERT OR REPLACE INTO unreadable(path, seen_at) VALUES (?1, ?2)",
            params![path.to_string_lossy(), now],
        )?;
    }

    Ok(())
}

/// Drop artists, albums and genres that no longer have any tracks.
fn prune_orphans(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    tx.execute(
        "DELETE FROM albums WHERE id NOT IN
             (SELECT album_id FROM tracks WHERE album_id IS NOT NULL)",
        [],
    )?;
    tx.execute(
        "DELETE FROM artists WHERE id NOT IN
             (SELECT artist_id FROM tracks WHERE artist_id IS NOT NULL)
           AND id NOT IN
             (SELECT artist_id FROM albums WHERE artist_id IS NOT NULL)",
        [],
    )?;
    tx.execute(
        "DELETE FROM genres WHERE id NOT IN (SELECT genre_id FROM track_genres)",
        [],
    )?;
    Ok(())
}

/// Cover ids no longer referenced by any track or album.
pub fn unreferenced_art(connection: &Connection) -> Result<Vec<String>> {
    let mut statement = connection.prepare(
        "SELECT DISTINCT art_id FROM (
             SELECT art_id FROM tracks WHERE art_id IS NOT NULL
             UNION ALL
             SELECT art_id FROM albums WHERE art_id IS NOT NULL
         )",
    )?;
    let referenced: HashSet<String> = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .filter_map(std::result::Result::ok)
        .collect();
    drop(statement);

    // The caller compares this against what is on disk; returning the live set
    // rather than the dead one keeps the filesystem walk out of here.
    Ok(referenced.into_iter().collect())
}

/// Look up a track id by path, for reconciling the queue with the index.
pub fn track_id_for_path(connection: &Connection, path: &Path) -> Result<Option<i64>> {
    let id = connection
        .query_row(
            "SELECT id FROM tracks WHERE path = ?1",
            params![path.to_string_lossy()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(id)
}

/// Find a track by filename alone, when exactly one track has that name.
///
/// The fallback for importing a playlist written by other software, where the
/// paths may differ from ours in case, in drive letter, or in how the same
/// share is mounted, while the filenames match exactly. Restricting it to
/// unambiguous matches is what keeps it honest: with two `track01.mp3` in the
/// library there is no way to tell which one was meant, and guessing would
/// quietly put the wrong song in the playlist.
pub fn track_id_for_filename(connection: &Connection, name: &str) -> Result<Option<i64>> {
    let mut statement = connection.prepare(
        "SELECT id FROM tracks
         WHERE path = ?1 OR path LIKE '%' || ?1 OR path LIKE '%/' || ?1
         LIMIT 2",
    )?;

    let ids: Vec<i64> = statement
        .query_map(params![name], |row| row.get(0))?
        .collect::<std::result::Result<_, _>>()?;

    Ok(match ids.as_slice() {
        [only] => Some(*only),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_values_are_treated_as_missing() {
        assert_eq!(clean(Some("  ")), None);
        assert_eq!(clean(Some("Unknown")), None);
        assert_eq!(clean(Some("N/A")), None);
        assert_eq!(clean(Some(" Vellichor ")), Some("Vellichor".to_owned()));
    }

    /// Commas appear inside real genre names, so splitting on them would
    /// manufacture genres the user never had.
    #[test]
    fn genres_split_on_semicolons_and_slashes_only() {
        assert_eq!(split_genres("Rock"), vec!["Rock"]);
        assert_eq!(split_genres("Rock; Pop"), vec!["Rock", "Pop"]);
        assert_eq!(split_genres("Hip-Hop/Rap"), vec!["Hip-Hop", "Rap"]);
        assert_eq!(
            split_genres("Folk, World, & Country"),
            vec!["Folk, World, & Country"]
        );
    }

    #[test]
    fn years_are_read_out_of_any_date_format() {
        assert_eq!(year_from("1997"), Some(1997));
        assert_eq!(year_from("1997-05-21"), Some(1997));
        assert_eq!(year_from("1997-05-21T00:00:00"), Some(1997));
        assert_eq!(year_from(""), None);
        assert_eq!(year_from("nonsense"), None);
        assert_eq!(year_from("97"), None, "two digits is not a year we trust");
    }

    #[test]
    fn replaygain_values_lose_their_units() {
        assert_eq!(parse_gain_db("-7.25 dB"), Some(-7.25));
        assert_eq!(parse_gain_db("+3.5dB"), Some(3.5));
        assert_eq!(parse_gain_db("0.00 dB"), Some(0.0));
        assert_eq!(parse_gain_db("loud"), None);
    }

    #[test]
    fn a_zero_year_is_rejected_like_any_other_nonsense() {
        assert!(!is_plausible_year(&0));
        assert!(!is_plausible_year(&-1));
        assert!(is_plausible_year(&1997));
        assert!(is_plausible_year(&2024));
    }

    #[test]
    fn a_summary_with_no_changes_says_so() {
        let summary = Summary {
            unchanged: 400,
            ..Summary::default()
        };
        assert!(!summary.changed_anything());
        assert!(summary.describe().contains("up to date"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths_compare_case_insensitively() {
        assert_eq!(
            path_key(Path::new(r"D:\Music\Song.mp3")),
            path_key(Path::new(r"d:\music\SONG.MP3"))
        );
    }
}
