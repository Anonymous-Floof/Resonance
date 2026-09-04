//! The places Resonance is allowed to talk to, described in the user's terms.
//!
//! A [`Source`] is not a base URL with a label stuck on it. It is the answer to
//! the questions someone is entitled to ask before turning a network feature
//! on: *who is this, what does it give me, what leaves my machine, and how
//! often will it happen.* Those answers are needed in three places — the
//! opt-in screen, the activity log, and the rate limiter — and they must be the
//! same answers in all three, which is why they are one value and not three.
//!
//! ## The registry is empty, and that is the point
//!
//! [`SOURCES`] lists every source this build can reach. Today it is empty,
//! because this build reaches nothing: there is no transport in the crate yet.
//! Each source gets appended in the same commit that makes it work, alongside
//! the settings and the documentation that describe it — never before.
//!
//! Most of the tests below are therefore written as rules over the whole
//! registry rather than assertions about its current contents: they pass
//! vacuously today, they check the first entry the moment it is added, and
//! they never have to be rewritten to make room for it.
//!
//! One is not like that. `no_source_is_reachable_until_one_is_added_deliberately`
//! asserts the registry is empty, and adding a source is supposed to break it.
//! That is the point — it is the "never ship a setting before the feature"
//! rule made executable, and failing it is the reminder that the settings, the
//! documentation and the README claims are due in the same commit.

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

/// Every source this build can reach.
///
/// Empty until the first fetcher lands. See the module documentation.
pub const SOURCES: &[Source] = &[];

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
    fn no_source_is_reachable_until_one_is_added_deliberately() {
        assert!(
            SOURCES.is_empty(),
            "a source appeared without this test being updated to expect it, \
             which means it arrived without the settings and documentation \
             that are supposed to land in the same commit"
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
