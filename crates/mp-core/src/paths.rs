//! Canonical on-disk locations for everything Resonance owns.
//!
//! Normally everything lives under the platform config/data dirs — on Windows
//! that is `%APPDATA%/Resonance`. Nothing is ever written next to the user's
//! music.
//!
//! ## Portable mode
//!
//! Dropping an empty file called `resonance.portable` beside the executable
//! moves the lot to `<exe folder>/Resonance-data`. That makes the app carryable
//! on a stick: settings, index, artwork and logs travel with it, and the host
//! machine is left exactly as it was found. The marker is a file the user
//! creates rather than a setting inside the app, because a setting stored in
//! the very directory it is choosing cannot be read before it is found.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;

const QUALIFIER: &str = "";
const ORGANISATION: &str = "";
const APPLICATION: &str = "Resonance";

/// Placed beside the executable to switch on portable mode.
pub const PORTABLE_MARKER: &str = "resonance.portable";

/// Folder created beside the executable in portable mode.
pub const PORTABLE_DIR: &str = "Resonance-data";

/// Resolved application directories.
#[derive(Debug, Clone)]
pub struct AppPaths {
    config_dir: PathBuf,
    data_dir: PathBuf,
    cache_dir: PathBuf,
}

impl AppPaths {
    /// Resolve where this installation should store things.
    ///
    /// Portable mode wins when its marker is present; otherwise the standard
    /// per-user directories are used. This is what the application calls;
    /// [`Self::discover`] and [`Self::rooted_at`] are the two halves of it.
    pub fn resolve() -> Result<Self> {
        match portable_root() {
            Some(root) => {
                tracing::info!("portable mode: storing everything in {}", root.display());
                Self::rooted_at(root)
            }
            None => Self::discover(),
        }
    }

    /// Whether this installation is running in portable mode.
    pub fn is_portable() -> bool {
        portable_root().is_some()
    }

    /// Resolve the standard per-user directories, creating them if needed.
    pub fn discover() -> Result<Self> {
        let dirs = ProjectDirs::from(QUALIFIER, ORGANISATION, APPLICATION)
            .context("could not determine a home directory for the current user")?;

        let paths = Self {
            config_dir: dirs.config_dir().to_path_buf(),
            data_dir: dirs.data_dir().to_path_buf(),
            cache_dir: dirs.cache_dir().to_path_buf(),
        };
        paths.ensure_dirs()?;
        Ok(paths)
    }

    /// Point every directory at one root. Used by tests and portable mode.
    pub fn rooted_at(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let paths = Self {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            cache_dir: root.join("cache"),
        };
        paths.ensure_dirs()?;
        Ok(paths)
    }

    fn ensure_dirs(&self) -> Result<()> {
        for dir in [&self.config_dir, &self.data_dir, &self.cache_dir] {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating directory {}", dir.display()))?;
        }
        std::fs::create_dir_all(self.art_cache_dir())?;
        std::fs::create_dir_all(self.log_dir())?;
        Ok(())
    }

    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// `config.toml` — user settings.
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    /// Written next to the config whenever a migration rewrites it.
    pub fn config_backup_file(&self, from_version: u32) -> PathBuf {
        self.config_dir
            .join(format!("config.v{from_version}.bak.toml"))
    }

    /// `library.db` — the SQLite track index.
    pub fn library_db(&self) -> PathBuf {
        self.data_dir.join("library.db")
    }

    /// Content-addressed cover art thumbnails.
    pub fn art_cache_dir(&self) -> PathBuf {
        self.cache_dir.join("art")
    }

    pub fn log_dir(&self) -> PathBuf {
        self.data_dir.join("logs")
    }
}

/// Where portable mode would store things, if it is switched on.
fn portable_root() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    portable_root_beside(exe.parent()?)
}

/// The portable root for a given executable directory.
///
/// Split out from [`portable_root`] so the rule can be tested against a real
/// directory without having to relocate the running executable.
pub fn portable_root_beside(exe_dir: &Path) -> Option<PathBuf> {
    exe_dir
        .join(PORTABLE_MARKER)
        .is_file()
        .then(|| exe_dir.join(PORTABLE_DIR))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "resonance-paths-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn without_the_marker_there_is_no_portable_root() {
        let dir = temp_dir("plain");
        assert_eq!(portable_root_beside(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_marker_moves_everything_beside_the_executable() {
        let dir = temp_dir("portable");
        std::fs::write(dir.join(PORTABLE_MARKER), b"").unwrap();

        let root = portable_root_beside(&dir).expect("the marker should switch it on");
        assert_eq!(root, dir.join(PORTABLE_DIR));

        // And every directory really does land under it, rather than the
        // marker being noticed and then ignored.
        let paths = AppPaths::rooted_at(&root).unwrap();
        for path in [
            paths.config_file(),
            paths.library_db(),
            paths.art_cache_dir(),
            paths.log_dir(),
        ] {
            assert!(
                path.starts_with(&root),
                "{} escaped the portable folder",
                path.display()
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A *directory* of that name is not the marker. The check is deliberately
    /// `is_file`, so someone who makes a folder by accident does not silently
    /// get a second, empty library.
    #[test]
    fn a_directory_named_like_the_marker_does_not_count() {
        let dir = temp_dir("marker-dir");
        std::fs::create_dir_all(dir.join(PORTABLE_MARKER)).unwrap();

        assert_eq!(portable_root_beside(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rooted_paths_are_all_under_their_root() {
        let dir = temp_dir("rooted");
        let paths = AppPaths::rooted_at(&dir).unwrap();

        assert!(paths.config_dir().starts_with(&dir));
        assert!(paths.data_dir().starts_with(&dir));
        assert!(paths.cache_dir().starts_with(&dir));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
