//! The playback queue: what plays now, next, and after that.
//!
//! Shuffle is implemented as a *permutation of the queue* rather than by
//! picking a random track each time. That distinction is what makes "previous"
//! work correctly while shuffled, and guarantees every track plays once before
//! any repeats — the behaviour people actually expect, and the thing naive
//! shuffle gets wrong.

use std::path::{Path, PathBuf};

use mp_core::config::{RepeatMode, ShuffleMode};

/// A deterministic, dependency-free PRNG.
///
/// Shuffling a playlist does not need cryptographic randomness, and this avoids
/// pulling `rand` into the audio crate for one use.
struct Rng(u64);

impl Rng {
    fn from_entropy() -> Self {
        // Seeded from the clock; collisions across runs are harmless here.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0x2545_F491_4F6C_DD1D, |d| d.as_nanos() as u64);
        Self::seeded(nanos)
    }

    /// A reproducible generator.
    ///
    /// Exists so shuffle behaviour can be tested against a known ordering. A
    /// test that shuffles from the clock and then asserts a bound on the result
    /// is asserting a probability, and will eventually fail on a correct
    /// implementation for no reason anyone can reproduce.
    fn seeded(seed: u64) -> Self {
        // The low bit is forced because xorshift has one fixed point: zero
        // maps to zero and the sequence never starts.
        Self(seed | 1)
    }

    /// xorshift64*, good enough for shuffling and very cheap.
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            return 0;
        }
        (self.next_u64() % bound as u64) as usize
    }
}

/// An ordered list of tracks plus a cursor.
pub struct Queue {
    /// Every track, in the order it was added.
    tracks: Vec<PathBuf>,

    /// Indices into `tracks` giving the play order.
    ///
    /// Identity when shuffle is off, a permutation when it is on.
    order: Vec<usize>,

    /// Position within `order`, not within `tracks`.
    cursor: usize,

    shuffle: ShuffleMode,
    repeat: RepeatMode,
    rng: Rng,
}

impl Default for Queue {
    fn default() -> Self {
        Self::new()
    }
}

impl Queue {
    pub fn new() -> Self {
        Self {
            tracks: Vec::new(),
            order: Vec::new(),
            cursor: 0,
            shuffle: ShuffleMode::Off,
            repeat: RepeatMode::Off,
            rng: Rng::from_entropy(),
        }
    }

    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    pub fn tracks(&self) -> &[PathBuf] {
        &self.tracks
    }

    /// The play order as indices into [`tracks`](Self::tracks).
    pub fn order(&self) -> &[usize] {
        &self.order
    }

    /// Position of the current track within the play order.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The track that should be playing, if any.
    pub fn current(&self) -> Option<&Path> {
        self.order
            .get(self.cursor)
            .and_then(|&i| self.tracks.get(i))
            .map(PathBuf::as_path)
    }

    /// Index into `tracks` of the current entry, for highlighting a list.
    pub fn current_index(&self) -> Option<usize> {
        self.order.get(self.cursor).copied()
    }

    /// Replace the queue and start at `start` (an index into `tracks`).
    pub fn replace(&mut self, tracks: Vec<PathBuf>, start: usize) {
        self.tracks = tracks;

        // When shuffled, the chosen track still plays first; the rest are
        // shuffled around it. Starting a shuffled queue by jumping to a random
        // song would ignore what the user actually clicked.
        let pinned = (self.shuffle != ShuffleMode::Off).then_some(start);
        self.rebuild_order(pinned);

        self.cursor = if pinned.is_some() {
            0
        } else {
            self.order.iter().position(|&i| i == start).unwrap_or(0)
        };
    }

    /// Append tracks, preserving the current position.
    pub fn extend(&mut self, tracks: impl IntoIterator<Item = PathBuf>) {
        let first_new = self.tracks.len();
        self.tracks.extend(tracks);

        let added: Vec<usize> = (first_new..self.tracks.len()).collect();
        if added.is_empty() {
            return;
        }

        if self.shuffle == ShuffleMode::Off {
            self.order.extend(added);
        } else {
            // Scatter new tracks through the part of the order not yet played,
            // so adding to a shuffled queue does not simply tack them on the end.
            for index in added {
                let lower = self.cursor + 1;
                let at = if lower >= self.order.len() {
                    self.order.len()
                } else {
                    lower + self.rng.below(self.order.len() - lower + 1)
                };
                self.order.insert(at.min(self.order.len()), index);
            }
        }
    }

    /// Insert tracks so they play immediately after the current one.
    pub fn play_next(&mut self, tracks: impl IntoIterator<Item = PathBuf>) {
        let first_new = self.tracks.len();
        self.tracks.extend(tracks);

        let start = (self.cursor + 1).min(self.order.len());
        for (offset, index) in (first_new..self.tracks.len()).enumerate() {
            self.order.insert(start + offset, index);
        }
    }

    pub fn clear(&mut self) {
        self.tracks.clear();
        self.order.clear();
        self.cursor = 0;
    }

    // -- modes -------------------------------------------------------------

    pub fn shuffle(&self) -> ShuffleMode {
        self.shuffle
    }

    /// Change shuffle mode, keeping the currently playing track current.
    pub fn set_shuffle(&mut self, mode: ShuffleMode) {
        if self.shuffle == mode {
            return;
        }

        let playing = self.current_index();
        self.shuffle = mode;

        if mode == ShuffleMode::Off {
            // Back to the natural order. The playing track keeps playing, but
            // it does so from wherever it actually sits in the album rather
            // than being dragged to the front of it — turning shuffle off
            // should give you the record back, not a rotation of it.
            self.rebuild_order(None);
            self.cursor = playing
                .and_then(|playing| self.order.iter().position(|&i| i == playing))
                .unwrap_or(0);
            return;
        }

        // Pinning happens inside the rebuild, before the spacing pass. Without
        // this the track would jump the moment shuffle was toggled.
        self.rebuild_order(playing);
        if playing.is_some() {
            self.cursor = 0;
        }
    }

    pub fn repeat(&self) -> RepeatMode {
        self.repeat
    }

    pub fn set_repeat(&mut self, mode: RepeatMode) {
        self.repeat = mode;
    }

    // -- movement ----------------------------------------------------------

    /// Advance because the current track finished on its own.
    ///
    /// Distinct from [`next`](Self::next): repeat-one only loops here, never
    /// when the user presses the skip button.
    pub fn advance_after_playback(&mut self) -> Option<&Path> {
        match self.repeat {
            RepeatMode::One => self.current(),
            _ => self.step_forward(),
        }
    }

    /// Skip forward at the user's request.
    ///
    /// Named for what a music queue does, not for `Iterator`: a queue is not an
    /// iterator, and `queue.next()` is what every caller expects to write.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<&Path> {
        self.step_forward()
    }

    fn step_forward(&mut self) -> Option<&Path> {
        if self.order.is_empty() {
            return None;
        }

        if self.cursor + 1 < self.order.len() {
            self.cursor += 1;
            return self.current();
        }

        match self.repeat {
            RepeatMode::Off => None,
            RepeatMode::One | RepeatMode::All => {
                // Reshuffle on wrap so the second pass is not identical to the
                // first, then start over.
                if self.shuffle != ShuffleMode::Off {
                    self.rebuild_order(None);
                }
                self.cursor = 0;
                self.current()
            }
        }
    }

    /// Step back, wrapping to the end when repeating.
    pub fn previous(&mut self) -> Option<&Path> {
        if self.order.is_empty() {
            return None;
        }

        if self.cursor > 0 {
            self.cursor -= 1;
        } else if self.repeat == RepeatMode::All {
            self.cursor = self.order.len() - 1;
        }

        self.current()
    }

    /// Jump to a specific track by its index into `tracks`.
    pub fn jump_to(&mut self, index: usize) -> Option<&Path> {
        let at = self.order.iter().position(|&i| i == index)?;
        self.cursor = at;
        self.current()
    }

    /// A queue whose shuffle is reproducible, for tests.
    #[cfg(test)]
    fn seeded(seed: u64) -> Self {
        Self {
            rng: Rng::seeded(seed),
            ..Self::new()
        }
    }

    /// Rebuild the play order for the current shuffle mode.
    ///
    /// `pinned` is a track index to place first — the song the user clicked, or
    /// the one already playing when shuffle was switched on.
    ///
    /// The pinning happens here, between the shuffle and the spacing pass,
    /// rather than at the call site afterwards. Moving a track to the front
    /// once the order is settled displaces whatever was there into the middle,
    /// which can recreate exactly the same-folder adjacency the spacing pass
    /// had just removed. It measurably did: over two hundred orderings that
    /// took the average from about 0.3 clumped pairs to about 1.0.
    fn rebuild_order(&mut self, pinned: Option<usize>) {
        self.order = (0..self.tracks.len()).collect();

        if self.shuffle == ShuffleMode::Off {
            return;
        }

        // Fisher-Yates.
        for i in (1..self.order.len()).rev() {
            let j = self.rng.below(i + 1);
            self.order.swap(i, j);
        }

        if let Some(pinned) = pinned
            && let Some(at) = self.order.iter().position(|&i| i == pinned)
        {
            self.order.swap(0, at);
        }

        // `Smart` shuffle spaces out tracks from the same folder, which is the
        // best available proxy for "same artist" until the library lands in M2
        // and real tag data is available. It only ever moves entries after the
        // first, so the pinned track stays where it was put.
        if self.shuffle == ShuffleMode::Smart {
            self.space_out_neighbours();
        }
    }

    /// Push apart adjacent entries that share a parent folder.
    ///
    /// A single pass is enough to break up the obvious clumps without the cost
    /// of a full constraint solve.
    fn space_out_neighbours(&mut self) {
        if self.order.len() < 3 {
            return;
        }

        // Resolved up front: the swap loop below needs `self.order` mutably,
        // so it cannot also hold a borrow of `self.tracks`.
        let parents: Vec<Option<&Path>> = self.tracks.iter().map(|p| p.parent()).collect();
        let parent_of = |index: usize| parents.get(index).copied().flatten();

        let mut order = std::mem::take(&mut self.order);

        for i in 1..order.len() {
            if parent_of(order[i]) != parent_of(order[i - 1]) {
                continue;
            }

            // Find a later track from a different folder and swap it in.
            let swap_with =
                (i + 1..order.len()).find(|&j| parent_of(order[j]) != parent_of(order[i - 1]));

            if let Some(j) = swap_with {
                order.swap(i, j);
            }
        }

        self.order = order;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    fn queue_of(names: &[&str]) -> Queue {
        let mut q = Queue::new();
        q.replace(paths(names), 0);
        q
    }

    #[test]
    fn plays_in_order_and_stops_at_the_end() {
        let mut q = queue_of(&["a", "b", "c"]);
        assert_eq!(q.current(), Some(Path::new("a")));
        assert_eq!(q.next(), Some(Path::new("b")));
        assert_eq!(q.next(), Some(Path::new("c")));
        assert_eq!(q.next(), None, "repeat is off, so the queue ends");
    }

    #[test]
    fn starts_at_the_track_that_was_clicked() {
        let mut q = Queue::new();
        q.replace(paths(&["a", "b", "c"]), 2);
        assert_eq!(q.current(), Some(Path::new("c")));
    }

    #[test]
    fn repeat_all_wraps_around() {
        let mut q = queue_of(&["a", "b"]);
        q.set_repeat(RepeatMode::All);

        assert_eq!(q.next(), Some(Path::new("b")));
        assert_eq!(q.next(), Some(Path::new("a")));
    }

    /// Repeat-one should hold on a track when it ends naturally, but must not
    /// trap the user when they press skip.
    #[test]
    fn repeat_one_loops_playback_but_not_the_skip_button() {
        let mut q = queue_of(&["a", "b"]);
        q.set_repeat(RepeatMode::One);

        assert_eq!(q.advance_after_playback(), Some(Path::new("a")));
        assert_eq!(q.next(), Some(Path::new("b")));
    }

    #[test]
    fn previous_steps_back_and_holds_at_the_start() {
        let mut q = queue_of(&["a", "b", "c"]);
        q.next();
        assert_eq!(q.previous(), Some(Path::new("a")));
        assert_eq!(q.previous(), Some(Path::new("a")), "no wrap without repeat");
    }

    /// The defining property of permutation shuffle: everything plays once
    /// before anything plays twice.
    #[test]
    fn shuffle_covers_every_track_exactly_once() {
        let names = ["a", "b", "c", "d", "e", "f", "g", "h"];
        let mut q = queue_of(&names);
        q.set_shuffle(ShuffleMode::Random);

        let mut seen = vec![q.current().unwrap().to_path_buf()];
        while let Some(track) = q.next() {
            seen.push(track.to_path_buf());
        }

        assert_eq!(seen.len(), names.len());
        let mut unique = seen.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            unique.len(),
            names.len(),
            "a track repeated before the queue was exhausted"
        );
    }

    #[test]
    fn toggling_shuffle_keeps_the_current_track_playing() {
        let mut q = queue_of(&["a", "b", "c", "d", "e"]);
        q.next();
        let playing = q.current().unwrap().to_path_buf();

        q.set_shuffle(ShuffleMode::Random);
        assert_eq!(q.current().unwrap(), playing.as_path());

        q.set_shuffle(ShuffleMode::Off);
        assert_eq!(q.current().unwrap(), playing.as_path());
    }

    #[test]
    fn play_next_jumps_the_line() {
        let mut q = queue_of(&["a", "b", "c"]);
        q.play_next(paths(&["urgent"]));

        assert_eq!(q.current(), Some(Path::new("a")));
        assert_eq!(q.next(), Some(Path::new("urgent")));
        assert_eq!(q.next(), Some(Path::new("b")));
    }

    #[test]
    fn extend_appends_without_disturbing_playback() {
        let mut q = queue_of(&["a", "b"]);
        q.next();
        q.extend(paths(&["c"]));

        assert_eq!(q.current(), Some(Path::new("b")));
        assert_eq!(q.next(), Some(Path::new("c")));
    }

    #[test]
    fn jump_to_selects_by_track_index() {
        let mut q = queue_of(&["a", "b", "c"]);
        assert_eq!(q.jump_to(2), Some(Path::new("c")));
        assert_eq!(q.current_index(), Some(2));
    }

    #[test]
    fn an_empty_queue_has_nothing_to_play() {
        let mut q = Queue::new();
        assert!(q.is_empty());
        assert_eq!(q.current(), None);
        assert_eq!(q.next(), None);
        assert_eq!(q.previous(), None);
    }

    /// Four folders of six tracks each, the fixture both shuffle tests use.
    fn clumpable_tracks() -> Vec<PathBuf> {
        let mut tracks = Vec::new();
        for folder in ["one", "two", "three", "four"] {
            for n in 0..6 {
                tracks.push(PathBuf::from(format!("{folder}/{n}.mp3")));
            }
        }
        tracks
    }

    /// How many neighbouring pairs in the play order share a folder.
    fn adjacent_same_folder(seed: u64, mode: ShuffleMode) -> usize {
        let mut q = Queue::seeded(seed);
        q.replace(clumpable_tracks(), 0);
        q.set_shuffle(mode);

        let parent = |i: usize| q.tracks()[i].parent().unwrap().to_path_buf();

        q.order()
            .windows(2)
            .filter(|pair| parent(pair[0]) == parent(pair[1]))
            .count()
    }

    /// Smart shuffle should break up runs from the same folder.
    ///
    /// Seeded, so this either always passes or always fails. It used to shuffle
    /// from the clock and assert a bound, which made it a coin toss: the
    /// implementation is good — it clumps at all in only a quarter of orderings
    /// — but roughly one run in a hundred exceeded the bound and failed a
    /// correct implementation. A few fixed seeds cover more ground than one
    /// random draw and cost nothing to reproduce.
    #[test]
    fn smart_shuffle_separates_tracks_from_one_folder() {
        for seed in [1, 7, 42, 1_000, 0xDEAD_BEEF, u64::MAX] {
            let adjacent = adjacent_same_folder(seed, ShuffleMode::Smart);

            assert!(
                adjacent <= 2,
                "seed {seed}: {adjacent} same-folder neighbours is too clumped"
            );
        }
    }

    /// The claim smart shuffle actually makes, stated as a comparison rather
    /// than as a threshold.
    ///
    /// A single ordering cannot demonstrate this — random shuffle produces a
    /// well-spaced ordering now and then all by itself. Averaging over many
    /// removes the luck while still failing loudly if the spacing pass stops
    /// doing anything.
    #[test]
    fn smart_shuffle_clumps_far_less_than_random_shuffle() {
        const RUNS: u64 = 200;

        let mut smart = 0;
        let mut random = 0;
        let mut worst_smart = 0;

        for run in 0..RUNS {
            // Spread the seeds rather than walking 0..200: consecutive seeds
            // into an xorshift start out correlated.
            let seed = run.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);

            let spaced = adjacent_same_folder(seed, ShuffleMode::Smart);
            worst_smart = worst_smart.max(spaced);

            smart += spaced;
            random += adjacent_same_folder(seed, ShuffleMode::Random);
        }

        let smart_mean = smart as f64 / RUNS as f64;
        let random_mean = random as f64 / RUNS as f64;

        // Measured over these two hundred orderings: 0.35 against 5.04.
        // The bounds are loose enough that ordinary variation cannot reach
        // them, and tight enough that a spacing pass which had quietly stopped
        // working would land on the wrong side.
        assert!(
            random_mean > 3.0,
            "random shuffle averaged only {random_mean:.2} adjacent pairs — the \
             fixture is no longer clumpy enough to prove anything"
        );
        assert!(
            smart_mean < 1.0,
            "smart shuffle averaged {smart_mean:.2} adjacent pairs against \
             random's {random_mean:.2}"
        );
        assert!(
            smart_mean * 3.0 < random_mean,
            "smart shuffle ({smart_mean:.2}) is not meaningfully better than \
             random ({random_mean:.2})"
        );

        // Even the worst ordering should stay well clear of random's average.
        // The measured worst is 3; the bound leaves room for the fixture or
        // the seeds to change without turning this into a tripwire.
        assert!(
            worst_smart < 8,
            "the worst smart ordering had {worst_smart} adjacent pairs"
        );
    }

    /// Turning shuffle off should hand back the record, not a rotation of it.
    ///
    /// The old code moved the playing track to the front of the restored
    /// order, so switching shuffle off mid-album left track seven first and
    /// track one stranded in the middle.
    #[test]
    fn turning_shuffle_off_restores_the_original_order() {
        let tracks = paths(&["a.mp3", "b.mp3", "c.mp3", "d.mp3", "e.mp3", "f.mp3"]);

        let mut q = Queue::seeded(99);
        q.replace(tracks.clone(), 0);
        q.set_shuffle(ShuffleMode::Random);

        // Move off the first track so the restore has something to get wrong.
        q.next();
        q.next();
        let playing = q.current().unwrap().to_path_buf();

        q.set_shuffle(ShuffleMode::Off);

        assert_eq!(
            q.order(),
            (0..tracks.len()).collect::<Vec<_>>(),
            "the natural order was not restored"
        );
        assert_eq!(
            q.current().unwrap(),
            playing,
            "the playing track changed when shuffle was turned off"
        );

        // And playback continues from there, in album order.
        let was_at = q.cursor();
        let following = q.next().unwrap().to_path_buf();
        assert_eq!(following, tracks[was_at + 1]);
    }

    /// Seeding has to actually determine the result, or the tests above are
    /// only pretending to be reproducible.
    #[test]
    fn a_seeded_queue_shuffles_the_same_way_every_time() {
        let order_for = |seed: u64| {
            let mut q = Queue::seeded(seed);
            q.replace(clumpable_tracks(), 0);
            q.set_shuffle(ShuffleMode::Random);
            q.order().to_vec()
        };

        assert_eq!(order_for(12_345), order_for(12_345));
        assert_ne!(
            order_for(12_345),
            order_for(54_321),
            "different seeds produced the same ordering"
        );
    }
}
