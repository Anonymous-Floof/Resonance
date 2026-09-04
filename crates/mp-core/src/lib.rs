//! Core domain layer for Resonance: settings, on-disk paths, and (from M2) the
//! library index, playlists and similarity engine.
//!
//! This crate is deliberately free of any UI or audio-backend types so it can
//! be unit tested without a window or a sound device.

pub mod bundle;
pub mod color;
pub mod config;
pub mod format;
pub mod library;
pub mod paths;

pub use config::Config;
pub use format::{SUPPORTED_EXTENSIONS, Support};

pub use paths::AppPaths;

/// Product name, used for filenames and the config directory.
///
/// Deliberately unchanged on this branch. It decides where settings live, so
/// making it say "networked" would give the two builds separate libraries and
/// separate settings — which is not what distinguishing them should cost.
pub const APP_NAME: &str = "Resonance";

/// The name shown in the window's title bar.
///
/// This is where the two builds are told apart. `main` promises it has no
/// network client at all; this one can fetch lyrics. Someone who downloaded a
/// binary six months ago is entitled to know which of those they are looking
/// at without opening the settings page, so the title bar says it outright.
pub const APP_TITLE: &str = "Resonance (networked)";

/// Version reported in the about screen and written into exported bundles.
///
/// Carries the `-networked` suffix from the workspace manifest, so a log line
/// or an exported bundle identifies the build that produced it.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
