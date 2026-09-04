//! Everything Resonance sends to the internet, and the record of having sent
//! it.
//!
//! # There is no transport here yet
//!
//! This crate contains no HTTP client, and neither does the workspace. Nothing
//! in it can make a request, and the application binary does not depend on it
//! at all. That is why `main`'s offline claims are still literally true at
//! this commit, and it is a fact worth checking rather than believing:
//!
//! ```text
//! cargo tree --workspace | grep -iE "reqwest|hyper|ureq|curl|tls"
//! ```
//!
//! What is here is the machinery that has to exist *before* the first request,
//! not after it:
//!
//! - [`Activity`] — the disclosure log. Every request, in a plain text file
//!   the user can read without asking the application to summarise itself.
//! - [`Source`] — who a service is, what it gives, what leaves the machine,
//!   and how often it may be asked. [`SOURCES`] lists the ones this build can
//!   reach, and is currently empty.
//! - [`Limiter`] — a floor on the gap between requests and exponential backoff
//!   on failure.
//!
//! Each was written first on purpose. A log added after the fetchers is a log
//! with gaps in it; a rate limiter added afterwards is one that was missing
//! while the fetcher was being tested against a live service.
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
//! - No other crate in the workspace may take an HTTP dependency.
//! - Nothing here may be called from the audio thread, or from anywhere a UI
//!   frame waits on it. Network work is background enrichment that can fail
//!   silently and be retried; playback never waits for it.
//!
//! # What is deliberately not here yet
//!
//! A response cache, an error type, and the User-Agent string identifying the
//! application to the services it calls. All three are real needs, and all
//! three are shaped by what is actually being fetched — bytes of a JPEG cache
//! very differently from a JSON document, an error enum whose variants nothing
//! constructs is the same mistake as a setting nothing reads, and a
//! User-Agent is a claim about a request that does not exist. They arrive with
//! the first fetcher.

pub mod activity;
pub mod rate;
pub mod source;
pub mod timestamp;

pub use activity::{Activity, Entry, LOG_FILE_NAME, Outcome};
pub use rate::Limiter;
pub use source::{SOURCES, Source};
