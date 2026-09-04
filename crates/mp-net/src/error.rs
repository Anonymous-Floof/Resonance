//! What can go wrong on the way out.
//!
//! The variants exist because something constructs each of them — an error
//! enum whose arms nobody builds is the same mistake as a setting nobody
//! reads. They map onto [`crate::Outcome`] so the activity log says the same
//! thing the code decided.
//!
//! Note that [`NetError::NotFound`] is an error only in the sense that there
//! is no answer. It is the ordinary reply for an obscure track, it does not
//! count as a failure, and it must not trigger backoff: the service worked.
//! [`NetError::is_failure`] is the distinction, and the rate limiter is fed
//! from it.

use std::time::Duration;

use thiserror::Error;

use crate::Outcome;

#[derive(Debug, Error)]
pub enum NetError {
    /// The service answered, and has nothing for this query.
    #[error("no match")]
    NotFound,

    /// The service asked us to slow down. `retry_after` is its own figure,
    /// from the `Retry-After` header, when it gave one.
    #[error("rate limited by the service")]
    RateLimited { retry_after: Option<Duration> },

    /// The service answered with something we cannot use.
    #[error("the service answered {status}")]
    Status { status: u16 },

    /// The request never completed: no route, refused, timed out, TLS.
    #[error("could not reach the service: {0}")]
    Transport(String),

    /// The answer arrived and was not what was expected.
    #[error("could not read the answer: {0}")]
    Decode(String),

    /// The request was not attempted, because the feature is switched off.
    ///
    /// Carried as an error rather than a silent `None` so the activity log can
    /// still record that something wanted to ask and did not.
    #[error("networking is switched off")]
    Disabled,
}

impl NetError {
    /// Whether this should lengthen the gap before the next request.
    ///
    /// A miss should not: it is a normal answer, and backing off after every
    /// obscure track would slow the fetcher to a crawl on exactly the library
    /// that needs it most.
    pub fn is_failure(&self) -> bool {
        match self {
            Self::NotFound | Self::Disabled => false,
            Self::RateLimited { .. }
            | Self::Status { .. }
            | Self::Transport(_)
            | Self::Decode(_) => true,
        }
    }

    /// How this reads in the activity log.
    pub fn outcome(&self) -> Outcome {
        match self {
            Self::NotFound => Outcome::NotFound,
            Self::Disabled => Outcome::Skipped,
            _ => Outcome::Failed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The common answer for an obscure track. Treating it as a failure would
    /// back the fetcher off into uselessness on a library of rarities.
    #[test]
    fn a_miss_is_not_a_failure() {
        assert!(!NetError::NotFound.is_failure());
        assert_eq!(NetError::NotFound.outcome(), Outcome::NotFound);
    }

    #[test]
    fn being_switched_off_is_not_a_failure_either() {
        assert!(!NetError::Disabled.is_failure());
        assert_eq!(NetError::Disabled.outcome(), Outcome::Skipped);
    }

    #[test]
    fn real_problems_count_as_failures() {
        let problems = [
            NetError::RateLimited { retry_after: None },
            NetError::Status { status: 500 },
            NetError::Transport("refused".into()),
            NetError::Decode("not json".into()),
        ];

        for problem in problems {
            assert!(
                problem.is_failure(),
                "{problem} should back the fetcher off"
            );
            assert_eq!(problem.outcome(), Outcome::Failed);
        }
    }

    /// The message ends up in the log's detail column, where it is the only
    /// explanation the user gets.
    #[test]
    fn every_error_says_something_useful() {
        assert_eq!(
            NetError::Status { status: 503 }.to_string(),
            "the service answered 503"
        );
        assert!(
            NetError::Transport("timed out".into())
                .to_string()
                .contains("timed out")
        );
    }
}
