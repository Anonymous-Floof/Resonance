//! The signal chain that runs inside the audio callback.
//!
//! Everything here obeys one rule: the callback must be real-time safe. No
//! allocation, no locks, no I/O, no `sin`/`cos`/`powf`. Anything that needs
//! those runs on the control thread and is shipped over as plain data.
//!
//! The chain, in order:
//!
//! 1. **ReplayGain / preamp** — level correction from the track's own tags.
//! 2. **Equalizer** — ten cascaded biquads per channel.
//! 3. **Limiter** — catches the clipping that boosted bands would otherwise
//!    cause. On by default, because +12 dB on four bands clips almost any
//!    material and silent distortion is a bad default.
//! 4. **Volume**, smoothed, then the **fade envelope** that keeps pause, seek
//!    and track changes from clicking.

pub mod biquad;
pub mod chain;
pub mod eq;
pub mod limiter;
pub mod presets;

pub use biquad::{Coefficients, State};
pub use chain::{Chain, Params};
pub use eq::{BAND_COUNT, BAND_FREQUENCIES, Bank, BankState, db_to_linear, linear_to_db};
pub use limiter::{Limiter, Settings as LimiterSettings};
pub use presets::Preset;
