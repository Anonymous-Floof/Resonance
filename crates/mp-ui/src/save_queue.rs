//! Tracks waiting to be saved into the library, and how saving is going.
//!
//! Saving shares the one worker that fetches links, deliberately: one program
//! running at a time is gentler on the service and means two jobs can never
//! download the same video into the same cache file at once. So saves are
//! queued here and handed over whenever the worker is free, after anything the
//! listener is waiting on.
//!
//! Like `link_queue`, this is decisions only — no threads — so every rule is
//! tested without a network or a process.

use std::collections::{HashSet, VecDeque};

use crate::link_queue::{AfterMiss, MISSES_BEFORE_GIVING_UP};

/// One track to save.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveItem {
    pub video_id: String,
    /// For saying what is being saved before anything is known about it.
    pub title: String,
    /// The playlist it belongs to, which becomes the folder it is saved in.
    pub collection: Option<String>,
    /// An album name to use if the track's own answer has none.
    pub album: Option<String>,
}

/// Saves waiting, the one in progress, and a tally for the summary.
#[derive(Debug, Default)]
pub struct SaveQueue {
    pending: VecDeque<SaveItem>,
    in_flight: Option<SaveItem>,
    /// Everything asked for this session, so asking twice does nothing.
    /// A failure is taken back out, so it can be asked for again.
    asked: HashSet<String>,
    saved: usize,
    already: usize,
    failed: usize,
    misses_in_a_row: usize,
    /// The title of the only track in a batch of one, for a summary that
    /// names it.
    last_title: Option<String>,
}

impl SaveQueue {
    /// Ask for a track to be saved. `false` if it already has been, or is
    /// waiting to be.
    pub fn add(&mut self, item: SaveItem) -> bool {
        if !self.asked.insert(item.video_id.clone()) {
            return false;
        }

        self.pending.push_back(item);
        true
    }

    /// Ask for several. Returns how many were new.
    pub fn add_all(&mut self, items: impl IntoIterator<Item = SaveItem>) -> usize {
        items
            .into_iter()
            .filter(|item| self.add(item.clone()))
            .count()
    }

    /// Whether a video has been asked for this session.
    pub fn has_asked(&self, video_id: &str) -> bool {
        self.asked.contains(video_id)
    }

    /// The next track to save, if nothing is being saved already.
    pub fn next_save(&mut self) -> Option<SaveItem> {
        if self.in_flight.is_some() {
            return None;
        }

        let next = self.pending.pop_front()?;
        self.in_flight = Some(next.clone());
        Some(next)
    }

    pub fn is_saving(&self) -> bool {
        self.in_flight.is_some()
    }

    /// Nothing waiting and nothing in progress.
    pub fn is_idle(&self) -> bool {
        self.pending.is_empty() && self.in_flight.is_none()
    }

    /// The track in progress was saved, or was found already there.
    pub fn done(&mut self, newly_saved: bool, title: &str) {
        self.in_flight = None;
        self.misses_in_a_row = 0;
        self.last_title = Some(title.to_owned());

        if newly_saved {
            self.saved += 1;
        } else {
            self.already += 1;
        }
    }

    /// The track in progress could not be saved.
    ///
    /// Stops everything still waiting when the reason is not about that one
    /// track — a folder that cannot be written to, an out-of-date program — or
    /// when several in a row have failed, for the same reason a playlist does.
    pub fn failed(&mut self, blocks_the_rest: bool) -> AfterMiss {
        if let Some(item) = self.in_flight.take() {
            self.asked.remove(&item.video_id);
        }

        self.failed += 1;
        self.misses_in_a_row += 1;

        if blocks_the_rest || self.misses_in_a_row >= MISSES_BEFORE_GIVING_UP {
            self.stop();
            return AfterMiss::GiveUp;
        }

        AfterMiss::CarryOn
    }

    /// Drop everything still waiting. The one in progress finishes.
    pub fn stop(&mut self) {
        for item in self.pending.drain(..) {
            self.asked.remove(&item.video_id);
        }
    }

    /// Drop everything, including what was in progress, because nothing can
    /// finish it: the worker is gone, or there is nowhere left to save to.
    pub fn abandon(&mut self) {
        self.stop();
        if let Some(item) = self.in_flight.take() {
            self.asked.remove(&item.video_id);
        }
        self.saved = 0;
        self.already = 0;
        self.failed = 0;
        self.misses_in_a_row = 0;
    }

    /// A line saying how saving is going, while it is.
    pub fn status(&self) -> Option<String> {
        if self.is_idle() {
            return None;
        }

        let finished = self.saved + self.already + self.failed;
        let total = finished + self.pending.len() + usize::from(self.in_flight.is_some());

        let current = self
            .in_flight
            .as_ref()
            .map(|item| format!(": \u{201c}{}\u{201d}", item.title))
            .unwrap_or_default();

        Some(format!("Saving {} of {total}{current}", finished + 1))
    }

    /// Once everything asked for is done, one sentence about how it went, and
    /// the tally starts again.
    pub fn take_summary(&mut self) -> Option<String> {
        if !self.is_idle() {
            return None;
        }

        let total = self.saved + self.already + self.failed;
        if total == 0 {
            return None;
        }

        let summary = match (self.saved, self.already, self.failed) {
            (1, 0, 0) => format!(
                "Saved \u{201c}{}\u{201d} to Resonance Downloads.",
                self.last_title.as_deref().unwrap_or("the track")
            ),
            (0, 1, 0) => format!(
                "\u{201c}{}\u{201d} was already in Resonance Downloads, so it was left as it is.",
                self.last_title.as_deref().unwrap_or("The track")
            ),
            (saved, already, failed) => {
                let mut parts = vec![match saved {
                    1 => "Saved 1 track to Resonance Downloads.".to_owned(),
                    n => format!("Saved {n} tracks to Resonance Downloads."),
                }];
                match already {
                    0 => {}
                    1 => parts.push("1 was already there.".to_owned()),
                    n => parts.push(format!("{n} were already there.")),
                }
                match failed {
                    0 => {}
                    1 => parts.push("1 could not be saved.".to_owned()),
                    n => parts.push(format!("{n} could not be saved.")),
                }
                parts.join(" ")
            }
        };

        self.saved = 0;
        self.already = 0;
        self.failed = 0;
        self.misses_in_a_row = 0;
        self.last_title = None;

        Some(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str) -> SaveItem {
        SaveItem {
            video_id: id.to_owned(),
            title: format!("Title {id}"),
            collection: None,
            album: None,
        }
    }

    #[test]
    fn saves_happen_one_at_a_time_in_order() {
        let mut queue = SaveQueue::default();
        queue.add(item("a"));
        queue.add(item("b"));

        assert_eq!(queue.next_save().unwrap().video_id, "a");
        assert_eq!(queue.next_save(), None);

        queue.done(true, "Title a");
        assert_eq!(queue.next_save().unwrap().video_id, "b");
    }

    #[test]
    fn asking_twice_saves_once() {
        let mut queue = SaveQueue::default();

        assert!(queue.add(item("a")));
        assert!(!queue.add(item("a")));
        assert_eq!(queue.add_all([item("a"), item("b")]), 1);
    }

    /// Still asked after it is done, so the button stays "saved" and a second
    /// click does not queue it again.
    #[test]
    fn a_saved_track_stays_asked_for() {
        let mut queue = SaveQueue::default();
        queue.add(item("a"));
        queue.next_save();
        queue.done(true, "Title a");

        assert!(queue.has_asked("a"));
        assert!(!queue.add(item("a")));
    }

    #[test]
    fn a_failed_track_can_be_asked_for_again() {
        let mut queue = SaveQueue::default();
        queue.add(item("a"));
        queue.next_save();
        queue.failed(false);

        assert!(!queue.has_asked("a"));
        assert!(queue.add(item("a")));
    }

    #[test]
    fn a_failure_that_is_not_about_the_track_stops_the_rest() {
        let mut queue = SaveQueue::default();
        queue.add_all([item("a"), item("b"), item("c")]);
        queue.next_save();

        assert_eq!(queue.failed(true), AfterMiss::GiveUp);
        assert!(queue.is_idle());
        assert!(
            !queue.has_asked("b"),
            "stopped tracks can be asked for again"
        );
    }

    #[test]
    fn too_many_failures_in_a_row_stop_the_rest() {
        let mut queue = SaveQueue::default();
        queue.add_all([item("a"), item("b"), item("c"), item("d")]);

        for _ in 0..MISSES_BEFORE_GIVING_UP - 1 {
            queue.next_save();
            assert_eq!(queue.failed(false), AfterMiss::CarryOn);
        }

        queue.next_save();
        assert_eq!(queue.failed(false), AfterMiss::GiveUp);
        assert!(queue.is_idle());
    }

    #[test]
    fn stopping_lets_the_one_in_progress_finish() {
        let mut queue = SaveQueue::default();
        queue.add_all([item("a"), item("b")]);
        queue.next_save();

        queue.stop();

        assert!(queue.is_saving());
        assert_eq!(queue.next_save(), None);
        queue.done(true, "Title a");
        assert!(queue.is_idle());
    }

    #[test]
    fn abandoning_forgets_even_the_one_in_progress() {
        let mut queue = SaveQueue::default();
        queue.add_all([item("a"), item("b")]);
        queue.next_save();

        queue.abandon();

        assert!(queue.is_idle());
        assert!(!queue.has_asked("a"));
        assert_eq!(queue.take_summary(), None);
    }

    #[test]
    fn the_status_counts_through_the_batch() {
        let mut queue = SaveQueue::default();
        queue.add_all([item("a"), item("b"), item("c")]);

        queue.next_save();
        assert_eq!(
            queue.status().as_deref(),
            Some("Saving 1 of 3: \u{201c}Title a\u{201d}")
        );

        queue.done(true, "Title a");
        queue.next_save();
        assert!(queue.status().unwrap().starts_with("Saving 2 of 3"));
    }

    #[test]
    fn nothing_to_say_while_idle() {
        let mut queue = SaveQueue::default();

        assert_eq!(queue.status(), None);
        assert_eq!(queue.take_summary(), None);
    }

    #[test]
    fn one_track_is_named_in_the_summary() {
        let mut queue = SaveQueue::default();
        queue.add(item("a"));
        queue.next_save();
        queue.done(true, "Together Forever");

        assert_eq!(
            queue.take_summary().as_deref(),
            Some("Saved \u{201c}Together Forever\u{201d} to Resonance Downloads.")
        );
        assert_eq!(queue.take_summary(), None, "said once");
    }

    #[test]
    fn a_batch_is_counted_in_the_summary() {
        let mut queue = SaveQueue::default();
        queue.add_all([item("a"), item("b"), item("c"), item("d")]);

        queue.next_save();
        queue.done(true, "a");
        queue.next_save();
        queue.done(true, "b");
        queue.next_save();
        queue.done(false, "c");
        queue.next_save();
        queue.failed(false);

        assert_eq!(
            queue.take_summary().as_deref(),
            Some(
                "Saved 2 tracks to Resonance Downloads. 1 was already there. 1 could not be saved."
            )
        );
    }

    #[test]
    fn no_summary_until_the_batch_is_done() {
        let mut queue = SaveQueue::default();
        queue.add_all([item("a"), item("b")]);
        queue.next_save();
        queue.done(true, "a");

        assert_eq!(queue.take_summary(), None);
    }
}
