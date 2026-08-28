//! The background audio-analysis pass, driven from the interface.
//!
//! Measuring what a track sounds like means decoding it, which is seconds of
//! work per file. On a library of a few thousand that is an hour or two — so
//! this is built to be entirely unobtrusive: it runs on its own thread with its
//! own database connection, commits each result as it goes, and can be stopped
//! at any moment without losing anything.
//!
//! It is never on the path of something the user is waiting for. Similarity
//! works without it, using tags and playlist history; the analysis only makes
//! the answers better, and best of all on the untagged files where nothing else
//! has anything to go on.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use mp_core::library::Library;

/// How many tracks one batch handles before checking in.
///
/// Small enough that stopping is prompt, large enough that the check-in cost
/// is noise beside the decoding.
const BATCH: usize = 8;

/// How long the thread rests between batches.
///
/// Deliberate: the point is to finish eventually without the fans spinning up
/// or a track stuttering because the decoder is competing with this for a core.
const BREATH: std::time::Duration = std::time::Duration::from_millis(120);

/// Live counters, shared with the running thread.
#[derive(Debug, Default)]
pub struct Progress {
    pub analysed: AtomicU32,
    pub failed: AtomicU32,
    pub remaining: AtomicU32,
    pub finished: AtomicBool,
}

impl Progress {
    fn snapshot(&self) -> Status {
        Status {
            analysed: self.analysed.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
            remaining: self.remaining.load(Ordering::Relaxed),
            finished: self.finished.load(Ordering::Relaxed),
        }
    }
}

/// A readable snapshot of the pass.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    pub analysed: u32,
    pub failed: u32,
    pub remaining: u32,
    pub finished: bool,
}

impl Status {
    /// How far along, `0.0..=1.0`, or `None` when nothing is running.
    pub fn fraction(&self) -> Option<f32> {
        let done = self.analysed + self.failed;
        let total = done + self.remaining;

        if total == 0 {
            return None;
        }

        Some(done as f32 / total as f32)
    }

    /// A line for the settings panel.
    pub fn summary(&self) -> String {
        if self.finished {
            return match self.failed {
                0 => format!("Analysed {}.", tracks(self.analysed)),
                failed => format!(
                    "Analysed {}, {failed} could not be read.",
                    tracks(self.analysed)
                ),
            };
        }

        format!("Analysed {}, {} to go.", self.analysed, self.remaining)
    }
}

/// "1 track" rather than "1 tracks".
fn tracks(count: u32) -> String {
    match count {
        1 => "1 track".to_owned(),
        other => format!("{other} tracks"),
    }
}

/// A running pass.
pub struct AnalysisJob {
    progress: Arc<Progress>,
    cancel: Arc<AtomicBool>,
    /// Set when the thread stops, carrying anything that went wrong.
    outcome: Arc<Mutex<Option<String>>>,
}

impl AnalysisJob {
    /// Start analysing, if there is a persistent library to analyse into.
    ///
    /// Returns `None` for an in-memory library — there is nowhere to store the
    /// results, so the work would be thrown away.
    pub fn start(library: &Library) -> Option<Self> {
        let connection = match library.detached_connection()? {
            Ok(connection) => connection,
            Err(err) => {
                tracing::error!("could not open a connection for analysis: {err:#}");
                return None;
            }
        };

        let progress = Arc::new(Progress::default());
        let cancel = Arc::new(AtomicBool::new(false));
        let outcome: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        // Seed the remaining count before the first batch, so the progress bar
        // does not start at "0 to go" and jump.
        if let Ok((done, total)) = mp_core::library::features::progress(&connection) {
            progress
                .remaining
                .store(total.saturating_sub(done), Ordering::Relaxed);
        }

        let job = Self {
            progress: Arc::clone(&progress),
            cancel: Arc::clone(&cancel),
            outcome: Arc::clone(&outcome),
        };

        let spawned = std::thread::Builder::new()
            .name("resonance-analysis".into())
            .spawn(move || {
                loop {
                    if cancel.load(Ordering::Relaxed) {
                        break;
                    }

                    match mp_audio::analysis::run_batch(&connection, BATCH, &cancel) {
                        Ok(batch) => {
                            progress
                                .analysed
                                .fetch_add(batch.analysed as u32, Ordering::Relaxed);
                            progress
                                .failed
                                .fetch_add(batch.failed as u32, Ordering::Relaxed);
                            progress.remaining.store(batch.remaining, Ordering::Relaxed);

                            // Nothing done and nothing left: the queue is empty.
                            if batch.analysed == 0 && batch.failed == 0 {
                                break;
                            }
                        }
                        Err(err) => {
                            tracing::error!("the analysis pass stopped: {err:#}");
                            if let Ok(mut slot) = outcome.lock() {
                                *slot = Some(err.to_string());
                            }
                            break;
                        }
                    }

                    std::thread::sleep(BREATH);
                }

                progress.finished.store(true, Ordering::Relaxed);
            });

        if let Err(err) = spawned {
            tracing::error!("could not start the analysis thread: {err}");
            return None;
        }

        Some(job)
    }

    pub fn status(&self) -> Status {
        self.progress.snapshot()
    }

    pub fn is_running(&self) -> bool {
        !self.progress.finished.load(Ordering::Relaxed)
    }

    /// Ask the pass to stop. It finishes the track it is on and exits.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Take the error the pass stopped with, if it did.
    pub fn take_error(&self) -> Option<String> {
        self.outcome.lock().ok()?.take()
    }
}

impl Drop for AnalysisJob {
    /// Dropping the handle stops the work.
    ///
    /// Otherwise closing the window would leave a thread decoding audio until
    /// the process was killed, which on a large library is minutes of a warm
    /// laptop for nothing.
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A library with exactly one unanalysed track is the ordinary case after
    /// adding a song, and "Analysed 1 tracks." looked broken.
    #[test]
    fn a_single_track_is_not_plural() {
        let status = Status {
            analysed: 1,
            failed: 0,
            remaining: 0,
            finished: true,
        };

        assert_eq!(status.summary(), "Analysed 1 track.");
    }

    #[test]
    fn several_tracks_are_plural() {
        let status = Status {
            analysed: 42,
            failed: 0,
            remaining: 0,
            finished: true,
        };

        assert_eq!(status.summary(), "Analysed 42 tracks.");
    }

    #[test]
    fn failures_are_reported_alongside() {
        let status = Status {
            analysed: 10,
            failed: 2,
            remaining: 0,
            finished: true,
        };

        assert!(status.summary().contains("2 could not be read"));
    }

    #[test]
    fn an_in_memory_library_has_nothing_to_analyse_into() {
        let library = Library::in_memory().unwrap();
        assert!(
            AnalysisJob::start(&library).is_none(),
            "a job was started with nowhere to store its results"
        );
    }

    #[test]
    fn progress_is_reported_as_a_fraction() {
        let status = Status {
            analysed: 25,
            failed: 5,
            remaining: 70,
            finished: false,
        };

        assert_eq!(status.fraction(), Some(0.3));
    }

    /// Nothing to do is not zero percent done — it is no progress bar at all.
    #[test]
    fn an_empty_queue_has_no_fraction() {
        assert_eq!(Status::default().fraction(), None);
    }

    #[test]
    fn a_finished_pass_reads_as_finished() {
        let clean = Status {
            analysed: 40,
            failed: 0,
            remaining: 0,
            finished: true,
        };
        assert_eq!(clean.summary(), "Analysed 40 tracks.");

        let messy = Status { failed: 3, ..clean };
        assert!(messy.summary().contains("3 could not be read"));
    }

    #[test]
    fn a_running_pass_says_how_much_is_left() {
        let status = Status {
            analysed: 10,
            failed: 0,
            remaining: 90,
            finished: false,
        };

        assert!(status.summary().contains("90 to go"));
        assert_eq!(status.fraction(), Some(0.1));
    }
}
