//! The background pass that finds covers for albums that have none.
//!
//! Shaped like the audio-analysis pass rather than like the lyrics lookup, and
//! for the same reason: it is a sweep over the whole library rather than a
//! question about the track in front of you. It runs on its own thread with
//! its own database connection, commits each album as it finishes, and can be
//! stopped at any moment without losing what it has already done.
//!
//! It is never on the path of something the user is waiting for. An album with
//! no cover looks exactly as it did before — the pass only ever fills gaps in,
//! and a failure leaves the gap where it was.
//!
//! ## It paces itself
//!
//! MusicBrainz permits one request a second and enforces it, so a library with
//! a thousand uncovered albums is well over half an hour of wall-clock time.
//! That is fine, and it is why this is a background sweep with a progress
//! readout rather than something the user waits on. The rate limiter in
//! `mp-net` does the pacing; the small rest between albums here is for the
//! image decoding, which is real work on the CPU and should not compete with
//! playback for a core.
//!
//! ## Where an answer goes
//!
//! Into the app's own cache and the index, through the same content-addressed
//! store used for embedded artwork. **No audio file is opened, and no tag is
//! written.** Turning the feature off later leaves the covers in place; the
//! cache can be cleared from Settings.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use mp_core::library::art::ArtCache;
use mp_core::library::{Library, enrich};
use mp_net::Activity;
use mp_net::artwork::{Client, Query};

/// How many albums are claimed from the index at a time.
///
/// Small: the list is re-read each round, so an album that gained a cover by
/// some other route in the meantime drops out of the queue on its own.
const BATCH: usize = 8;

/// How long the thread rests between albums.
///
/// The network pacing dwarfs this. It exists so the image decode and resize do
/// not run back to back on a core the decoder wants.
const BREATH: std::time::Duration = std::time::Duration::from_millis(150);

/// Live counters, shared with the running thread.
#[derive(Debug, Default)]
pub struct Progress {
    /// Albums that came back with a cover.
    pub found: AtomicU32,
    /// Albums looked up with no cover to be had.
    pub missing: AtomicU32,
    pub remaining: AtomicU32,
    pub finished: AtomicBool,
}

impl Progress {
    fn snapshot(&self) -> Status {
        Status {
            found: self.found.load(Ordering::Relaxed),
            missing: self.missing.load(Ordering::Relaxed),
            remaining: self.remaining.load(Ordering::Relaxed),
            finished: self.finished.load(Ordering::Relaxed),
        }
    }
}

/// A readable snapshot of the pass.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    pub found: u32,
    pub missing: u32,
    pub remaining: u32,
    pub finished: bool,
}

impl Status {
    /// How far along, `0.0..=1.0`, or `None` when nothing is running.
    pub fn fraction(&self) -> Option<f32> {
        let done = self.found + self.missing;
        let total = done + self.remaining;

        if total == 0 {
            return None;
        }

        Some(done as f32 / total as f32)
    }

    /// A line for the settings screen.
    ///
    /// Says how many were looked up and how many actually had a cover, because
    /// those are very different numbers on a scrappily tagged library and a
    /// user seeing only the first would think it had worked.
    pub fn summary(&self) -> String {
        if self.finished && self.remaining == 0 {
            return format!(
                "Finished. {} covers found, {} albums had none.",
                self.found, self.missing
            );
        }

        format!(
            "{} found, {} with none, {} to go.",
            self.found, self.missing, self.remaining
        )
    }
}

/// The running pass.
pub struct ArtworkJob {
    progress: Arc<Progress>,
    cancel: Arc<AtomicBool>,
    outcome: Arc<Mutex<Option<String>>>,
}

impl std::fmt::Debug for ArtworkJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArtworkJob")
            .field("status", &self.status())
            .finish()
    }
}

impl ArtworkJob {
    /// Start looking, if there is a persistent library to write into.
    ///
    /// Returns `None` for an in-memory library — the covers would be found and
    /// then thrown away.
    pub fn start(
        library: &Library,
        activity: Arc<Activity>,
        cache_root: PathBuf,
        ctx: egui::Context,
    ) -> Option<Self> {
        let connection = match library.detached_connection()? {
            Ok(connection) => connection,
            Err(err) => {
                tracing::error!("could not open a connection for artwork: {err:#}");
                return None;
            }
        };

        // Built from the same root the library uses, so a fetched cover lands
        // beside the embedded ones and is indistinguishable afterwards.
        let art = ArtCache::new(library.art().root().to_path_buf());

        let progress = Arc::new(Progress::default());
        let cancel = Arc::new(AtomicBool::new(false));
        let outcome: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        // Seed the count before the first request so the readout does not
        // start at "nothing to go" and then jump.
        if let Ok(waiting) = enrich::albums_without_art(&connection, usize::MAX) {
            progress
                .remaining
                .store(waiting.len() as u32, Ordering::Relaxed);
        }

        let job = Self {
            progress: Arc::clone(&progress),
            cancel: Arc::clone(&cancel),
            outcome: Arc::clone(&outcome),
        };

        let spawned = std::thread::Builder::new()
            .name("resonance-artwork".into())
            .spawn(move || {
                // Owned by this thread. Nothing on the UI side can reach the
                // transport, which is what keeps the request off the frame.
                let client = Client::new(&cache_root, activity);

                loop {
                    if cancel.load(Ordering::Relaxed) {
                        break;
                    }

                    let batch = match enrich::albums_without_art(&connection, BATCH) {
                        Ok(batch) => batch,
                        Err(err) => {
                            *outcome.lock().unwrap_or_else(|p| p.into_inner()) =
                                Some(format!("{err:#}"));
                            break;
                        }
                    };

                    if batch.is_empty() {
                        break;
                    }

                    for album in batch {
                        if cancel.load(Ordering::Relaxed) {
                            break;
                        }

                        let query = Query::new(&album.artist, &album.title);

                        match client.fetch(&query) {
                            Some(cover) => match art.store(&cover.bytes) {
                                Ok(art_id) => {
                                    if let Err(err) =
                                        enrich::attach_album_art(&connection, album.id, &art_id)
                                    {
                                        tracing::warn!("could not record a cover: {err:#}");
                                    }
                                    progress.found.fetch_add(1, Ordering::Relaxed);
                                }
                                Err(err) => {
                                    // A cover that will not decode is not a
                                    // cover. Counted as missing rather than
                                    // retried, so one bad file cannot wedge
                                    // the pass on the same album forever.
                                    tracing::warn!("a fetched cover would not decode: {err:#}");
                                    progress.missing.fetch_add(1, Ordering::Relaxed);
                                }
                            },
                            None => {
                                progress.missing.fetch_add(1, Ordering::Relaxed);
                            }
                        }

                        progress
                            .remaining
                            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
                                Some(left.saturating_sub(1))
                            })
                            .ok();

                        // A cover that landed while the window was idle drew
                        // no frame of its own. Without this the artwork simply
                        // does not appear until the user moves the mouse.
                        ctx.request_repaint();

                        std::thread::sleep(BREATH);
                    }
                }

                progress.finished.store(true, Ordering::Relaxed);
                ctx.request_repaint();
            });

        if let Err(err) = spawned {
            tracing::error!("could not start the artwork thread: {err}");
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

    /// Ask the thread to stop. It notices within one album.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn take_error(&self) -> Option<String> {
        self.outcome
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
    }
}

impl Drop for ArtworkJob {
    fn drop(&mut self) {
        // Closing the app, or switching the setting off, should not leave a
        // thread making requests nobody is waiting for.
        self.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_status_has_nothing_to_report() {
        let status = Status::default();
        assert_eq!(status.fraction(), None);
    }

    #[test]
    fn progress_counts_both_kinds_of_answer_as_done() {
        let status = Status {
            found: 3,
            missing: 1,
            remaining: 4,
            finished: false,
        };

        assert_eq!(status.fraction(), Some(0.5));
    }

    /// "40 looked up" reads as success on a library where only two had covers.
    /// The summary has to separate them.
    #[test]
    fn the_summary_separates_covers_found_from_albums_without_one() {
        let status = Status {
            found: 2,
            missing: 38,
            remaining: 10,
            finished: false,
        };

        let summary = status.summary();
        assert!(summary.contains("2 found"), "{summary}");
        assert!(summary.contains("38 with none"), "{summary}");
        assert!(summary.contains("10 to go"), "{summary}");
    }

    #[test]
    fn a_finished_pass_says_so() {
        let status = Status {
            found: 12,
            missing: 4,
            remaining: 0,
            finished: true,
        };

        let summary = status.summary();
        assert!(summary.starts_with("Finished."), "{summary}");
        assert!(summary.contains("12 covers found"), "{summary}");
        assert!(summary.contains("4 albums had none"), "{summary}");
    }

    /// A library where everything already has a cover should not report a
    /// division by zero or a full bar.
    #[test]
    fn nothing_to_do_is_not_progress() {
        let status = Status {
            found: 0,
            missing: 0,
            remaining: 0,
            finished: true,
        };

        assert_eq!(status.fraction(), None);
    }
}
