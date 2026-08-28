//! Decode, resample and output for Resonance.
//!
//! The engine runs itself: [`AudioEngine::new`] opens the output device and
//! spawns a worker thread, after which the UI only sends [`Command`]s and reads
//! lock-free [`Shared`] state. Nothing in this crate depends on the UI, so it
//! can be driven from a test without a window.

pub mod analysis;
pub mod decode;
pub mod device;
pub mod dsp;
pub mod engine;
pub mod error;
pub mod gapless;
pub mod queue;
pub mod resample;
pub mod scan;
pub mod shared;
pub mod viz;

/// Format classification lives in `mp-core` so the library index can reuse it
/// without pulling in a decoder; re-exported here because callers think of it
/// as an audio-engine concern.
pub use mp_core::format;

pub use device::DeviceInfo;
pub use engine::{AudioEngine, Command, Event};
pub use error::{AudioError, Result};
pub use format::{SUPPORTED_EXTENSIONS, Support};
pub use queue::Queue;
pub use scan::{ScanResult, scan};
pub use shared::{Shared, Status};
pub use viz::{Analyzer, Frame as VizFrame, Monitor as VizMonitor};
