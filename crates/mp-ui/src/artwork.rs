//! Turning cached cover files into GPU textures, without stalling the UI.
//!
//! Three rules make this safe to call from inside a row-drawing loop:
//!
//! * **Bounded work per frame.** A fast scroll can sweep past hundreds of rows;
//!   decoding every cover it touches would drop frames. Only a handful are
//!   decoded per frame and the rest simply draw their placeholder until their
//!   turn comes, which reads as artwork fading in rather than as a stutter.
//! * **Bounded memory.** Textures are evicted least-recently-used, so a 20k
//!   track library cannot pin 20k covers in video memory.
//! * **Failures are remembered.** A cover that will not decode is recorded as
//!   absent, so it is not retried once per frame forever.

use std::collections::HashMap;

use egui::{ColorImage, Context, TextureHandle, TextureOptions};
use mp_core::library::{ArtCache, ArtSize};

/// How many covers may be decoded in a single frame.
///
/// Twelve 64 px JPEGs is well under a millisecond; the limit exists for the
/// case where a scroll jump asks for a screenful of 800 px covers at once.
const DECODES_PER_FRAME: usize = 12;

/// How many textures to keep resident before evicting the coldest.
const CAPACITY: usize = 512;

/// A cover that has been asked for at a particular size.
type Key = (String, ArtSize);

struct Entry {
    /// `None` when the file is missing or will not decode.
    texture: Option<TextureHandle>,
    /// Frame number this was last drawn on, for eviction.
    used: u64,
}

/// Cover textures, keyed by content id and size.
pub struct Artwork {
    entries: HashMap<Key, Entry>,
    frame: u64,
    decoded_this_frame: usize,
    /// Covers that were wanted but arrived after the per-frame budget ran out.
    deferred: usize,
}

impl Default for Artwork {
    fn default() -> Self {
        Self::new()
    }
}

impl Artwork {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            frame: 0,
            decoded_this_frame: 0,
            deferred: 0,
        }
    }

    /// Call once at the top of each frame.
    pub fn begin_frame(&mut self) {
        self.frame += 1;
        self.decoded_this_frame = 0;
        self.deferred = 0;
    }

    /// Whether any cover was postponed this frame, so the caller knows to ask
    /// for another repaint rather than leaving blanks on screen.
    pub fn wants_repaint(&self) -> bool {
        self.deferred > 0
    }

    /// The texture for a cover, loading it if there is budget left this frame.
    ///
    /// Returns `None` while a cover is still queued, when it does not exist,
    /// and when it could not be decoded — the caller draws its placeholder in
    /// all three cases, which is the right thing to show for each.
    pub fn get(
        &mut self,
        ctx: &Context,
        cache: &ArtCache,
        art_id: &str,
        size: ArtSize,
    ) -> Option<TextureHandle> {
        let key = (art_id.to_owned(), size);

        if let Some(entry) = self.entries.get_mut(&key) {
            entry.used = self.frame;
            return entry.texture.clone();
        }

        if self.decoded_this_frame >= DECODES_PER_FRAME {
            self.deferred += 1;
            return None;
        }
        self.decoded_this_frame += 1;

        let texture = load(ctx, cache, art_id, size);
        let handle = texture.clone();

        self.entries.insert(
            key,
            Entry {
                texture,
                used: self.frame,
            },
        );

        self.evict_if_needed();
        handle
    }

    /// Forget everything. Used when the library is rebuilt, since content ids
    /// survive a rescan but the files behind them may not.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Drop the coldest entries once the cache is over capacity.
    fn evict_if_needed(&mut self) {
        if self.entries.len() <= CAPACITY {
            return;
        }

        // Evict in one pass rather than repeatedly finding the single oldest:
        // trimming a quarter at a time keeps this from running every frame.
        let target = CAPACITY * 3 / 4;
        let mut ages: Vec<(u64, Key)> = self
            .entries
            .iter()
            .map(|(key, entry)| (entry.used, key.clone()))
            .collect();
        ages.sort_unstable_by_key(|(used, _)| *used);

        for (_, key) in ages.into_iter().take(self.entries.len() - target) {
            self.entries.remove(&key);
        }
    }

    /// Number of resident textures, for the debug overlay.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Read and decode one cover.
fn load(ctx: &Context, cache: &ArtCache, art_id: &str, size: ArtSize) -> Option<TextureHandle> {
    let bytes = cache.read(art_id, size)?;

    let decoded = match image::load_from_memory(&bytes) {
        Ok(image) => image.to_rgba8(),
        Err(err) => {
            tracing::debug!("cover {art_id} will not decode: {err}");
            return None;
        }
    };

    let dimensions = [decoded.width() as usize, decoded.height() as usize];
    let image = ColorImage::from_rgba_unmultiplied(dimensions, decoded.as_raw());

    Some(ctx.load_texture(
        format!("art-{art_id}-{size:?}"),
        image,
        // Covers are drawn at close to their stored size, so linear filtering
        // is right; nearest would alias badly on the 64 px row thumbnails.
        TextureOptions::LINEAR,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The budget is what keeps a fast scroll from turning into a stall.
    #[test]
    fn only_a_bounded_number_of_covers_load_per_frame() {
        let ctx = Context::default();
        let cache = ArtCache::new(std::env::temp_dir().join("resonance-artwork-test-missing"));
        let mut artwork = Artwork::new();

        artwork.begin_frame();
        for index in 0..(DECODES_PER_FRAME * 3) {
            let _ = artwork.get(&ctx, &cache, &format!("{index:032x}"), ArtSize::Thumb);
        }

        assert_eq!(artwork.decoded_this_frame, DECODES_PER_FRAME);
        assert!(artwork.wants_repaint(), "the rest must be asked for again");
        assert_eq!(artwork.len(), DECODES_PER_FRAME);
    }

    /// A missing cover must be remembered as missing, not retried every frame.
    #[test]
    fn a_missing_cover_is_only_looked_for_once() {
        let ctx = Context::default();
        let cache = ArtCache::new(std::env::temp_dir().join("resonance-artwork-test-missing"));
        let mut artwork = Artwork::new();

        artwork.begin_frame();
        assert!(
            artwork
                .get(&ctx, &cache, "deadbeef", ArtSize::Thumb)
                .is_none()
        );
        assert_eq!(artwork.decoded_this_frame, 1);

        artwork.begin_frame();
        assert!(
            artwork
                .get(&ctx, &cache, "deadbeef", ArtSize::Thumb)
                .is_none()
        );
        assert_eq!(
            artwork.decoded_this_frame, 0,
            "the second ask must be served from the cache"
        );
    }

    #[test]
    fn the_cache_stays_bounded() {
        let ctx = Context::default();
        let cache = ArtCache::new(std::env::temp_dir().join("resonance-artwork-test-missing"));
        let mut artwork = Artwork::new();

        for round in 0..(CAPACITY / DECODES_PER_FRAME + 8) {
            artwork.begin_frame();
            for index in 0..DECODES_PER_FRAME {
                let id = format!("{:032x}", round * DECODES_PER_FRAME + index);
                let _ = artwork.get(&ctx, &cache, &id, ArtSize::Thumb);
            }
        }

        assert!(
            artwork.len() <= CAPACITY,
            "cache grew to {} entries",
            artwork.len()
        );
    }
}
