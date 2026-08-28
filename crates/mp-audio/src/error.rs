//! Errors surfaced by the audio engine.
//!
//! These are user-facing: a failure to play one track must never take the
//! player down, so every variant is something the UI can show in a toast and
//! then move on from.

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("no audio output device is available")]
    NoOutputDevice,

    #[error("the output device does not support any usable format")]
    NoUsableConfig,

    #[error("could not open the audio output: {0}")]
    DeviceInit(String),

    #[error("{path}: {reason}")]
    Unsupported { path: PathBuf, reason: String },

    #[error("could not read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("could not decode {path}: {source}")]
    Decode {
        path: PathBuf,
        #[source]
        source: symphonia::core::errors::Error,
    },

    #[error("{path} contains no audio track")]
    NoAudioTrack { path: PathBuf },

    #[error("could not set up resampling: {0}")]
    Resampler(String),
}

pub type Result<T> = std::result::Result<T, AudioError>;
