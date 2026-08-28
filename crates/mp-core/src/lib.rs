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

/// Product name, used for window titles and the config directory.
pub const APP_NAME: &str = "Resonance";

/// Version reported in the about screen and written into exported bundles.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
