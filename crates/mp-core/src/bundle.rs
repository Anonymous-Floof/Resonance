//! `.mpbundle` — settings, playlists and listening history in one file.
//!
//! This is the answer to "I am moving to a new machine" and to "I am about to
//! change a lot of settings and would like a way back". It is an ordinary zip
//! with a `.mpbundle` extension, so it can be opened, inspected and repaired
//! with anything, and every member is plain text.
//!
//! ```text
//! manifest.json    what this bundle is and when it was made
//! config.toml      the whole settings file, verbatim
//! playlists.json   playlists, with their tracks stored as file paths
//! statistics.json  play counts and history (optional)
//! ```
//!
//! Two decisions worth stating.
//!
//! **Playlists travel as paths, not ids.** Track ids are local to one index
//! and mean nothing anywhere else. Paths mean something on any machine with
//! the same music, and where they do not, the import says which ones it could
//! not find rather than silently producing a shorter playlist.
//!
//! **Importing is idempotent.** Bringing the same bundle in twice leaves the
//! library exactly as it was after the first time: playlists are matched by
//! name, play counts take the higher of the two, and history entries are
//! matched on their timestamp. That matters because "did that work?" is
//! usually answered by doing it again.

use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::Config;
use crate::library::{Library, SmartRules, TrackId};

/// Marks the file as ours, so a wrong file chosen in the open dialog is
/// reported as such rather than half-applied.
const FORMAT: &str = "resonance-bundle";

/// The bundle layout's own version, separate from the app's.
const FORMAT_VERSION: u32 = 1;

pub const EXTENSION: &str = "mpbundle";

const MANIFEST: &str = "manifest.json";
const CONFIG: &str = "config.toml";
const PLAYLISTS: &str = "playlists.json";
const STATISTICS: &str = "statistics.json";

/// What a bundle says about itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub format: String,
    pub version: u32,
    /// The Resonance version that wrote it.
    pub app_version: String,
    /// Unix seconds.
    pub exported_at: i64,
    pub playlists: usize,
    pub has_statistics: bool,
}

/// A playlist as it travels between machines.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredPlaylist {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Present for a smart playlist; its tracks are then derived, not stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules: Option<SmartRules>,
    #[serde(default)]
    pub tracks: Vec<PathBuf>,
}

/// One track's listening record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredStats {
    pub path: PathBuf,
    pub play_count: u32,
    /// Unix seconds, one per play.
    #[serde(default)]
    pub plays: Vec<i64>,
}

/// What to put in a bundle.
#[derive(Debug, Clone, Copy)]
pub struct ExportOptions {
    pub include_playlists: bool,
    /// Play counts and history. Separate because it is the one part that is
    /// about *behaviour* rather than configuration, and some people would
    /// rather not carry it around.
    pub include_statistics: bool,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            include_playlists: true,
            include_statistics: false,
        }
    }
}

/// How an import treats what is already there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Settings are overwritten; playlists with a matching name are replaced.
    Replace,
    /// Settings are left alone; only playlists that do not already exist by
    /// name are added.
    Merge,
}

/// What an import did.
#[derive(Debug, Clone, Default)]
pub struct ImportSummary {
    pub settings_applied: bool,
    pub playlists_added: usize,
    pub playlists_replaced: usize,
    pub playlists_skipped: usize,
    pub tracks_matched: usize,
    /// Paths the index has never seen, usually an unscanned folder.
    pub tracks_missing: Vec<PathBuf>,
    pub statistics_applied: usize,
}

impl ImportSummary {
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();

        if self.settings_applied {
            parts.push("settings".to_owned());
        }

        let playlists = self.playlists_added + self.playlists_replaced;
        if playlists > 0 {
            parts.push(format!("{playlists} playlists"));
        }
        if self.statistics_applied > 0 {
            parts.push(format!("history for {} tracks", self.statistics_applied));
        }

        let head = match parts.as_slice() {
            [] => "Nothing to import".to_owned(),
            parts => format!("Imported {}", parts.join(", ")),
        };

        match self.tracks_missing.len() {
            0 => head,
            missing => format!("{head} — {missing} tracks are not in your library"),
        }
    }
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

/// Write a bundle.
pub fn export(
    destination: &Path,
    config: &Config,
    library: &Library,
    options: ExportOptions,
) -> Result<Manifest> {
    let playlists = if options.include_playlists {
        collect_playlists(library)?
    } else {
        Vec::new()
    };

    let statistics = if options.include_statistics {
        Some(collect_statistics(library)?)
    } else {
        None
    };

    let manifest = Manifest {
        format: FORMAT.to_owned(),
        version: FORMAT_VERSION,
        app_version: crate::APP_VERSION.to_owned(),
        exported_at: now_unix(),
        playlists: playlists.len(),
        has_statistics: statistics.is_some(),
    };

    // Written to a temporary beside the target and renamed into place, so an
    // interrupted export cannot leave a half-written bundle where the user
    // expects a good one.
    let temporary = destination.with_extension("mpbundle.part");

    let file = std::fs::File::create(&temporary)
        .with_context(|| format!("creating {}", temporary.display()))?;

    let result = write_members(file, &manifest, config, &playlists, statistics.as_deref());

    if let Err(err) = result {
        let _ = std::fs::remove_file(&temporary);
        return Err(err);
    }

    std::fs::rename(&temporary, destination)
        .with_context(|| format!("writing {}", destination.display()))?;

    Ok(manifest)
}

fn write_members<W: Write + Seek>(
    sink: W,
    manifest: &Manifest,
    config: &Config,
    playlists: &[StoredPlaylist],
    statistics: Option<&[StoredStats]>,
) -> Result<()> {
    let mut zip = zip::ZipWriter::new(sink);
    let options: zip::write::FileOptions<'_, ()> =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    zip.start_file(MANIFEST, options)?;
    zip.write_all(serde_json::to_string_pretty(manifest)?.as_bytes())?;

    zip.start_file(CONFIG, options)?;
    zip.write_all(config.to_toml()?.as_bytes())?;

    zip.start_file(PLAYLISTS, options)?;
    zip.write_all(serde_json::to_string_pretty(playlists)?.as_bytes())?;

    if let Some(statistics) = statistics {
        zip.start_file(STATISTICS, options)?;
        zip.write_all(serde_json::to_string_pretty(statistics)?.as_bytes())?;
    }

    zip.finish()?;
    Ok(())
}

fn collect_playlists(library: &Library) -> Result<Vec<StoredPlaylist>> {
    let mut out = Vec::new();

    for playlist in library.playlists()? {
        // A smart playlist's tracks are whatever its rules say, so storing
        // them would freeze a list that is meant to keep moving.
        let tracks = if playlist.rules.is_some() {
            Vec::new()
        } else {
            library
                .playlist_tracks(playlist.id)?
                .into_iter()
                .map(|track| track.path)
                .collect()
        };

        out.push(StoredPlaylist {
            name: playlist.name,
            description: playlist.description,
            rules: playlist.rules,
            tracks,
        });
    }

    Ok(out)
}

fn collect_statistics(library: &Library) -> Result<Vec<StoredStats>> {
    library.play_statistics()
}

// ---------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------

/// Read a bundle's manifest without changing anything.
///
/// What the confirmation step shows, so the user can see what they picked
/// before any of it is applied.
pub fn inspect(source: &Path) -> Result<Manifest> {
    let file =
        std::fs::File::open(source).with_context(|| format!("opening {}", source.display()))?;
    let mut zip = zip::ZipArchive::new(file)
        .with_context(|| format!("{} is not a readable bundle", source.display()))?;

    read_manifest(&mut zip)
}

fn read_manifest<R: Read + Seek>(zip: &mut zip::ZipArchive<R>) -> Result<Manifest> {
    let text = member(zip, MANIFEST)?
        .context("this file has no manifest, so it is not a Resonance bundle")?;

    let manifest: Manifest =
        serde_json::from_str(&text).context("this bundle's manifest is unreadable")?;

    if manifest.format != FORMAT {
        bail!("this is a {} file, not a Resonance bundle", manifest.format);
    }

    if manifest.version > FORMAT_VERSION {
        bail!(
            "this bundle was written by a newer version of Resonance \
             (format {}, this build understands {FORMAT_VERSION})",
            manifest.version
        );
    }

    Ok(manifest)
}

/// Apply a bundle.
pub fn import(
    source: &Path,
    config: &mut Config,
    library: &mut Library,
    mode: Mode,
) -> Result<ImportSummary> {
    let file =
        std::fs::File::open(source).with_context(|| format!("opening {}", source.display()))?;
    let mut zip = zip::ZipArchive::new(file)
        .with_context(|| format!("{} is not a readable bundle", source.display()))?;

    // Checked first, so a bundle from the future is refused before any of it
    // has been applied.
    read_manifest(&mut zip)?;

    let mut summary = ImportSummary::default();

    if mode == Mode::Replace
        && let Some(text) = member(&mut zip, CONFIG)?
    {
        let mut imported =
            Config::from_toml(&text).context("the bundle's settings are unreadable")?;
        imported.validate();
        *config = imported;
        summary.settings_applied = true;
    }

    if let Some(text) = member(&mut zip, PLAYLISTS)? {
        let playlists: Vec<StoredPlaylist> =
            serde_json::from_str(&text).context("the bundle's playlists are unreadable")?;
        apply_playlists(library, &playlists, mode, &mut summary)?;
    }

    if let Some(text) = member(&mut zip, STATISTICS)? {
        let statistics: Vec<StoredStats> =
            serde_json::from_str(&text).context("the bundle's statistics are unreadable")?;
        summary.statistics_applied = library.merge_play_statistics(&statistics)?;
    }

    Ok(summary)
}

fn apply_playlists(
    library: &mut Library,
    playlists: &[StoredPlaylist],
    mode: Mode,
    summary: &mut ImportSummary,
) -> Result<()> {
    let existing: Vec<(String, crate::library::PlaylistId)> = library
        .playlists()?
        .into_iter()
        .map(|playlist| (playlist.name, playlist.id))
        .collect();

    for stored in playlists {
        let already = existing
            .iter()
            .find(|(name, _)| name == &stored.name)
            .map(|(_, id)| *id);

        match (already, mode) {
            // Leave what is there alone. Merging is for bringing across what
            // is missing, not for overwriting work done since.
            (Some(_), Mode::Merge) => {
                summary.playlists_skipped += 1;
                continue;
            }
            (Some(id), Mode::Replace) => {
                library.delete_playlist(id)?;
                summary.playlists_replaced += 1;
            }
            (None, _) => summary.playlists_added += 1,
        }

        let id = match &stored.rules {
            Some(rules) => library.create_smart_playlist(&stored.name, rules)?,
            None => library.create_playlist(&stored.name)?,
        };

        if stored.rules.is_some() {
            continue;
        }

        let mut found: Vec<TrackId> = Vec::new();
        for path in &stored.tracks {
            match library.id_for_path(path)? {
                Some(track) => found.push(track),
                None => summary.tracks_missing.push(path.clone()),
            }
        }

        summary.tracks_matched += found.len();
        library.add_to_playlist(id, &found)?;
    }

    Ok(())
}

/// Read one member as text, or `None` when the bundle does not carry it.
fn member<R: Read + Seek>(zip: &mut zip::ZipArchive<R>, name: &str) -> Result<Option<String>> {
    let mut file = match zip.by_name(name) {
        Ok(file) => file,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("reading {name} from the bundle")),
    };

    let mut text = String::new();
    file.read_to_string(&mut text)
        .with_context(|| format!("reading {name} from the bundle"))?;

    Ok(Some(text))
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manifest_round_trips() {
        let manifest = Manifest {
            format: FORMAT.to_owned(),
            version: FORMAT_VERSION,
            app_version: "0.1.0".into(),
            exported_at: 1_700_000_000,
            playlists: 3,
            has_statistics: true,
        };

        let text = serde_json::to_string(&manifest).unwrap();
        let back: Manifest = serde_json::from_str(&text).unwrap();

        assert_eq!(back.format, manifest.format);
        assert_eq!(back.version, manifest.version);
        assert_eq!(back.playlists, 3);
        assert!(back.has_statistics);
    }

    #[test]
    fn a_stored_playlist_round_trips_with_and_without_rules() {
        let plain = StoredPlaylist {
            name: "Mixtape".into(),
            description: String::new(),
            rules: None,
            tracks: vec![PathBuf::from("C:/music/a.mp3")],
        };

        let text = serde_json::to_string(&plain).unwrap();
        assert!(
            !text.contains("rules"),
            "an ordinary playlist should not carry an empty rules field: {text}"
        );

        let back: StoredPlaylist = serde_json::from_str(&text).unwrap();
        assert_eq!(back.tracks.len(), 1);
        assert!(back.rules.is_none());

        let smart = StoredPlaylist {
            name: "Recent".into(),
            description: String::new(),
            rules: Some(SmartRules::default()),
            tracks: Vec::new(),
        };

        let back: StoredPlaylist =
            serde_json::from_str(&serde_json::to_string(&smart).unwrap()).unwrap();
        assert!(back.rules.is_some());
    }

    #[test]
    fn the_summary_says_what_happened() {
        let summary = ImportSummary {
            settings_applied: true,
            playlists_added: 2,
            tracks_matched: 40,
            ..ImportSummary::default()
        };
        let text = summary.summary();
        assert!(text.contains("settings"), "{text}");
        assert!(text.contains("2 playlists"), "{text}");

        let with_missing = ImportSummary {
            playlists_added: 1,
            tracks_missing: vec![PathBuf::from("a"), PathBuf::from("b")],
            ..ImportSummary::default()
        };
        assert!(
            with_missing
                .summary()
                .contains("2 tracks are not in your library"),
            "{}",
            with_missing.summary()
        );

        assert_eq!(ImportSummary::default().summary(), "Nothing to import");
    }

    #[test]
    fn statistics_round_trip() {
        let stats = vec![StoredStats {
            path: PathBuf::from("C:/music/a.mp3"),
            play_count: 12,
            plays: vec![1_700_000_000, 1_700_000_100],
        }];

        let back: Vec<StoredStats> =
            serde_json::from_str(&serde_json::to_string(&stats).unwrap()).unwrap();

        assert_eq!(back.len(), 1);
        assert_eq!(back[0].play_count, 12);
        assert_eq!(back[0].plays.len(), 2);
    }
}
