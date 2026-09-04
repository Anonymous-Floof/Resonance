//! Asking politely: a minimum gap between requests, and backing off on failure.
//!
//! This exists before any transport does, deliberately. A rate limiter added
//! after the first fetcher works is a rate limiter that was not there when the
//! fetcher was being tested against a live service, and "we got blocked while
//! developing it" is the normal outcome of that order. MusicBrainz permits one
//! request per second and enforces it.
//!
//! Two behaviours, kept in one place because they interact:
//!
//! - **A floor on the gap between requests**, taken from the source itself
//!   ([`Source::min_interval`]).
//! - **Exponential backoff after failures**, so a service that is down, rate
//!   limiting us, or simply unreachable is not hammered. The gap doubles per
//!   consecutive failure and is capped, and one success clears it.
//!
//! ## Blocking
//!
//! [`Limiter::acquire`] sleeps. That is correct on a background enrichment
//! worker and wrong anywhere near the UI or the audio thread — a limiter in
//! backoff can sleep for minutes. Callers that must not block ask
//! [`Limiter::delay_at`] what the wait would be and come back later; that is
//! also the entry point the tests use, since it takes the current time as an
//! argument rather than reading the clock.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::source::Source;

/// The longest the gap between requests will grow to under repeated failure.
///
/// Long enough that a service being down overnight costs a handful of
/// requests rather than thousands; short enough that recovery is noticed
/// within one sitting rather than needing a restart.
pub const MAX_BACKOFF: Duration = Duration::from_secs(300);

/// How many doublings are applied before the cap does the work anyway.
///
/// Purely to keep the shift in range: at a one-second floor, 2^20 seconds is
/// already twelve days, and [`MAX_BACKOFF`] has long since clamped it.
const MAX_DOUBLINGS: u32 = 20;

/// Paces requests to one service.
///
/// One limiter per source. Sharing one between services would make a slow
/// source throttle a healthy one, and a failing source stop both.
#[derive(Debug)]
pub struct Limiter {
    min_interval: Duration,
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    /// When the last request was made. `None` before the first one, which is
    /// what lets the first request go out immediately.
    last_request: Option<Instant>,
    consecutive_failures: u32,
}

impl Limiter {
    /// A limiter with an explicit floor between requests.
    pub fn new(min_interval: Duration) -> Self {
        Self {
            min_interval,
            state: Mutex::new(State {
                last_request: None,
                consecutive_failures: 0,
            }),
        }
    }

    /// A limiter honouring what the source itself asks for.
    ///
    /// The normal way to build one — it is why [`Source::min_interval`] is
    /// part of a source's description rather than a constant at a call site.
    pub fn for_source(source: &Source) -> Self {
        Self::new(source.min_interval)
    }

    /// How long a request made at `now` would have to wait.
    ///
    /// `ZERO` means it may go immediately. Takes the time as an argument so
    /// that a caller which cannot block can poll it, and so the tests can
    /// examine the whole schedule without sleeping through it.
    pub fn delay_at(&self, now: Instant) -> Duration {
        let state = self.lock();

        let Some(last) = state.last_request else {
            // Nothing has been sent yet, so nothing is owed.
            return Duration::ZERO;
        };

        let ready = last + self.interval(state.consecutive_failures);
        ready.saturating_duration_since(now)
    }

    /// The current gap between requests, backoff included.
    ///
    /// Worth showing in the activity view: "waiting 40s before the next
    /// request" is a far better explanation of an idle fetcher than silence.
    pub fn current_interval(&self) -> Duration {
        self.interval(self.lock().consecutive_failures)
    }

    /// Record that a request was made at `now`.
    ///
    /// Called by [`acquire`](Self::acquire); public for callers driving the
    /// limiter themselves rather than sleeping on it.
    pub fn record_at(&self, now: Instant) {
        self.lock().last_request = Some(now);
    }

    /// Block until a request is allowed, then record it.
    ///
    /// **Background threads only.** See the note on blocking in the module
    /// documentation.
    pub fn acquire(&self) {
        // Re-read the clock after sleeping rather than assuming the sleep was
        // exact. Windows timer granularity rounds a sleep up by a millisecond
        // or two, and recording the pre-sleep time would let that error
        // accumulate into a gap slightly under the floor.
        let delay = self.delay_at(Instant::now());
        if delay > Duration::ZERO {
            std::thread::sleep(delay);
        }
        self.record_at(Instant::now());
    }

    /// Note that a request succeeded, clearing any backoff.
    pub fn note_success(&self) {
        self.lock().consecutive_failures = 0;
    }

    /// Note that a request failed, lengthening the gap before the next one.
    ///
    /// "Failed" means the service could not be reached or would not answer —
    /// a timeout, a refused connection, a 5xx, a 429. A clean answer of "I do
    /// not have that", which is the common case when looking up an obscure
    /// track, is a success: the service worked exactly as intended.
    pub fn note_failure(&self) {
        let mut state = self.lock();
        state.consecutive_failures = state.consecutive_failures.saturating_add(1);
    }

    /// How many requests have failed in a row.
    pub fn consecutive_failures(&self) -> u32 {
        self.lock().consecutive_failures
    }

    /// The gap implied by a failure count.
    fn interval(&self, failures: u32) -> Duration {
        if failures == 0 {
            return self.min_interval;
        }

        let doublings = failures.min(MAX_DOUBLINGS);
        let grown = self
            .min_interval
            .checked_mul(1u32 << doublings)
            .unwrap_or(Duration::MAX);

        // The cap is a ceiling on the wait, not on the floor: a source that
        // asks for a longer gap than MAX_BACKOFF keeps the gap it asked for.
        grown.min(MAX_BACKOFF.max(self.min_interval))
    }

    /// A poisoned lock here means another thread panicked mid-update. The
    /// worst state that can leave behind is a stale timestamp or a wrong
    /// failure count, neither of which is worth propagating a panic into an
    /// enrichment worker over.
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: Duration = Duration::from_secs(1);

    #[test]
    fn the_first_request_does_not_wait() {
        let limiter = Limiter::new(SECOND);
        assert_eq!(limiter.delay_at(Instant::now()), Duration::ZERO);
    }

    #[test]
    fn a_second_request_waits_out_the_interval() {
        let limiter = Limiter::new(SECOND);
        let start = Instant::now();
        limiter.record_at(start);

        assert_eq!(limiter.delay_at(start), SECOND);
        assert_eq!(
            limiter.delay_at(start + Duration::from_millis(400)),
            Duration::from_millis(600)
        );
        assert_eq!(limiter.delay_at(start + SECOND), Duration::ZERO);
        assert_eq!(
            limiter.delay_at(start + Duration::from_secs(30)),
            Duration::ZERO,
            "waiting longer than required does not earn credit"
        );
    }

    #[test]
    fn failures_double_the_gap() {
        let limiter = Limiter::new(SECOND);
        let start = Instant::now();
        limiter.record_at(start);

        limiter.note_failure();
        assert_eq!(limiter.delay_at(start), Duration::from_secs(2));

        limiter.note_failure();
        assert_eq!(limiter.delay_at(start), Duration::from_secs(4));

        limiter.note_failure();
        assert_eq!(limiter.delay_at(start), Duration::from_secs(8));
    }

    #[test]
    fn one_success_clears_the_backoff() {
        let limiter = Limiter::new(SECOND);
        let start = Instant::now();
        limiter.record_at(start);

        for _ in 0..5 {
            limiter.note_failure();
        }
        assert_eq!(limiter.consecutive_failures(), 5);
        assert!(limiter.delay_at(start) > SECOND);

        limiter.note_success();
        assert_eq!(limiter.consecutive_failures(), 0);
        assert_eq!(limiter.delay_at(start), SECOND);
    }

    /// Without a cap, a service down overnight would schedule the next attempt
    /// somewhere in the following decade.
    #[test]
    fn backoff_stops_growing_at_the_cap() {
        let limiter = Limiter::new(SECOND);
        let start = Instant::now();
        limiter.record_at(start);

        for _ in 0..200 {
            limiter.note_failure();
        }

        assert_eq!(limiter.current_interval(), MAX_BACKOFF);
        assert_eq!(limiter.delay_at(start), MAX_BACKOFF);
    }

    /// The failure count is saturating, so a very long outage must not wrap
    /// the counter around to zero and quietly resume full-rate requests.
    #[test]
    fn an_absurd_number_of_failures_does_not_wrap_around() {
        let limiter = Limiter::new(SECOND);
        for _ in 0..3 {
            limiter.note_failure();
        }
        limiter.lock().consecutive_failures = u32::MAX;

        limiter.note_failure();

        assert_eq!(limiter.consecutive_failures(), u32::MAX);
        assert_eq!(limiter.current_interval(), MAX_BACKOFF);
    }

    /// The cap is a ceiling on backoff, not permission to ignore a source that
    /// asks for a longer gap than the cap in the first place.
    #[test]
    fn a_source_slower_than_the_cap_keeps_its_own_interval() {
        let hourly = Duration::from_secs(3_600);
        let limiter = Limiter::new(hourly);
        let start = Instant::now();
        limiter.record_at(start);

        assert_eq!(limiter.delay_at(start), hourly);

        limiter.note_failure();
        assert_eq!(
            limiter.delay_at(start),
            hourly,
            "backoff may lengthen the gap, never shorten it"
        );
    }

    #[test]
    fn a_limiter_takes_its_pace_from_the_source() {
        let source = Source {
            id: "example",
            label: "Example",
            host: "example.org",
            purpose: "A test fixture.",
            sends: "Nothing.",
            terms: "https://example.org/terms",
            min_interval: Duration::from_millis(1_500),
        };

        let limiter = Limiter::for_source(&source);
        let start = Instant::now();
        limiter.record_at(start);

        assert_eq!(limiter.delay_at(start), Duration::from_millis(1_500));
    }

    /// `acquire` is the blocking path, so it is tested at a pace short enough
    /// not to slow the suite down but long enough to be measurable.
    #[test]
    fn acquire_actually_waits_between_requests() {
        let limiter = Limiter::new(Duration::from_millis(40));

        let start = Instant::now();
        limiter.acquire();
        limiter.acquire();
        let elapsed = start.elapsed();

        assert!(
            elapsed >= Duration::from_millis(40),
            "two requests took {elapsed:?}, less than one interval apart"
        );
    }
}
