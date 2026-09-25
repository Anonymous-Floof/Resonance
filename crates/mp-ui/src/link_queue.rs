//! A playlist, played from a link, fetched as it goes.
//!
//! The engine's queue is a list of files, and a playlist's tracks are not files
//! until they are fetched. Fetching all of them first would mean a long wait
//! and a cache full of tracks nobody reached; fetching none ahead would mean a
//! gap between every one. So this keeps exactly one track ready behind the one
//! playing: when a track starts, the next is fetched while it plays.
//!
//! No threads and no clock here — only decisions. The caller reports what
//! happened (a track landed, a fetch missed, the player moved on) and asks what
//! to do next, which is what lets every rule below be tested without a
//! network, a process or a sound device.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use mp_net::youtube::{Listed, Listing};

/// How many fetches in a row may miss before the rest of the list is given up
/// on. One gone video is ordinary; several in a row is almost always something
/// that will go on failing, and stopping says so once instead of once per
/// track.
pub const MISSES_BEFORE_GIVING_UP: usize = 3;

/// What to do with a track that has just been fetched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handoff {
    /// Replace whatever is queued and start it: the first track of the list,
    /// or one that arrived after the player had already run out.
    PlayNow,
    /// Add it behind what is playing.
    Enqueue,
}

/// What a miss means for the rest of the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterMiss {
    /// Skip that one and carry on.
    CarryOn,
    /// Stop: whatever went wrong will go wrong for the rest too.
    GiveUp,
}

/// One playlist in progress.
#[derive(Debug)]
pub struct LinkQueue {
    title: String,
    /// The album's name, when the list is one. Offered to a track whose own
    /// answer names no album.
    album: Option<String>,
    upcoming: VecDeque<Listed>,
    total: usize,
    /// How many have been given to the player so far.
    handed: usize,
    /// Being fetched now.
    in_flight: Option<Listed>,
    /// Given to the player and not yet playing.
    ready: Option<PathBuf>,
    /// The last of this list's tracks to start playing. While nothing is
    /// ready, this is what says the list is still what the user is hearing.
    current: Option<PathBuf>,
    /// Whether `ready` has been seen in the player's queue. Until it has, its
    /// absence means the engine has not caught up yet rather than that the
    /// user removed it.
    ready_seen: bool,
    misses_in_a_row: usize,
}

impl LinkQueue {
    /// Start a list, from the video the link named if it named one.
    ///
    /// Everything before that video is left out, which is what opening a
    /// playlist at a video means on YouTube too. A named video the list does
    /// not contain — beyond the listing ceiling, or added since — goes first
    /// rather than being dropped, because it is the one the user pointed at.
    pub fn new(listing: Listing, start_at: Option<&str>) -> Self {
        let album = listing.is_album().then(|| listing.title.clone());
        let title = listing.title.clone();

        let mut upcoming: VecDeque<Listed> = match start_at {
            Some(video_id) => match listing.position_of(video_id) {
                Some(at) => listing.entries.into_iter().skip(at).collect(),
                None => {
                    let mut entries: VecDeque<Listed> = listing.entries.into();
                    entries.push_front(Listed {
                        video_id: video_id.to_owned(),
                        title: video_id.to_owned(),
                        artist: String::new(),
                        duration: None,
                    });
                    entries
                }
            },
            None => listing.entries.into(),
        };
        upcoming.shrink_to_fit();

        Self {
            title,
            album,
            total: upcoming.len(),
            upcoming,
            handed: 0,
            in_flight: None,
            ready: None,
            current: None,
            ready_seen: false,
            misses_in_a_row: 0,
        }
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn album(&self) -> Option<&str> {
        self.album.as_deref()
    }

    pub fn total(&self) -> usize {
        self.total
    }

    /// The next video to fetch, if one is due.
    ///
    /// Due only when nothing is being fetched and nothing is waiting to play:
    /// one track ahead, never more. Taking it marks it as in flight, so asking
    /// twice in a frame does not fetch twice.
    pub fn next_fetch(&mut self) -> Option<Listed> {
        if self.in_flight.is_some() || self.ready.is_some() {
            return None;
        }

        let next = self.upcoming.pop_front()?;
        self.in_flight = Some(next.clone());
        Some(next)
    }

    /// Whether a fetch for this list is out.
    pub fn is_fetching(&self) -> bool {
        self.in_flight.is_some()
    }

    /// The title of the video being fetched, for saying which one missed.
    pub fn fetching_title(&self) -> Option<&str> {
        self.in_flight.as_ref().map(|entry| entry.title.as_str())
    }

    /// The track given to the player and not yet playing.
    pub fn ready(&self) -> Option<&Path> {
        self.ready.as_deref()
    }

    /// A fetch came back with a file.
    ///
    /// `player_idle` is whether the player has anything playing. If it ran out
    /// while this was being fetched, adding to the end of a finished queue
    /// would leave the track sitting there unplayed.
    pub fn landed(&mut self, path: PathBuf, player_idle: bool) -> Handoff {
        self.in_flight = None;
        self.misses_in_a_row = 0;
        self.handed += 1;

        let handoff = if self.handed == 1 || player_idle {
            Handoff::PlayNow
        } else {
            Handoff::Enqueue
        };

        self.ready = Some(path);
        self.ready_seen = false;
        handoff
    }

    /// A fetch came back with nothing.
    ///
    /// `blocks_the_rest` is for failures that are about the program rather
    /// than the video — an out-of-date `yt-dlp`, one that will not run — where
    /// trying the next track would only fail the same way.
    pub fn missed(&mut self, blocks_the_rest: bool) -> AfterMiss {
        self.in_flight = None;
        self.misses_in_a_row += 1;

        if blocks_the_rest || self.misses_in_a_row >= MISSES_BEFORE_GIVING_UP {
            self.upcoming.clear();
            return AfterMiss::GiveUp;
        }

        AfterMiss::CarryOn
    }

    /// Look at the player, once a frame.
    ///
    /// `now_playing` is the path playing now, and `is_queued` says whether a
    /// path is anywhere in the player's queue. Returns `false` when the list
    /// has been abandoned — the user played something else, cleared the
    /// queue, or took this list's next track out of it — and should be
    /// dropped. Carrying on regardless would push tracks into a queue the user
    /// has just replaced.
    pub fn observe(
        &mut self,
        now_playing: Option<&Path>,
        is_queued: impl Fn(&Path) -> bool,
    ) -> bool {
        if let Some(ready) = &self.ready {
            if now_playing == Some(ready.as_path()) {
                // Playing, so the next one is due.
                self.current = self.ready.take();
                self.ready_seen = false;
                return true;
            }

            if is_queued(ready) {
                self.ready_seen = true;
                return true;
            }

            // Gone from a queue it had reached. The engine republishes the
            // queue a moment after being told to add to it, so absence before
            // that is just lag.
            return !self.ready_seen;
        }

        // Nothing is waiting, which is the usual state while the next track
        // is being fetched. The list is still the user's for as long as the
        // track it last started is in the queue; a queue replaced by an album
        // no longer has it.
        self.current.as_deref().is_none_or(is_queued)
    }

    /// Whether anything is left to do.
    pub fn is_finished(&self) -> bool {
        self.upcoming.is_empty() && self.in_flight.is_none() && self.ready.is_none()
    }

    /// How many are still to be fetched, not counting one in flight.
    pub fn remaining(&self) -> usize {
        self.upcoming.len()
    }

    /// A line for the queue panel about what is not in it yet.
    ///
    /// `None` once there is nothing more to come, so the panel does not carry
    /// a sentence about a list that has finished.
    pub fn upcoming_note(&self) -> Option<String> {
        let left = self.upcoming.len() + usize::from(self.in_flight.is_some());
        match left {
            0 => None,
            1 => Some(format!(
                "1 more from \u{201c}{}\u{201d}, fetched when its turn comes.",
                self.title
            )),
            n => Some(format!(
                "{n} more from \u{201c}{}\u{201d}, each fetched when its turn comes.",
                self.title
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn listed(id: &str) -> Listed {
        Listed {
            video_id: id.to_owned(),
            title: format!("Title {id}"),
            artist: "Artist".to_owned(),
            duration: Some(Duration::from_secs(200)),
        }
    }

    fn listing(ids: &[&str]) -> Listing {
        Listing {
            id: "PLtest".to_owned(),
            title: "Road trip".to_owned(),
            entries: ids.iter().map(|id| listed(id)).collect(),
        }
    }

    fn ids(queue: &LinkQueue) -> Vec<String> {
        queue
            .upcoming
            .iter()
            .map(|entry| entry.video_id.clone())
            .collect()
    }

    fn path(id: &str) -> PathBuf {
        PathBuf::from(format!("cache/{id}.m4a"))
    }

    #[test]
    fn a_list_starts_from_the_top() {
        let queue = LinkQueue::new(listing(&["aaaaaaaaaaa", "bbbbbbbbbbb"]), None);

        assert_eq!(ids(&queue), ["aaaaaaaaaaa", "bbbbbbbbbbb"]);
        assert_eq!(queue.total(), 2);
    }

    #[test]
    fn a_named_video_starts_the_list_there() {
        let queue = LinkQueue::new(
            listing(&["aaaaaaaaaaa", "bbbbbbbbbbb", "ccccccccccc"]),
            Some("bbbbbbbbbbb"),
        );

        assert_eq!(ids(&queue), ["bbbbbbbbbbb", "ccccccccccc"]);
        assert_eq!(queue.total(), 2);
    }

    /// The video the user pointed at is the one they want first, even when the
    /// list as read does not contain it.
    #[test]
    fn a_named_video_missing_from_the_list_goes_first() {
        let queue = LinkQueue::new(listing(&["aaaaaaaaaaa"]), Some("zzzzzzzzzzz"));

        assert_eq!(ids(&queue), ["zzzzzzzzzzz", "aaaaaaaaaaa"]);
    }

    #[test]
    fn only_one_fetch_is_ever_out() {
        let mut queue = LinkQueue::new(listing(&["aaaaaaaaaaa", "bbbbbbbbbbb"]), None);

        assert_eq!(queue.next_fetch().unwrap().video_id, "aaaaaaaaaaa");
        assert_eq!(queue.next_fetch(), None);
        assert!(queue.is_fetching());
    }

    #[test]
    fn the_first_track_plays_at_once_and_the_rest_queue_behind() {
        let mut queue = LinkQueue::new(listing(&["aaaaaaaaaaa", "bbbbbbbbbbb"]), None);

        queue.next_fetch();
        assert_eq!(queue.landed(path("aaaaaaaaaaa"), true), Handoff::PlayNow);
        assert!(queue.observe(Some(&path("aaaaaaaaaaa")), |_| true));

        queue.next_fetch();
        assert_eq!(queue.landed(path("bbbbbbbbbbb"), false), Handoff::Enqueue);
    }

    /// One ahead, never more: the track after next is not fetched until the
    /// next has actually started.
    #[test]
    fn nothing_more_is_fetched_until_the_ready_track_starts() {
        let mut queue = LinkQueue::new(
            listing(&["aaaaaaaaaaa", "bbbbbbbbbbb", "ccccccccccc"]),
            None,
        );

        queue.next_fetch();
        queue.landed(path("aaaaaaaaaaa"), true);
        queue.observe(Some(&path("aaaaaaaaaaa")), |_| true);

        queue.next_fetch();
        queue.landed(path("bbbbbbbbbbb"), false);
        queue.observe(Some(&path("aaaaaaaaaaa")), |_| true);
        assert_eq!(queue.next_fetch(), None);

        queue.observe(Some(&path("bbbbbbbbbbb")), |_| true);
        assert_eq!(queue.next_fetch().unwrap().video_id, "ccccccccccc");
    }

    /// Fetching lost the race with a short track. Queued onto a finished
    /// queue the new track would never play.
    #[test]
    fn a_track_that_arrives_after_the_player_ran_out_plays_now() {
        let mut queue = LinkQueue::new(listing(&["aaaaaaaaaaa", "bbbbbbbbbbb"]), None);

        queue.next_fetch();
        queue.landed(path("aaaaaaaaaaa"), true);
        queue.observe(Some(&path("aaaaaaaaaaa")), |_| true);
        queue.next_fetch();

        assert_eq!(queue.landed(path("bbbbbbbbbbb"), true), Handoff::PlayNow);
    }

    #[test]
    fn a_video_that_is_gone_is_skipped() {
        let mut queue = LinkQueue::new(listing(&["aaaaaaaaaaa", "bbbbbbbbbbb"]), None);

        queue.next_fetch();
        assert_eq!(queue.missed(false), AfterMiss::CarryOn);
        assert_eq!(queue.next_fetch().unwrap().video_id, "bbbbbbbbbbb");
    }

    /// An out-of-date program fails every track the same way. Saying so once
    /// beats a notice per track for the length of a playlist.
    #[test]
    fn a_failure_that_is_about_the_program_stops_the_list() {
        let mut queue = LinkQueue::new(listing(&["aaaaaaaaaaa", "bbbbbbbbbbb"]), None);

        queue.next_fetch();
        assert_eq!(queue.missed(true), AfterMiss::GiveUp);
        assert_eq!(queue.next_fetch(), None);
        assert!(queue.is_finished());
    }

    #[test]
    fn too_many_misses_in_a_row_stop_the_list() {
        let mut queue = LinkQueue::new(
            listing(&["aaaaaaaaaaa", "bbbbbbbbbbb", "ccccccccccc", "ddddddddddd"]),
            None,
        );

        for _ in 0..MISSES_BEFORE_GIVING_UP - 1 {
            queue.next_fetch();
            assert_eq!(queue.missed(false), AfterMiss::CarryOn);
        }

        queue.next_fetch();
        assert_eq!(queue.missed(false), AfterMiss::GiveUp);
        assert!(queue.is_finished());
    }

    #[test]
    fn a_success_resets_the_count_of_misses() {
        let mut queue = LinkQueue::new(
            listing(&[
                "aaaaaaaaaaa",
                "bbbbbbbbbbb",
                "ccccccccccc",
                "ddddddddddd",
                "eeeeeeeeeee",
            ]),
            None,
        );

        queue.next_fetch();
        queue.missed(false);
        queue.next_fetch();
        queue.missed(false);
        queue.next_fetch();
        queue.landed(path("ccccccccccc"), true);
        queue.observe(Some(&path("ccccccccccc")), |_| true);

        queue.next_fetch();
        assert_eq!(queue.missed(false), AfterMiss::CarryOn);
    }

    /// The engine republishes its queue a frame or so after an add. Until the
    /// ready track has been seen there, not seeing it means nothing.
    #[test]
    fn a_ready_track_not_yet_in_the_queue_is_not_an_abandonment() {
        let mut queue = LinkQueue::new(listing(&["aaaaaaaaaaa", "bbbbbbbbbbb"]), None);

        queue.next_fetch();
        queue.landed(path("aaaaaaaaaaa"), true);

        assert!(queue.observe(None, |_| false));
    }

    /// The user played an album, or cleared the queue, or removed the next
    /// track. Whatever they did, the list is no longer what is playing.
    #[test]
    fn a_ready_track_that_leaves_the_queue_ends_the_list() {
        let mut queue = LinkQueue::new(
            listing(&["aaaaaaaaaaa", "bbbbbbbbbbb", "ccccccccccc"]),
            None,
        );

        queue.next_fetch();
        queue.landed(path("aaaaaaaaaaa"), true);
        queue.observe(Some(&path("aaaaaaaaaaa")), |_| true);
        queue.next_fetch();
        queue.landed(path("bbbbbbbbbbb"), false);

        assert!(queue.observe(Some(&path("aaaaaaaaaaa")), |_| true));
        assert!(!queue.observe(Some(Path::new("library/song.flac")), |_| false));
    }

    /// The usual moment for it: the next track is being fetched, so there is
    /// nothing waiting to go missing, and the user plays an album. The track
    /// that was playing leaves the queue, and that is enough.
    #[test]
    fn replacing_the_queue_mid_fetch_ends_the_list() {
        let mut queue = LinkQueue::new(listing(&["aaaaaaaaaaa", "bbbbbbbbbbb"]), None);

        queue.next_fetch();
        queue.landed(path("aaaaaaaaaaa"), true);
        queue.observe(Some(&path("aaaaaaaaaaa")), |_| true);
        queue.next_fetch();

        let playing = path("aaaaaaaaaaa");
        assert!(queue.observe(Some(&playing), |p| p == playing));
        assert!(!queue.observe(Some(Path::new("library/song.flac")), |_| false));
    }

    /// Running out before the next track arrived is not the user leaving: a
    /// finished queue still holds what it played.
    #[test]
    fn a_player_that_ran_out_mid_fetch_keeps_the_list() {
        let mut queue = LinkQueue::new(listing(&["aaaaaaaaaaa", "bbbbbbbbbbb"]), None);

        queue.next_fetch();
        queue.landed(path("aaaaaaaaaaa"), true);
        queue.observe(Some(&path("aaaaaaaaaaa")), |_| true);
        queue.next_fetch();

        let played = path("aaaaaaaaaaa");
        assert!(queue.observe(None, |p| p == played));
    }

    #[test]
    fn a_list_is_finished_when_the_last_track_starts() {
        let mut queue = LinkQueue::new(listing(&["aaaaaaaaaaa"]), None);

        assert!(!queue.is_finished());
        queue.next_fetch();
        assert!(!queue.is_finished());
        queue.landed(path("aaaaaaaaaaa"), true);
        assert!(!queue.is_finished());
        queue.observe(Some(&path("aaaaaaaaaaa")), |_| true);
        assert!(queue.is_finished());
    }

    #[test]
    fn the_note_counts_what_is_still_to_come() {
        let mut queue = LinkQueue::new(
            listing(&["aaaaaaaaaaa", "bbbbbbbbbbb", "ccccccccccc"]),
            None,
        );

        assert_eq!(
            queue.upcoming_note().as_deref(),
            Some("3 more from \u{201c}Road trip\u{201d}, each fetched when its turn comes.")
        );

        // One in flight still counts: it is not in the queue yet.
        queue.next_fetch();
        assert!(queue.upcoming_note().unwrap().starts_with("3 more"));

        queue.landed(path("aaaaaaaaaaa"), true);
        assert!(queue.upcoming_note().unwrap().starts_with("2 more"));
    }

    #[test]
    fn nothing_left_to_come_says_nothing() {
        let mut queue = LinkQueue::new(listing(&["aaaaaaaaaaa"]), None);

        queue.next_fetch();
        queue.landed(path("aaaaaaaaaaa"), true);

        assert_eq!(queue.upcoming_note(), None);
    }

    #[test]
    fn an_album_offers_its_name_and_a_playlist_does_not() {
        let mut album = listing(&["aaaaaaaaaaa"]);
        album.id = "OLAK5uy_lR6ovo0isWA_63d1PrY9oCjpYPbrUn2GQ".to_owned();
        album.title = "Are We There Yet?".to_owned();

        assert_eq!(
            LinkQueue::new(album, None).album(),
            Some("Are We There Yet?")
        );
        assert_eq!(
            LinkQueue::new(listing(&["aaaaaaaaaaa"]), None).album(),
            None
        );
    }
}
