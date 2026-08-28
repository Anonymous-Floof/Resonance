//! Cover art: extracted once, stored once, read back cheaply.
//!
//! Artwork is keyed by the hash of its own bytes rather than by album or track.
//! Twelve tracks from one album embed twelve identical JPEGs; content
//! addressing collapses those to a single set of files on disk and — more
//! importantly — lets the UI cache a texture per `art_id` instead of per track.
//!
//! Every cover is stored pre-resized at the sizes the interface actually uses.
//! Decoding a 3000×3000 embedded cover to draw a 64 px row thumbnail is the
//! kind of thing that makes a list stutter, and it would happen on the UI
//! thread.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use image::imageops::FilterType;

use super::accent::{self, CoverPalette};

/// The sizes covers are stored at, in pixels on the long edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArtSize {
    /// Track rows in a list.
    Thumb,
    /// Album and artist cards in a grid.
    Card,
    /// Full-screen now playing.
    Full,
}

impl ArtSize {
    pub const ALL: [Self; 3] = [Self::Thumb, Self::Card, Self::Full];

    pub fn pixels(self) -> u32 {
        match self {
            Self::Thumb => 64,
            Self::Card => 256,
            Self::Full => 800,
        }
    }

    fn suffix(self) -> &'static str {
        match self {
            Self::Thumb => "t",
            Self::Card => "c",
            Self::Full => "f",
        }
    }
}

/// Filenames that hold a cover for every track in the folder.
///
/// Checked in order, so a purpose-named `cover.jpg` wins over a generic
/// `front.png` if a folder somehow has both.
///
/// Windows Media Player's `AlbumArt_{GUID}_Large.jpg` and `AlbumArtSmall.jpg`
/// are deliberately absent. They are per-album caches that WMP scatters into
/// whatever folder it likes, so treating them as folder art would paste one
/// arbitrary album's cover onto every loose track sitting beside them.
pub const SIDECAR_NAMES: &[&str] = &["cover", "folder", "front", "album"];

/// Extensions accepted for a sidecar cover.
pub const SIDECAR_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp", "bmp", "gif"];

/// A directory of content-addressed cover thumbnails.
#[derive(Debug, Clone)]
pub struct ArtCache {
    root: PathBuf,
}

impl ArtCache {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where a stored cover lives. The file may not exist.
    pub fn path(&self, art_id: &str, size: ArtSize) -> PathBuf {
        // One level of fan-out keeps directories to a sane size; some tools
        // slow down badly with tens of thousands of entries in one folder.
        self.root
            .join(&art_id[..2.min(art_id.len())])
            .join(format!("{art_id}-{}.jpg", size.suffix()))
    }

    /// Where a cover's extracted palette lives. The file may not exist.
    ///
    /// Kept beside the images rather than in the database so that a cover and
    /// everything derived from it live and die together — pruning the cache is
    /// then a file delete and not a delete plus a row.
    pub fn palette_path(&self, art_id: &str) -> PathBuf {
        self.root
            .join(&art_id[..2.min(art_id.len())])
            .join(format!("{art_id}-p.txt"))
    }

    /// Whether every size for this cover is already on disk.
    pub fn contains(&self, art_id: &str) -> bool {
        ArtSize::ALL
            .iter()
            .all(|size| self.path(art_id, *size).is_file())
    }

    /// Store an encoded image, returning its content id.
    ///
    /// Decoding and resizing is skipped entirely when the cover is already
    /// cached, which is the common case: the second track of an album costs a
    /// hash of the bytes and nothing more.
    pub fn store(&self, encoded: &[u8]) -> Result<String> {
        let art_id = content_id(encoded);

        if self.contains(&art_id) {
            return Ok(art_id);
        }

        let image = image::load_from_memory(encoded).context("decoding embedded cover art")?;

        // Extracted here because the cover is already decoded. Doing it on
        // demand would mean decoding a JPEG on the UI thread at the exact
        // moment a track changes, which is the one moment it must not stutter.
        self.write_palette(&art_id, &accent::from_image(&image));

        for size in ArtSize::ALL {
            let path = self.path(&art_id, size);
            if path.is_file() {
                continue;
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }

            // `thumbnail` for the small sizes: it is a box filter and is many
            // times faster than Lanczos, and at 64 px the difference is not
            // visible. The full size gets the good filter.
            let scaled = match size {
                ArtSize::Full => image.resize(size.pixels(), size.pixels(), FilterType::Lanczos3),
                _ => image.thumbnail(size.pixels(), size.pixels()),
            };

            // JPEG has no alpha; flattening onto black matches the dark shell
            // and avoids the black-fringe artefact of premultiplied edges.
            //
            // Written to a private temporary name and renamed into place: the
            // scanner is parallel, and two threads finding the same cover in
            // two files would otherwise interleave their writes into one file
            // and produce a corrupt image.
            let temporary = path.with_extension(format!("{}.part", next_temp_id()));
            scaled
                .to_rgb8()
                .save_with_format(&temporary, image::ImageFormat::Jpeg)
                .with_context(|| format!("writing {}", temporary.display()))?;

            if std::fs::rename(&temporary, &path).is_err() {
                // Another thread got there first, which is fine - the contents
                // are identical by construction.
                let _ = std::fs::remove_file(&temporary);
            }
        }

        Ok(art_id)
    }

    /// The colours of a stored cover.
    ///
    /// Computes and caches the palette if it is missing, which is what happens
    /// for every cover in a library scanned before palettes existed. That
    /// backfill reads the 256 px thumbnail rather than the original file: the
    /// original may be long gone, and clustering is downsampling to 48 px
    /// anyway, so the small copy costs nothing in quality.
    pub fn palette(&self, art_id: &str) -> Option<CoverPalette> {
        if let Some(cached) = std::fs::read_to_string(self.palette_path(art_id))
            .ok()
            .as_deref()
            .and_then(accent::decode)
        {
            return Some(cached);
        }

        let bytes = self.read(art_id, ArtSize::Card)?;
        let palette = accent::from_encoded(&bytes).ok()?;
        self.write_palette(art_id, &palette);

        Some(palette)
    }

    /// Best-effort: a palette that cannot be written is simply recomputed next
    /// time, so a read-only or full disk costs a little work and nothing else.
    fn write_palette(&self, art_id: &str, palette: &CoverPalette) {
        let path = self.palette_path(art_id);

        if let Some(parent) = path.parent()
            && std::fs::create_dir_all(parent).is_err()
        {
            return;
        }

        let temporary = path.with_extension(format!("{}.part", next_temp_id()));
        if std::fs::write(&temporary, accent::encode(palette)).is_err() {
            let _ = std::fs::remove_file(&temporary);
            return;
        }

        if std::fs::rename(&temporary, &path).is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
    }

    /// Read a stored cover back, falling back through the sizes so a partially
    /// written cache still shows something.
    pub fn read(&self, art_id: &str, size: ArtSize) -> Option<Vec<u8>> {
        std::fs::read(self.path(art_id, size))
            .ok()
            .or_else(|| std::fs::read(self.path(art_id, ArtSize::Card)).ok())
    }

    /// Delete every cached file for a cover.
    pub fn remove(&self, art_id: &str) {
        for size in ArtSize::ALL {
            let _ = std::fs::remove_file(self.path(art_id, size));
        }
        let _ = std::fs::remove_file(self.palette_path(art_id));
    }
}

/// Find a cover image sitting beside the music in `folder`.
///
/// Used for the loose, untagged files that make up much of a downloaded
/// collection, where the only artwork present is a `folder.jpg`.
pub fn sidecar_in(folder: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(folder).ok()?;

    let mut best: Option<(usize, PathBuf)> = None;

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(extension) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };

        if !SIDECAR_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str()) {
            continue;
        }

        let stem = stem.to_ascii_lowercase();
        let Some(rank) = SIDECAR_NAMES.iter().position(|name| *name == stem) else {
            continue;
        };

        if best.as_ref().is_none_or(|(seen, _)| rank < *seen) {
            best = Some((rank, path));
        }
    }

    best.map(|(_, path)| path)
}

/// A name no other in-flight write can be using.
///
/// Two threads reaching the same cover at the same moment is the ordinary case
/// during a parallel scan, so the temporary name has to be unique per call and
/// not merely per process.
fn next_temp_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// A stable 128-bit id for a byte string, rendered as hex.
///
/// FNV-1a rather than a cryptographic hash: nothing here is adversarial, the
/// only requirement is that two different covers are overwhelmingly unlikely to
/// collide. Across even a hundred thousand distinct images the chance of a
/// 128-bit collision is far below the chance of the disk itself lying.
fn content_id(bytes: &[u8]) -> String {
    const OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;

    let mut hash = OFFSET;
    for byte in bytes {
        hash ^= u128::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }

    format!("{hash:032x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "resonance-art-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A tiny valid PNG, generated rather than embedded so the test has no
    /// binary fixture to keep in sync.
    fn png(width: u32, height: u32, shade: u8) -> Vec<u8> {
        let buffer = image::RgbImage::from_pixel(width, height, image::Rgb([shade, shade, shade]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(buffer)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    #[test]
    fn identical_covers_share_one_id_and_one_set_of_files() {
        let dir = temp_dir("dedupe");
        let cache = ArtCache::new(&dir);

        let cover = png(300, 300, 128);
        let first = cache.store(&cover).unwrap();
        let second = cache.store(&cover).unwrap();

        assert_eq!(first, second, "the same bytes must produce the same id");

        // Every size, plus the one palette derived from them.
        let files = walk_count(&dir);
        assert_eq!(files, ArtSize::ALL.len() + 1, "one cover, one set of files");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The palette is written during the scan, so nothing has to decode a
    /// cover on the UI thread when a track changes.
    #[test]
    fn storing_a_cover_extracts_its_palette() {
        let dir = temp_dir("palette");
        let cache = ArtCache::new(&dir);

        let art_id = cache.store(&coloured_png(200, 200)).unwrap();

        assert!(
            cache.palette_path(&art_id).is_file(),
            "the palette should be on disk immediately after storing"
        );

        let palette = cache.palette(&art_id).expect("a palette was just written");
        assert!(palette.accent.is_some(), "this cover has an obvious colour");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A library scanned before palettes existed has covers but no palettes.
    /// They must fill themselves in rather than staying blank forever.
    #[test]
    fn a_missing_palette_is_recomputed_from_the_cached_cover() {
        let dir = temp_dir("backfill");
        let cache = ArtCache::new(&dir);

        let art_id = cache.store(&coloured_png(200, 200)).unwrap();
        let expected = cache.palette(&art_id).unwrap();

        // Exactly the state an older cache is in.
        std::fs::remove_file(cache.palette_path(&art_id)).unwrap();
        assert!(!cache.palette_path(&art_id).is_file());

        let recovered = cache.palette(&art_id).expect("it should rebuild itself");
        assert_eq!(recovered.accent, expected.accent);
        assert!(
            cache.palette_path(&art_id).is_file(),
            "the rebuilt palette should be cached, not recomputed every time"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn removing_a_cover_takes_its_palette_with_it() {
        let dir = temp_dir("remove");
        let cache = ArtCache::new(&dir);

        let art_id = cache.store(&coloured_png(120, 120)).unwrap();
        cache.remove(&art_id);

        assert_eq!(walk_count(&dir), 0, "nothing should be left behind");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn asking_for_the_palette_of_a_cover_we_do_not_have_is_none() {
        let dir = temp_dir("absent");
        let cache = ArtCache::new(&dir);
        assert!(cache.palette("0123456789abcdef").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn different_covers_get_different_ids() {
        let dir = temp_dir("distinct");
        let cache = ArtCache::new(&dir);

        let a = cache.store(&png(64, 64, 10)).unwrap();
        let b = cache.store(&png(64, 64, 200)).unwrap();
        assert_ne!(a, b);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Covers are stored small so the UI never decodes a 3000 px image to draw
    /// a 64 px row.
    #[test]
    fn stored_covers_are_resized_down() {
        let dir = temp_dir("resize");
        let cache = ArtCache::new(&dir);

        let art_id = cache.store(&png(1200, 1200, 90)).unwrap();

        for size in ArtSize::ALL {
            let bytes = cache.read(&art_id, size).expect("every size is written");
            let decoded = image::load_from_memory(&bytes).unwrap();
            assert!(
                decoded.width() <= size.pixels(),
                "{size:?} should be at most {} px, got {}",
                size.pixels(),
                decoded.width()
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_image_is_an_error_not_a_panic() {
        let dir = temp_dir("corrupt");
        let cache = ArtCache::new(&dir);
        assert!(cache.store(b"this is not an image").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_best_named_sidecar_wins() {
        let dir = temp_dir("sidecar");
        std::fs::write(dir.join("front.jpg"), png(8, 8, 1)).unwrap();
        std::fs::write(dir.join("cover.png"), png(8, 8, 2)).unwrap();
        std::fs::write(dir.join("screenshot.png"), png(8, 8, 3)).unwrap();

        let found = sidecar_in(&dir).expect("a sidecar should be found");
        assert_eq!(found.file_stem().unwrap(), "cover");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_folder_with_no_cover_yields_nothing() {
        let dir = temp_dir("bare");
        std::fs::write(dir.join("song.mp3"), b"x").unwrap();
        assert!(sidecar_in(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A dark cover with a bright coloured block, so there is an accent to
    /// find. `png()` produces flat greys, which correctly yield no accent.
    fn coloured_png(width: u32, height: u32) -> Vec<u8> {
        let mut buffer = image::RgbImage::from_pixel(width, height, image::Rgb([14, 16, 20]));
        for y in 0..height / 4 {
            for x in 0..width / 4 {
                buffer.put_pixel(x, y, image::Rgb([225, 95, 45]));
            }
        }

        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(buffer)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    fn walk_count(dir: &Path) -> usize {
        walkdir::WalkDir::new(dir)
            .into_iter()
            .flatten()
            .filter(|e| e.file_type().is_file())
            .count()
    }
}
