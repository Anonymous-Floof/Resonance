//! User settings: strongly typed, TOML-backed, and forward/backward tolerant.
//!
//! Every struct is `#[serde(default)]` so a config written by an older build
//! still loads, and unknown keys from a newer build are ignored rather than
//! rejected. `schema_version` exists for changes that defaults cannot express
//! (renames, semantic changes); see [`migrate`].

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths::AppPaths;

/// Bump when a change cannot be handled by serde defaults alone.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// Number of graphic-EQ bands. The frequencies themselves live in `mp-audio`.
pub const EQ_BAND_COUNT: usize = 10;

// ---------------------------------------------------------------------------
// Root
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub schema_version: u32,
    pub appearance: Appearance,
    pub playback: Playback,
    pub library: Library,
    pub equalizer: Equalizer,
    pub visualizer: Visualizer,
    pub privacy: Privacy,
    pub window: Window,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            appearance: Appearance::default(),
            playback: Playback::default(),
            library: Library::default(),
            equalizer: Equalizer::default(),
            visualizer: Visualizer::default(),
            privacy: Privacy::default(),
            window: Window::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Appearance
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    Dark,
    Light,
    /// Dark shell, accent colour pulled from the current album art.
    Adaptive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Density {
    Compact,
    Comfortable,
}

/// What a panel takes its background from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceStyle {
    /// The theme's own colour, and nothing else.
    Solid,
    /// A soft wash of the current cover's colours.
    AlbumArt,
    /// The visualiser, well behind the content.
    Visualizer,
}

impl SurfaceStyle {
    pub const ALL: [Self; 3] = [Self::Solid, Self::AlbumArt, Self::Visualizer];

    pub fn label(self) -> &'static str {
        match self {
            Self::Solid => "Plain",
            Self::AlbumArt => "Album art",
            Self::Visualizer => "Visualizer",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Solid => "The theme colour on its own",
            Self::AlbumArt => "A blurred wash of the current cover",
            Self::Visualizer => "The visualizer, dimmed and behind everything",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
    pub theme: ThemeMode,
    pub density: Density,
    /// Accent used for Dark/Light, and as the fallback for Adaptive. `#RRGGBB`.
    pub accent: String,
    /// Global UI scale multiplier applied on top of the OS DPI setting.
    pub ui_scale: f32,
    /// Preferred UI font family names, tried in order before egui's built-in.
    pub font_candidates: Vec<String>,
    /// Windows 11 Mica / acrylic window backdrop.
    pub mica_backdrop: bool,

    /// Background treatment for the main content area.
    pub content_background: SurfaceStyle,
    /// Background treatment for the player bar along the bottom.
    pub player_background: SurfaceStyle,
    /// How strongly those treatments show, 0.0..=1.0.
    ///
    /// Scaled into a range that always leaves the panel readable, so even the
    /// maximum is a background rather than a picture with text on it.
    pub background_intensity: f32,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            theme: ThemeMode::Dark,
            density: Density::Comfortable,
            accent: "#7C5CFF".to_owned(),
            ui_scale: 1.0,
            font_candidates: vec![
                "Segoe UI Variable Text".to_owned(),
                "Segoe UI".to_owned(),
                "Inter".to_owned(),
            ],
            mica_backdrop: true,
            content_background: SurfaceStyle::Solid,
            player_background: SurfaceStyle::Solid,
            background_intensity: 0.6,
        }
    }
}

// ---------------------------------------------------------------------------
// Playback
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShuffleMode {
    Off,
    /// Uniformly random.
    Random,
    /// Random, but spaces out the same artist and avoids recent repeats.
    Smart,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepeatMode {
    Off,
    All,
    One,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayGainMode {
    Off,
    Track,
    Album,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrossfadeCurve {
    Linear,
    /// Constant-power; keeps perceived loudness steady through the blend.
    EqualPower,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Playback {
    pub shuffle: ShuffleMode,
    pub repeat: RepeatMode,
    /// Master volume, 0.0..=1.0 on a perceptual (dB-mapped) scale.
    pub volume: f32,
    pub muted: bool,
    /// -1.0 = full left, 0.0 = centre, 1.0 = full right.
    pub gapless: bool,
    pub crossfade_seconds: f32,
    pub crossfade_curve: CrossfadeCurve,
    pub replay_gain: ReplayGainMode,
    /// Applied when a track has no ReplayGain tags, in dB.
    pub replay_gain_fallback_db: f32,
    /// Skip leading/trailing digital silence.
    pub trim_silence: bool,
    /// `None` = system default device.
    pub output_device: Option<String>,
    /// Requested output buffer size in frames. `None` = let cpal decide.
    pub buffer_frames: Option<u32>,
    /// Start a track on one click rather than two.
    ///
    /// Off by default, because a double-click is what a list of files does
    /// everywhere else on the system and a single click is how you select one.
    pub play_on_single_click: bool,
    /// When the queue empties, keep going with similar tracks.
    pub auto_radio: bool,
    /// How many tracks auto-radio adds each time it tops the queue up.
    pub radio_batch: usize,
}

impl Default for Playback {
    fn default() -> Self {
        Self {
            shuffle: ShuffleMode::Off,
            repeat: RepeatMode::Off,
            // Deliberately quiet. A player that opens at 70% is the one
            // that makes someone lunge for the volume key the first time they
            // press play, and turning it up is a cheaper mistake to fix.
            volume: 0.41,
            muted: false,
            gapless: true,
            crossfade_seconds: 0.0,
            crossfade_curve: CrossfadeCurve::EqualPower,
            replay_gain: ReplayGainMode::Off,
            replay_gain_fallback_db: 0.0,
            trim_silence: false,
            output_device: None,
            buffer_frames: None,
            play_on_single_click: false,
            auto_radio: false,
            radio_batch: 10,
        }
    }
}

// ---------------------------------------------------------------------------
// Library
// ---------------------------------------------------------------------------

/// The library section Resonance opens on.
///
/// Named for the views that exist rather than for ways of grouping in the
/// abstract. An earlier version offered `Year`, which no view has ever been
/// able to show, and split `Artist` from `AlbumArtist` when both land in the
/// same place — a setting that offers a destination the app cannot reach is
/// not a setting, it is a promise it breaks.
///
/// The aliases keep configs written by those builds loading: without them an
/// unrecognised value fails the whole `[library]` section, which would take
/// the user's watched folders down with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Grouping {
    #[serde(alias = "year")]
    Songs,
    #[serde(alias = "artist", alias = "album_artist")]
    Artists,
    #[serde(alias = "album")]
    Albums,
    #[serde(alias = "genre")]
    Genres,
    #[serde(alias = "folder")]
    Folders,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortKey {
    Title,
    Artist,
    Album,
    Year,
    Duration,
    DateAdded,
    PlayCount,
    LastPlayed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Library {
    /// Folders scanned recursively.
    pub watched_folders: Vec<PathBuf>,
    /// Re-scan watched folders on launch.
    pub scan_on_startup: bool,
    /// Keep the library live via a filesystem watcher.
    pub watch_for_changes: bool,
    pub default_grouping: Grouping,
    pub default_sort: SortKey,
    pub sort_descending: bool,
    /// Sort "The Wandering Hours" under B.
    pub ignore_leading_articles: bool,
    /// Files shorter than this are skipped (strips interstitials). Seconds.
    pub min_track_seconds: u32,
    /// Extensions considered for import, lowercase and without the dot.
    pub extensions: Vec<String>,
    /// Enables the tag editor. Off by default - writes to your actual files.
    pub allow_tag_editing: bool,
    /// Measure what each track sounds like, in the background.
    ///
    /// Decodes a slice of every file once — minutes of work on a large
    /// library, resumable, and never on the path of anything the user is
    /// waiting for. Suggestions work without it; this is what makes them work
    /// on files with no useful tags.
    pub analyze_audio_features: bool,
}

impl Default for Library {
    fn default() -> Self {
        Self {
            watched_folders: Vec::new(),
            scan_on_startup: true,
            watch_for_changes: true,
            // Artists rather than a flat song list: a collection large
            // enough to need a player is one where an unbroken A-to-Z of every
            // track is the least useful thing to open on.
            default_grouping: Grouping::Artists,
            default_sort: SortKey::Title,
            sort_descending: false,
            ignore_leading_articles: true,
            min_track_seconds: 5,
            // Sorted, because `validate` sorts them: an unsorted list here
            // would mean the defaults change the first time they are loaded.
            extensions: [
                "aac", "aif", "aiff", "alac", "ape", "flac", "m4a", "mp3", "mp4", "oga", "ogg",
                "opus", "wav", "wma", "wv",
            ]
            .iter()
            .map(|s| (*s).to_owned())
            .collect(),
            allow_tag_editing: false,
            // On, because the similar-tracks and auto-radio features are
            // inert without it. It genuinely decodes every track in the
            // background on first run, which is the cost of those features
            // working at all rather than sitting there greyed out.
            analyze_audio_features: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Equalizer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Equalizer {
    pub enabled: bool,
    /// Per-band gain in dB, clamped to +/- [`Equalizer::MAX_GAIN_DB`].
    pub gains_db: Vec<f32>,
    /// Applied before the filter bank, in dB.
    pub preamp_db: f32,
    /// Soft-knee limiter guarding against clipping from boosted bands.
    pub limiter: bool,
    /// Name of the currently selected preset, for UI display only.
    pub preset: Option<String>,
}

impl Equalizer {
    pub const MAX_GAIN_DB: f32 = 12.0;
    pub const MAX_PREAMP_DB: f32 = 12.0;
}

impl Default for Equalizer {
    fn default() -> Self {
        // The "Rock" preset from `mp_audio::dsp::presets`, which cannot be
        // named from here: `mp-audio` depends on this crate, not the other way
        // round. A test over there asserts the two still agree.
        Self {
            enabled: true,
            gains_db: vec![4.0, 3.0, 1.0, -0.5, -1.5, 0.0, 1.5, 3.0, 3.5, 3.0],
            preamp_db: -4.5,
            limiter: true,
            preset: Some("Rock".to_owned()),
        }
    }
}

// ---------------------------------------------------------------------------
// Visualizer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VisualizerKind {
    None,
    SpectrumBars,
    Oscilloscope,
    RadialSpectrum,
    WaveformRibbon,
    AuroraBloom,
    ParticleField,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VizColorMode {
    Accent,
    AlbumArt,
    Spectrum,
    Custom,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Visualizer {
    pub kind: VisualizerKind,
    pub color_mode: VizColorMode,
    pub custom_color: String,
    /// Input gain into the visualiser, 0.1..=4.0.
    pub sensitivity: f32,
    /// Temporal smoothing, 0.0 (instant) ..= 0.95 (very smooth).
    pub smoothing: f32,
    pub bar_count: usize,
    pub show_peak_caps: bool,
    pub fps_cap: u32,
    /// Drop to 30fps when the window is not focused.
    pub low_power_when_unfocused: bool,
}

impl Default for Visualizer {
    fn default() -> Self {
        Self {
            kind: VisualizerKind::AuroraBloom,
            color_mode: VizColorMode::AlbumArt,
            custom_color: "#7C5CFF".to_owned(),
            sensitivity: 1.0,
            smoothing: 0.72,
            bar_count: 64,
            show_peak_caps: true,
            fps_cap: 60,
            low_power_when_unfocused: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Privacy
// ---------------------------------------------------------------------------

/// What Resonance records about you, which is the whole of the subject.
///
/// There is deliberately nothing here about network access. Resonance has no
/// HTTP client and makes no requests, so a setting offering to turn networking
/// on would be a control over something that does not exist. An earlier
/// version carried exactly that — switches for MusicBrainz, Last.fm and
/// artwork fetching that nothing ever read — and a settings screen full of
/// decoration is worse than one that is simply short.
///
/// Artwork comes from embedded tags and sidecar files already on disk;
/// suggestions come from the offline analysis pass. Neither needs permission
/// to reach anything.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Privacy {
    /// Record play counts and listening history locally.
    pub track_play_history: bool,
    /// Include play counts and history when exporting a settings bundle.
    ///
    /// Off by default. A bundle is often shared or stored somewhere less
    /// private than the machine it came from, and what you listened to and
    /// when is the most personal thing in this app.
    pub bundle_statistics: bool,
}

impl Default for Privacy {
    fn default() -> Self {
        Self {
            track_play_history: true,
            bundle_statistics: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Window state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Window {
    pub width: f32,
    pub height: f32,
    pub maximized: bool,
    pub nav_collapsed: bool,
    pub queue_panel_open: bool,
}

impl Default for Window {
    fn default() -> Self {
        Self {
            // The size the window restores to when it is un-maximised, so
            // it wants to be comfortable rather than minimal.
            width: 1500.0,
            height: 900.0,
            maximized: true,
            nav_collapsed: false,
            queue_panel_open: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Load / save / validate
// ---------------------------------------------------------------------------

impl Config {
    /// Read the config, falling back to defaults if it is missing.
    ///
    /// A corrupt config is preserved as `config.corrupt.toml` rather than
    /// silently overwritten - losing a user's settings without a trace is worse
    /// than starting from defaults.
    pub fn load(paths: &AppPaths) -> Result<Self> {
        let file = paths.config_file();
        if !file.exists() {
            tracing::info!("no config found, writing defaults to {}", file.display());
            let config = Self::default();
            config.save(paths)?;
            return Ok(config);
        }

        let text = std::fs::read_to_string(&file)
            .with_context(|| format!("reading {}", file.display()))?;

        match toml::from_str::<Self>(&text) {
            Ok(mut config) => {
                let from = config.schema_version;
                if from != CURRENT_SCHEMA_VERSION {
                    let backup = paths.config_backup_file(from);
                    let _ = std::fs::write(&backup, &text);
                    tracing::info!(
                        "migrating config v{} -> v{} (backup: {})",
                        from,
                        CURRENT_SCHEMA_VERSION,
                        backup.display()
                    );
                    migrate(&mut config, from);
                    config.schema_version = CURRENT_SCHEMA_VERSION;
                }
                config.validate();
                Ok(config)
            }
            Err(err) => {
                let quarantine = paths.config_dir().join("config.corrupt.toml");
                let _ = std::fs::write(&quarantine, &text);
                tracing::error!(
                    "config is unreadable ({}); kept a copy at {} and continuing with defaults",
                    err,
                    quarantine.display()
                );
                Ok(Self::default())
            }
        }
    }

    /// The settings as TOML, exactly as they would be written to disk.
    ///
    /// Shared with [`save`](Self::save) so a settings bundle carries the same
    /// text the config file would, rather than a second rendering of it that
    /// could drift.
    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).context("serialising config")
    }

    /// Parse settings from TOML.
    pub fn from_toml(text: &str) -> Result<Self> {
        toml::from_str(text).context("parsing config")
    }

    /// Write atomically - a crash mid-write must not truncate the config.
    pub fn save(&self, paths: &AppPaths) -> Result<()> {
        let file = paths.config_file();
        let tmp = file.with_extension("toml.tmp");
        let text = self.to_toml()?;

        std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &file).with_context(|| format!("replacing {}", file.display()))?;
        Ok(())
    }

    /// Clamp everything into a sane range. Hand-edited configs land here too.
    pub fn validate(&mut self) {
        let a = &mut self.appearance;
        a.ui_scale = a.ui_scale.clamp(0.6, 2.5);
        a.background_intensity = a.background_intensity.clamp(0.0, 1.0);

        let p = &mut self.playback;
        p.volume = p.volume.clamp(0.0, 1.0);
        p.crossfade_seconds = p.crossfade_seconds.clamp(0.0, 12.0);
        p.replay_gain_fallback_db = p.replay_gain_fallback_db.clamp(-24.0, 24.0);

        let e = &mut self.equalizer;
        e.gains_db.resize(EQ_BAND_COUNT, 0.0);
        for g in &mut e.gains_db {
            *g = g.clamp(-Equalizer::MAX_GAIN_DB, Equalizer::MAX_GAIN_DB);
        }
        e.preamp_db = e
            .preamp_db
            .clamp(-Equalizer::MAX_PREAMP_DB, Equalizer::MAX_PREAMP_DB);

        let v = &mut self.visualizer;
        v.sensitivity = v.sensitivity.clamp(0.1, 4.0);
        v.smoothing = v.smoothing.clamp(0.0, 0.95);
        v.bar_count = v.bar_count.clamp(8, 256);
        v.fps_cap = v.fps_cap.clamp(15, 240);

        let l = &mut self.library;
        for ext in &mut l.extensions {
            *ext = ext.trim_start_matches('.').to_ascii_lowercase();
        }
        l.extensions.retain(|e| !e.is_empty());
        l.extensions.sort();
        l.extensions.dedup();

        let w = &mut self.window;
        w.width = w.width.clamp(640.0, 10_000.0);
        w.height = w.height.clamp(480.0, 10_000.0);
    }

    /// Whether a path looks like something we should try to import.
    pub fn is_supported_audio(&self, path: &std::path::Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .is_some_and(|e| self.library.extensions.contains(&e))
    }
}

/// Apply changes that serde defaults cannot express.
///
/// v0 was pre-release and had no `schema_version` key at all, so serde gives it
/// 0. Every field it could contain has a default, so there is nothing to do
/// yet - this exists so the call site is already wired up when it is needed.
fn migrate(config: &mut Config, from_version: u32) {
    let _ = (config, from_version);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_paths(tag: &str) -> AppPaths {
        let dir =
            std::env::temp_dir().join(format!("resonance-test-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        AppPaths::rooted_at(dir).expect("temp paths")
    }

    #[test]
    fn defaults_round_trip_through_toml() {
        let original = Config::default();
        let text = toml::to_string_pretty(&original).expect("serialise");
        let parsed: Config = toml::from_str(&text).expect("deserialise");

        assert_eq!(parsed.schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(parsed.equalizer.gains_db.len(), EQ_BAND_COUNT);
        assert_eq!(parsed.appearance.accent, original.appearance.accent);
        assert_eq!(parsed.playback.volume, original.playback.volume);
    }

    #[test]
    fn the_shipped_defaults_name_nobody_in_particular() {
        // The defaults were lifted from a real configuration, which is a good
        // way to get sensible values and a very good way to accidentally ship
        // somebody's home directory. Everything machine-specific has to stay
        // empty.
        let config = Config::default();
        assert!(
            config.library.watched_folders.is_empty(),
            "a watched folder was baked into the defaults: {:?}",
            config.library.watched_folders
        );
        assert!(config.playback.output_device.is_none());
        assert!(config.playback.buffer_frames.is_none());
    }

    #[test]
    fn the_shipped_defaults_survive_validation_unchanged() {
        // A default outside its own clamp would be silently rewritten on first
        // load, which makes the value in the source a lie.
        let mut config = Config::default();
        let before = config.to_toml().expect("serialising defaults");
        config.validate();
        let after = config.to_toml().expect("serialising validated defaults");
        assert_eq!(before, after);
    }

    #[test]
    fn missing_config_is_created_with_defaults() {
        let paths = temp_paths("missing");
        assert!(!paths.config_file().exists());

        let config = Config::load(&paths).expect("load");
        assert_eq!(config.schema_version, CURRENT_SCHEMA_VERSION);
        assert!(paths.config_file().exists(), "load should persist defaults");
    }

    #[test]
    fn partial_config_fills_in_defaults() {
        let paths = temp_paths("partial");
        std::fs::write(
            paths.config_file(),
            "schema_version = 1\n[playback]\nvolume = 0.25\n",
        )
        .expect("write");

        let config = Config::load(&paths).expect("load");
        assert_eq!(config.playback.volume, 0.25);
        // Untouched groups still get their defaults.
        assert_eq!(config.appearance.theme, ThemeMode::Dark);
        assert_eq!(config.equalizer.gains_db.len(), EQ_BAND_COUNT);
    }

    #[test]
    fn corrupt_config_is_quarantined_not_lost() {
        let paths = temp_paths("corrupt");
        std::fs::write(paths.config_file(), "this is not valid toml {{{").expect("write");

        let config = Config::load(&paths).expect("load falls back");
        assert_eq!(config.playback.volume, Playback::default().volume);

        let quarantine = paths.config_dir().join("config.corrupt.toml");
        assert!(quarantine.exists(), "original must be preserved");
    }

    #[test]
    fn validate_clamps_out_of_range_values() {
        let mut config = Config::default();
        config.playback.volume = 4.0;
        config.equalizer.gains_db = vec![99.0, -99.0]; // also too short
        config.visualizer.bar_count = 4;
        config.appearance.ui_scale = 17.0;

        config.validate();

        assert_eq!(config.playback.volume, 1.0);
        assert_eq!(config.equalizer.gains_db.len(), EQ_BAND_COUNT);
        assert_eq!(config.equalizer.gains_db[0], Equalizer::MAX_GAIN_DB);
        assert_eq!(config.equalizer.gains_db[1], -Equalizer::MAX_GAIN_DB);
        assert_eq!(config.equalizer.gains_db[2], 0.0);
        assert_eq!(config.visualizer.bar_count, 8);
        assert_eq!(config.appearance.ui_scale, 2.5);
    }

    #[test]
    fn a_config_from_an_older_build_still_loads_its_library_section() {
        // The groupings an older build offered are gone. Without the aliases
        // an unrecognised value fails the whole `[library]` table, and the
        // user would silently lose their watched folders along with it.
        for (written, expected) in [
            ("artist", Grouping::Artists),
            ("album_artist", Grouping::Artists),
            ("album", Grouping::Albums),
            ("genre", Grouping::Genres),
            ("folder", Grouping::Folders),
            ("year", Grouping::Songs),
            ("songs", Grouping::Songs),
            ("folders", Grouping::Folders),
        ] {
            let text = format!(
                "[library]
watched_folders = [\"/music\"]
default_grouping = \"{written}\"
"
            );
            let parsed: Config = toml::from_str(&text)
                .unwrap_or_else(|err| panic!("{written} should still parse: {err}"));

            assert_eq!(parsed.library.default_grouping, expected, "{written}");
            assert_eq!(
                parsed.library.watched_folders.len(),
                1,
                "{written} took the watched folders down with it"
            );
        }
    }

    #[test]
    fn extensions_are_normalised() {
        let mut config = Config::default();
        config.library.extensions = vec![".MP3".into(), "FLAC".into(), "mp3".into(), "".into()];
        config.validate();
        assert_eq!(config.library.extensions, vec!["flac", "mp3"]);
    }

    #[test]
    fn supported_audio_detection_is_case_insensitive() {
        let config = Config::default();
        assert!(config.is_supported_audio(std::path::Path::new("a/b/Song.FLAC")));
        assert!(config.is_supported_audio(std::path::Path::new("Song.mp3")));
        assert!(!config.is_supported_audio(std::path::Path::new("cover.jpg")));
        assert!(!config.is_supported_audio(std::path::Path::new("no-extension")));
    }

    #[test]
    fn save_then_load_preserves_changes() {
        let paths = temp_paths("roundtrip");
        let mut config = Config::default();
        config.playback.shuffle = ShuffleMode::Smart;
        config.appearance.theme = ThemeMode::Adaptive;
        config.library.watched_folders = vec![PathBuf::from("D:/Music")];
        config.save(&paths).expect("save");

        let loaded = Config::load(&paths).expect("load");
        assert_eq!(loaded.playback.shuffle, ShuffleMode::Smart);
        assert_eq!(loaded.appearance.theme, ThemeMode::Adaptive);
        assert_eq!(loaded.library.watched_folders.len(), 1);
    }
}
