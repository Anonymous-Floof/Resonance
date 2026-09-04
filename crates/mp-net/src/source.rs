//! The places Resonance is allowed to talk to, described in the user's terms.
//!
//! A [`Source`] is not a base URL with a label stuck on it. It is the answer to
//! the questions someone is entitled to ask before turning a network feature
//! on: *who is this, what does it give me, what leaves my machine, and how
//! often will it happen.* Those answers are needed in three places — the
//! opt-in screen, the activity log, and the rate limiter — and they must be the
//! same answers in all three, which is why they are one value and not three.
//!
//! ## The registry
//!
//! [`SOURCES`] lists every source this build can reach — the complete answer
//! to "where can this thing talk to", in one place, checkable at a glance.
//! Each entry is added in the same commit that makes it work, alongside the
//! setting that governs it and the documentation that describes it.
//!
//! It held nothing until LRCLIB was added for lyrics, and a test asserted the
//! emptiness so that the first addition could not happen quietly. That test
//! has done its job and is gone; the rules below outlive it, and they are
//! written over the whole registry so the *next* entry is checked the moment
//! it arrives.

use std::time::Duration;

/// One remote service, and everything that has to be said about it.
///
/// Every field is `&'static str` on purpose. A source is a fact about the
/// build — decided at compile time, identical for every user, and not
/// something that could arrive from a config file. A service the user could
/// add themselves would make "here is everything this build can reach" an
/// unanswerable question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Source {
    /// Stable machine name. Written into the log and the config file, so
    /// changing one is a migration rather than a rename.
    pub id: &'static str,

    /// What to call it on screen.
    pub label: &'static str,

    /// The single host contacted, without a scheme or a path.
    ///
    /// One host per source, so that "what did it talk to" has a short and
    /// complete answer. A service needing two hosts is two sources.
    pub host: &'static str,

    /// What the user gets out of it, in a sentence.
    pub purpose: &'static str,

    /// What leaves the machine, specifically.
    ///
    /// Not "some metadata". The actual fields: an artist name, a track title,
    /// a duration. This is the sentence the opt-in screen is built around, and
    /// vagueness here is the failure mode the whole branch exists to avoid.
    pub sends: &'static str,

    /// Where to read the service's own terms.
    pub terms: &'static str,

    /// The shortest gap this service permits between requests.
    ///
    /// Declared per source because it is the service's rule, not ours —
    /// MusicBrainz asks for one request per second and enforces it. Carried
    /// here so [`crate::Limiter`] cannot be built without one.
    pub min_interval: Duration,
}

/// Lyrics, contributed by its users and given away for free.
///
/// Chosen over the alternatives because it needs no account and no API key,
/// and because `/api/get` matches on artist, title, album *and* duration at
/// once — so a lookup either finds the right recording or finds nothing. A
/// service that returns near-enough matches is how the wrong words end up on
/// a song, and there is no undo for that.
pub const LRCLIB: Source = Source {
    id: "lrclib",
    label: "LRCLIB",
    host: "lrclib.net",
    purpose: "Lyrics, timed to the music where someone has contributed them.",
    sends: "The artist, title and album from the track's own tags, and its length in seconds.",
    terms: "https://lrclib.net/docs",
    // LRCLIB asks for requests one at a time with a short gap between them,
    // and answers 429 with a Retry-After when that is ignored.
    min_interval: Duration::from_millis(500),
};

/// Every source this build can reach.
pub const SOURCES: &[Source] = &[LRCLIB];

/// Look up a source by its [`id`](Source::id).
///
/// Used when reading an id back out of the config or the log, where the string
/// came from disk and may name a source this build no longer has.
pub fn find(id: &str) -> Option<&'static Source> {
    SOURCES.iter().find(|source| source.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A source is a fixture rather than a real entry, so these tests describe
    /// the shape a real one must have without pretending one exists.
    const EXAMPLE: Source = Source {
        id: "example",
        label: "Example",
        host: "example.org",
        purpose: "Nothing at all; this source is a test fixture.",
        sends: "Nothing, because no request is ever made to it.",
        terms: "https://example.org/terms",
        min_interval: Duration::from_secs(1),
    };

    #[test]
    fn the_registry_lists_what_this_build_can_reach() {
        assert!(
            find("lrclib").is_some(),
            "lyrics fetching is built, so its source must be listed"
        );
    }

    #[test]
    fn every_source_answers_all_of_the_questions() {
        for source in SOURCES {
            assert!(!source.id.is_empty(), "a source needs an id");
            assert!(!source.label.is_empty(), "{} needs a label", source.id);
            assert!(!source.host.is_empty(), "{} needs a host", source.id);
            assert!(!source.purpose.is_empty(), "{} needs a purpose", source.id);
            assert!(
                !source.sends.is_empty(),
                "{} must say what it sends: that sentence is the whole opt-in",
                source.id
            );
            assert!(!source.terms.is_empty(), "{} needs terms", source.id);
        }
    }

    /// A host with a scheme or a path cannot be shown to a user as "this is
    /// what it talked to", and would quietly break any comparison against it.
    #[test]
    fn a_host_is_a_bare_host() {
        for source in SOURCES.iter().chain(std::iter::once(&EXAMPLE)) {
            let host = source.host;
            assert!(!host.contains("://"), "{host} carries a scheme");
            assert!(!host.contains('/'), "{host} carries a path");
            assert!(!host.contains(' '), "{host} is not a single host");
        }
    }

    /// Two sources sharing an id would make the log ambiguous and let a
    /// config entry enable the wrong one.
    #[test]
    fn ids_are_unique() {
        for (index, source) in SOURCES.iter().enumerate() {
            let duplicate = SOURCES[index + 1..]
                .iter()
                .any(|other| other.id == source.id);
            assert!(!duplicate, "{} is listed twice", source.id);
        }
    }

    /// Zero would mean requests as fast as the loop can issue them, which is
    /// how a free service comes to block an application.
    #[test]
    fn every_source_declares_a_real_rate_limit() {
        for source in SOURCES {
            assert!(
                source.min_interval > Duration::ZERO,
                "{} must declare a minimum interval between requests",
                source.id
            );
        }
    }

    #[test]
    fn an_unknown_id_finds_nothing() {
        assert!(find("nothing-by-this-name").is_none());
        assert!(find("").is_none());
    }
}
