//! The transport. The only place in Resonance that opens a socket.
//!
//! Deliberately tiny: one blocking GET, a timeout, and a `User-Agent`. There
//! is no connection pooling to tune, no async runtime, and no middleware,
//! because the workload is one small request per track on a background thread
//! and nothing here is ever on a path the user waits for.
//!
//! `ureq` rather than `reqwest` for that reason. The rest of this codebase is
//! threads and channels with no async anywhere, and pulling in a full async
//! runtime to make a handful of blocking requests would add far more to
//! `cargo tree` than it earns — on a branch whose entire claim is that
//! `cargo tree` can be read and understood.
//!
//! ## Requests go through [`Transport`], not through here directly
//!
//! Everything above this module talks to the [`Transport`] trait, so the
//! fetchers can be tested exhaustively — misses, rate limits, garbage
//! responses, dead servers — without a network, and so the whole test suite
//! stays headless and offline. [`Http`] is the one implementation that
//! actually reaches out.

use std::time::Duration;

// Carries `get_uri`, which reports where a response actually came from once
// redirects have been followed. See `FetchedBytes::served_by`.
use ureq::ResponseExt;

use crate::error::NetError;

/// How long a single request may take, start to finish.
///
/// Generous enough for a slow connection, short enough that a hung request
/// does not pin a worker thread for the rest of the session.
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// The largest response body that will be read.
///
/// Lyrics are a few kilobytes. This is not a tuning knob, it is a refusal to
/// let a misbehaving or hostile endpoint stream until memory runs out.
pub const MAX_BODY_BYTES: u64 = 1024 * 1024;

/// The largest image that will be read.
///
/// A 500px cover is 30-100 KB. Four megabytes is far more than one should
/// ever be, and still a refusal rather than a tuning knob.
pub const MAX_IMAGE_BYTES: u64 = 4 * 1024 * 1024;

/// A binary response body, and where it actually came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedBytes {
    pub body: Vec<u8>,
    /// Bytes received, for the activity log.
    pub bytes: u64,
    /// The host that served this, after any redirects were followed.
    ///
    /// The reason this is carried at all: a request addressed to
    /// `coverartarchive.org` is answered by the Internet Archive, so the host
    /// asked and the host that replied are different machines. Recording the
    /// one that replied is what keeps the activity log a record of what
    /// happened rather than of what was intended.
    pub served_by: Option<String>,
}

/// A response body, and what it cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    pub body: String,
    /// Bytes received, for the activity log.
    pub bytes: u64,
}

/// Somewhere a request can be sent.
///
/// The seam that keeps the fetchers testable offline. See the module note.
pub trait Transport: Send + Sync {
    /// Perform a GET, following the service's rules for what counts as a miss.
    fn get(&self, url: &str) -> Result<Fetched, NetError>;

    /// The same, for a response that is not text.
    ///
    /// Required rather than defaulted: a transport that silently could not
    /// fetch an image would look exactly like a service with no cover for a
    /// release, and the difference matters.
    fn get_bytes(&self, url: &str) -> Result<FetchedBytes, NetError>;
}

/// The real thing.
pub struct Http {
    agent: ureq::Agent,
}

impl std::fmt::Debug for Http {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Http").finish_non_exhaustive()
    }
}

impl Default for Http {
    fn default() -> Self {
        Self::new()
    }
}

impl Http {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            .user_agent(user_agent())
            // Statuses are handled below rather than raised as errors, because
            // a 404 is a normal answer here and not a fault.
            .http_status_as_error(false)
            .build();

        Self {
            agent: config.into(),
        }
    }
}

/// How Resonance identifies itself.
///
/// LRCLIB asks callers to send a name, a version and a link, and MusicBrainz
/// blocks the ones that do not. It is also the other half of being open about
/// this: an operator looking at their traffic can tell what this is, and the
/// user can search for the same string and find the same answer.
pub fn user_agent() -> String {
    format!(
        "Resonance/{} ( {} )",
        env!("CARGO_PKG_VERSION"),
        "https://github.com/Anonymous-Floof/resonance"
    )
}

impl Transport for Http {
    fn get(&self, url: &str) -> Result<Fetched, NetError> {
        let mut response = self.send(url)?;

        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_BODY_BYTES)
            .read_to_string()
            .map_err(|err| NetError::Decode(err.to_string()))?;

        Ok(Fetched {
            bytes: body.len() as u64,
            body,
        })
    }

    fn get_bytes(&self, url: &str) -> Result<FetchedBytes, NetError> {
        let mut response = self.send(url)?;

        // Read after any redirect has been followed, so this names the machine
        // that actually answered rather than the one that was asked.
        let served_by = response
            .get_uri()
            .host()
            .map(|host| host.trim_start_matches("www.").to_owned());

        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_IMAGE_BYTES)
            .read_to_vec()
            .map_err(|err| NetError::Decode(err.to_string()))?;

        Ok(FetchedBytes {
            bytes: body.len() as u64,
            body,
            served_by,
        })
    }
}

impl Http {
    /// Send the request and turn the status line into a [`NetError`].
    ///
    /// Shared so that text and binary fetches cannot disagree about what a
    /// 404 or a 429 means — the rate limiter backs off on one and not the
    /// other, so the distinction is load-bearing.
    fn send(&self, url: &str) -> Result<ureq::http::Response<ureq::Body>, NetError> {
        let response = self
            .agent
            .get(url)
            .call()
            .map_err(|err| NetError::Transport(err.to_string()))?;

        let status = response.status().as_u16();

        if status == 404 {
            return Err(NetError::NotFound);
        }

        if status == 429 {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .and_then(|text| text.trim().parse::<u64>().ok())
                .map(Duration::from_secs);

            return Err(NetError::RateLimited { retry_after });
        }

        if !(200..300).contains(&status) {
            return Err(NetError::Status { status });
        }

        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// LRCLIB asks for a name, a version and a link, and this is the string
    /// their operators will see. It should not silently become "Resonance//".
    #[test]
    fn the_user_agent_identifies_the_application() {
        let agent = user_agent();

        assert!(agent.starts_with("Resonance/"), "{agent}");
        assert!(agent.contains(env!("CARGO_PKG_VERSION")), "{agent}");
        assert!(agent.contains("github.com"), "{agent}");
        assert!(!agent.contains("//)"), "the version went missing: {agent}");
    }

    /// Nothing in the test suite may touch the network, so the one type that
    /// can must not be built by accident in a test helper somewhere.
    #[test]
    fn constructing_the_agent_does_not_connect() {
        let _http = Http::new();
    }
}
