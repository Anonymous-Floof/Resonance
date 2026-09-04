//! Everything Resonance sends to the internet, and the record of having sent
//! it.
//!
//! # The rule this crate exists to enforce
//!
//! **Every outbound request goes through here.** Not most of them. If a
//! request can be made from `mp-core`, `mp-audio` or `mp-ui`, then no amount
//! of reading this crate tells anyone what the application talks to, and the
//! activity log becomes a partial record — which is worse than none, because
//! it looks complete.
//!
//! Two consequences follow, and both are load-bearing:
//!
//! - No other crate in the workspace may take an HTTP dependency. `ureq`
//!   appears exactly once in `cargo tree`, under this crate.
//! - Nothing here may be called from the audio thread, or from anywhere a UI
//!   frame waits on it. Network work is background enrichment that can fail
//!   silently and be retried; playback never waits for it.
//!
//! # What is here
//!
//! - [`Activity`] — the disclosure log. Every request, in a plain text file
//!   the user can read without asking the application to summarise itself.
//! - [`Source`] — who a service is, what it gives, what leaves the machine,
//!   and how often it may be asked. [`SOURCES`] is the complete list this
//!   build can reach.
//! - [`Limiter`] — a floor on the gap between requests and exponential backoff
//!   on failure.
//! - [`http`] — the transport, and the [`Transport`](http::Transport) seam
//!   that keeps everything above it testable without a network.
//! - [`cache`] — answers kept on disk, misses included.
//! - [`lyrics`] — the first fetcher: LRCLIB, for words the audio file does not
//!   carry.
//!
//! The first three were built before any of the others, deliberately. A log
//! added after the fetchers is a log with gaps in it, and a rate limiter added
//! afterwards is one that was missing while the fetcher was being tested
//! against a live service.
//!
//! # Nothing happens unless it is switched on
//!
//! Every fetcher is off until the user turns it on, and the setting is stored
//! outside this crate — `mp-net` has no opinion about consent, it just cannot
//! be reached without it. A build that has never been switched on makes no
//! requests at all, and the log is empty because nothing happened rather than
//! because nothing was recorded.
//!
//! # Testing
//!
//! The whole workspace suite runs offline. Fetchers talk to
//! [`Transport`](http::Transport) rather than to [`Http`](http::Http), so
//! misses, rate limits, dead servers and garbage responses are all tested
//! against a scripted fake. Nothing in `cargo test` opens a socket.

pub mod activity;
pub mod artwork;
pub mod cache;
pub mod error;
pub mod http;
pub mod lyrics;
pub mod rate;
pub mod source;
pub mod timestamp;

pub use activity::{Activity, Entry, LOG_FILE_NAME, Outcome};
pub use cache::Cache;
pub use error::NetError;
pub use http::{Http, Transport};
pub use rate::Limiter;
pub use source::{SOURCES, Source};
