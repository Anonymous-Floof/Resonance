//! Pulling a usable interface colour out of album art.
//!
//! The Adaptive theme tints the whole shell from the cover of whatever is
//! playing. That sounds like "find the dominant colour", but the dominant
//! colour of an album cover is almost always black, white, or a dishwater
//! grey — sleeve design overwhelmingly uses a neutral field with a small
//! amount of colour in it. Tinting the interface with the *most common* colour
//! produces a grey accent on nearly every record in a collection.
//!
//! So the job is not "most common", it is "most usable": prominent enough to
//! belong to the cover, colourful enough to read as a deliberate choice, and
//! light enough to sit on a dark shell. Those three pull against each other,
//! and the scoring below is where that trade-off lives.
//!
//! Everything happens in Oklab. Clustering in sRGB merges colours that look
//! nothing alike and splits ones that look identical.

use anyhow::{Context, Result};
use image::DynamicImage;
use image::imageops::FilterType;

use crate::color::{Oklab, Rgb};

/// Clusters to split a cover into.
///
/// Six is enough to keep a small accent colour from being swallowed by the
/// background it sits on, and few enough that the result is stable — at high
/// k, adjacent runs split the same region differently and the chosen accent
/// flickers between near-identical shades on re-scan.
const CLUSTERS: usize = 6;

/// Edge length the cover is sampled at before clustering.
///
/// 48×48 is 2304 pixels: enough that a colour occupying a twentieth of the
/// sleeve still lands ~115 samples, and small enough that the whole extraction
/// costs well under a millisecond.
const SAMPLE_EDGE: u32 = 48;

const MAX_ITERATIONS: usize = 24;

/// Below this chroma there is no colour here worth calling an accent.
///
/// A black-and-white sleeve must yield *nothing* rather than a muddy
/// near-grey: the configured accent is a better answer than a bad guess.
const MIN_CHROMA: f32 = 0.045;

/// The lightness band an accent is pushed into, for a dark shell.
const ACCENT_LIGHTNESS: (f32, f32) = (0.58, 0.80);

/// The colourfulness band. The floor stops a barely-tinted accent reading as
/// broken; the ceiling stops a saturated sleeve producing a neon interface.
const ACCENT_CHROMA: (f32, f32) = (0.085, 0.19);

/// One colour region of a cover.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Swatch {
    pub colour: Rgb,
    /// Fraction of the sampled pixels in this cluster, 0.0..=1.0.
    pub weight: f32,
}

/// What a cover looks like, reduced to a handful of colours.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CoverPalette {
    /// The colour to tint the interface with, if the cover offers one.
    ///
    /// `None` for a monochrome sleeve, which is a real answer and not a
    /// failure — the caller keeps the accent the user configured.
    pub accent: Option<Rgb>,
    /// Every cluster, most prominent first. Used for ambient backgrounds,
    /// where the *common* colours are exactly what is wanted.
    pub swatches: Vec<Swatch>,
}

impl CoverPalette {
    /// The darkest prominent colour, for a full-screen background wash.
    ///
    /// Backgrounds want the opposite of what an accent wants: common, dark,
    /// and unobtrusive. Falling back to the accent keeps a cover that is one
    /// flat bright colour from producing a black screen.
    pub fn backdrop(&self) -> Option<Rgb> {
        self.swatches
            .iter()
            .filter(|swatch| swatch.weight > 0.08)
            .min_by(|a, b| {
                a.colour
                    .luminance()
                    .partial_cmp(&b.colour.luminance())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|swatch| swatch.colour)
            .or(self.accent)
    }

    pub fn is_empty(&self) -> bool {
        self.swatches.is_empty()
    }
}

/// Extract from encoded image bytes.
pub fn from_encoded(bytes: &[u8]) -> Result<CoverPalette> {
    let image = image::load_from_memory(bytes).context("decoding cover art for its palette")?;
    Ok(from_image(&image))
}

/// Extract from an already-decoded cover.
///
/// Separate from [`from_encoded`] so the scanner, which has just decoded the
/// cover to resize it, does not decode it a second time.
pub fn from_image(image: &DynamicImage) -> CoverPalette {
    // A box filter is the right choice here and not merely the fast one:
    // averaging blocks of pixels is itself a mild denoise, and clustering
    // wants regions rather than the sharp edges a good filter preserves.
    let small = image
        .resize(SAMPLE_EDGE, SAMPLE_EDGE, FilterType::Triangle)
        .to_rgb8();

    let samples: Vec<Oklab> = small
        .pixels()
        .map(|pixel| Rgb::new(pixel[0], pixel[1], pixel[2]).to_oklab())
        .collect();

    if samples.is_empty() {
        return CoverPalette::default();
    }

    let clusters = kmeans(&samples, CLUSTERS);

    let mut swatches: Vec<Swatch> = clusters
        .iter()
        .filter(|cluster| cluster.count > 0)
        .map(|cluster| Swatch {
            colour: cluster.centre.to_rgb(),
            weight: cluster.count as f32 / samples.len() as f32,
        })
        .collect();

    swatches.sort_by(|a, b| {
        b.weight
            .partial_cmp(&a.weight)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    CoverPalette {
        accent: choose_accent(&clusters, samples.len()),
        swatches,
    }
}

/// Pick the cluster that would make the best accent, and make it usable.
///
/// The chroma gate is applied *before* ranking, not after. Screening the
/// winner instead threw away covers that plainly had a colour on them: a dark
/// sleeve with a red logo scores the near-black field highest on prominence
/// and lightness, and rejecting that field then reported "no accent" while the
/// red sat untouched in the next cluster along. Filtering first asks the right
/// question — of the colours here, which is the best accent — rather than the
/// wrong one, which is whether the single best-scoring region happens to be
/// colourful.
fn choose_accent(clusters: &[Cluster], total: usize) -> Option<Rgb> {
    let total = total as f32;

    let (centre, score) = clusters
        .iter()
        .filter(|cluster| cluster.count > 0)
        // A greyscale sleeve genuinely has no accent in it, and the configured
        // accent is a better answer than a muddy near-grey.
        .filter(|cluster| cluster.centre.chroma() >= MIN_CHROMA)
        .map(|cluster| {
            let weight = cluster.count as f32 / total;
            (cluster.centre, accent_score(cluster.centre, weight))
        })
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))?;

    if score <= 0.0 {
        return None;
    }

    Some(usable(centre))
}

/// How good an accent this colour would make.
///
/// The three factors multiply rather than add, so a colour has to satisfy all
/// of them: a vivid colour covering four pixels loses, and so does half the
/// sleeve in charcoal.
fn accent_score(colour: Oklab, weight: f32) -> f32 {
    // Square root, so prominence matters but does not dominate. Linear weight
    // hands the decision straight back to the background field this whole
    // module exists to avoid picking.
    let prominence = weight.sqrt();

    // Rises to 1.0 at a clearly-coloured 0.13 and stops there — beyond that,
    // more saturation is not more accent-worthy.
    let colourfulness = (colour.chroma() / 0.13).min(1.0);

    // Falls away from a mid-light target in both directions. Near-black and
    // near-white are the two things that must never win.
    let offset = (colour.l - 0.65) / 0.30;
    let lightness = (-offset * offset).exp();

    prominence * colourfulness * lightness
}

/// Move a colour into the band the interface can actually use.
fn usable(colour: Oklab) -> Rgb {
    let lightness = colour.l.clamp(ACCENT_LIGHTNESS.0, ACCENT_LIGHTNESS.1);
    let adjusted = colour.with_lightness(lightness);

    let chroma = adjusted.chroma().clamp(ACCENT_CHROMA.0, ACCENT_CHROMA.1);
    adjusted.with_chroma(chroma).to_rgb()
}

// ---------------------------------------------------------------------------
// Clustering
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct Cluster {
    centre: Oklab,
    count: usize,
}

/// Lloyd's algorithm with deterministic furthest-point seeding.
///
/// Determinism is the point of the seeding choice. A randomly seeded k-means
/// gives a slightly different accent every time the same cover is scanned,
/// which would show up as the interface changing colour for no reason after a
/// rescan. Furthest-point seeding is also a good fit for the problem: it picks
/// out the colours that are *unlike* each other, which is exactly the small
/// bright region that the dominant-colour approach loses.
fn kmeans(samples: &[Oklab], k: usize) -> Vec<Cluster> {
    let k = k.min(samples.len()).max(1);

    let mut centres = seed(samples, k);
    let mut assignment = vec![0usize; samples.len()];

    for _ in 0..MAX_ITERATIONS {
        let mut moved = false;

        for (index, sample) in samples.iter().enumerate() {
            let nearest = nearest_centre(*sample, &centres);
            if nearest != assignment[index] {
                assignment[index] = nearest;
                moved = true;
            }
        }

        // Recompute the centres from what landed in them.
        let mut sums = vec![(0.0f32, 0.0f32, 0.0f32, 0usize); centres.len()];
        for (index, sample) in samples.iter().enumerate() {
            let slot = &mut sums[assignment[index]];
            slot.0 += sample.l;
            slot.1 += sample.a;
            slot.2 += sample.b;
            slot.3 += 1;
        }

        for (centre, sum) in centres.iter_mut().zip(&sums) {
            if sum.3 == 0 {
                // An emptied cluster keeps its position rather than being
                // re-seeded: re-seeding costs determinism, and an empty
                // cluster is dropped from the result anyway.
                continue;
            }
            let count = sum.3 as f32;
            *centre = Oklab::new(sum.0 / count, sum.1 / count, sum.2 / count);
        }

        if !moved {
            break;
        }
    }

    let mut counts = vec![0usize; centres.len()];
    for index in &assignment {
        counts[*index] += 1;
    }

    centres
        .into_iter()
        .zip(counts)
        .map(|(centre, count)| Cluster { centre, count })
        .collect()
}

/// Start from the mean, then repeatedly take the sample furthest from every
/// centre chosen so far.
fn seed(samples: &[Oklab], k: usize) -> Vec<Oklab> {
    let count = samples.len() as f32;
    let mean = Oklab::new(
        samples.iter().map(|s| s.l).sum::<f32>() / count,
        samples.iter().map(|s| s.a).sum::<f32>() / count,
        samples.iter().map(|s| s.b).sum::<f32>() / count,
    );

    let mut centres = Vec::with_capacity(k);
    centres.push(mean);

    while centres.len() < k {
        let furthest = samples
            .iter()
            .map(|sample| {
                let nearest = centres
                    .iter()
                    .map(|centre| sample.distance(*centre))
                    .fold(f32::INFINITY, f32::min);
                (sample, nearest)
            })
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        match furthest {
            Some((sample, distance)) if distance > 1e-4 => centres.push(*sample),
            // Every remaining sample coincides with a centre: a flat image.
            _ => break,
        }
    }

    centres
}

fn nearest_centre(sample: Oklab, centres: &[Oklab]) -> usize {
    let mut best = 0;
    let mut best_distance = f32::INFINITY;

    for (index, centre) in centres.iter().enumerate() {
        let distance = sample.distance(*centre);
        if distance < best_distance {
            best_distance = distance;
            best = index;
        }
    }

    best
}

// ---------------------------------------------------------------------------
// On-disk form
// ---------------------------------------------------------------------------

/// The palette as a short line-based text block.
///
/// Hand-rolled rather than JSON because it is a hundred bytes sitting beside a
/// cached image, and a format you can read with `cat` while working out why an
/// album came out orange is worth more here than a serde derive.
pub fn encode(palette: &CoverPalette) -> String {
    use std::fmt::Write;

    let mut out = String::from("v1\n");

    match palette.accent {
        Some(colour) => {
            let _ = writeln!(out, "accent {colour}");
        }
        None => out.push_str("accent -\n"),
    }

    for swatch in &palette.swatches {
        let _ = writeln!(out, "{} {:.4}", swatch.colour, swatch.weight);
    }

    out
}

/// Read a palette back. Returns `None` for anything unrecognised, so a cache
/// written by a future version degrades to "recompute" rather than an error.
pub fn decode(text: &str) -> Option<CoverPalette> {
    let mut lines = text.lines();

    if lines.next()?.trim() != "v1" {
        return None;
    }

    let accent_line = lines.next()?;
    let accent = match accent_line.strip_prefix("accent ")?.trim() {
        "-" => None,
        hex => Some(Rgb::parse_hex(hex)?),
    };

    let mut swatches = Vec::new();
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (hex, weight) = line.split_once(' ')?;
        swatches.push(Swatch {
            colour: Rgb::parse_hex(hex)?,
            weight: weight.trim().parse().ok()?,
        });
    }

    Some(CoverPalette { accent, swatches })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cover: a field of `background` with a `patch`-sized square of
    /// `feature` in the corner, which is how most sleeves are actually built.
    fn cover(background: Rgb, feature: Rgb, patch: u32) -> DynamicImage {
        let mut buffer = image::RgbImage::from_pixel(200, 200, image::Rgb(background.to_array()));
        for y in 0..patch {
            for x in 0..patch {
                buffer.put_pixel(x, y, image::Rgb(feature.to_array()));
            }
        }
        DynamicImage::ImageRgb8(buffer)
    }

    #[test]
    fn oklab_round_trips_through_rgb() {
        for colour in [
            Rgb::new(0, 0, 0),
            Rgb::new(255, 255, 255),
            Rgb::new(220, 40, 80),
            Rgb::new(18, 92, 160),
            Rgb::new(130, 130, 130),
        ] {
            let back = colour.to_oklab().to_rgb();
            for (a, b) in colour.to_array().iter().zip(back.to_array().iter()) {
                assert!(
                    a.abs_diff(*b) <= 1,
                    "{colour} round-tripped to {back}, which is not close enough"
                );
            }
        }
    }

    /// The whole reason this module exists: a mostly-black sleeve with a small
    /// coloured element must yield the colour, not the black.
    #[test]
    fn a_small_bright_region_beats_a_large_dark_one() {
        let palette = from_image(&cover(Rgb::new(12, 12, 14), Rgb::new(230, 90, 40), 44));

        let accent = palette.accent.expect("there is a colour on this cover");
        let lab = accent.to_oklab();

        assert!(
            lab.chroma() > MIN_CHROMA,
            "{accent} is not colourful enough to be an accent"
        );
        assert!(
            lab.l >= ACCENT_LIGHTNESS.0 - 0.02,
            "{accent} is too dark to sit on a dark shell"
        );

        // Orange: red-positive, yellow-positive.
        assert!(lab.a > 0.0 && lab.b > 0.0, "{accent} is not the orange");
    }

    /// And the mirror case: a colourless sleeve must not invent one.
    #[test]
    fn a_monochrome_cover_offers_no_accent() {
        let palette = from_image(&cover(Rgb::new(20, 20, 20), Rgb::new(240, 240, 240), 80));

        assert!(
            palette.accent.is_none(),
            "a black-and-white cover produced {:?}",
            palette.accent
        );

        // It still describes itself, which is what backgrounds need.
        assert!(!palette.is_empty());
    }

    /// The failure the contact sheet turned up: a dark sleeve whose largest
    /// region is near-black, with one small saturated element. Screening the
    /// top-scoring cluster after ranking reported "no accent" for these and
    /// left the colour on the floor.
    #[test]
    fn a_colour_is_found_even_when_the_dominant_region_is_grey() {
        // Deep charcoal over nine tenths of the sleeve, a red mark on the rest.
        let palette = from_image(&cover(Rgb::new(9, 9, 11), Rgb::new(198, 32, 40), 34));

        let accent = palette
            .accent
            .expect("the red mark is an accent even though the black is bigger");

        let lab = accent.to_oklab();
        assert!(lab.a > 0.05, "{accent} is not the red");
        assert!(
            lab.l > 0.4,
            "{accent} came from the charcoal rather than the red"
        );
    }

    /// A near-white sleeve is the other extreme, and just as unusable raw.
    #[test]
    fn a_pale_cover_yields_something_dark_enough_to_read() {
        let palette = from_image(&cover(Rgb::new(248, 246, 240), Rgb::new(150, 205, 235), 90));
        let accent = palette.accent.expect("the blue is a colour");

        assert!(
            accent.to_oklab().l <= ACCENT_LIGHTNESS.1 + 0.02,
            "{accent} is too pale to show on a light background"
        );
    }

    /// The same cover twice must give the same answer, or the interface
    /// changes colour after a rescan for no visible reason.
    #[test]
    fn extraction_is_deterministic() {
        let image = cover(Rgb::new(40, 60, 90), Rgb::new(220, 180, 60), 70);

        let first = from_image(&image);
        let second = from_image(&image);

        assert_eq!(first, second);
    }

    #[test]
    fn swatches_are_ordered_by_prominence() {
        let palette = from_image(&cover(Rgb::new(30, 40, 60), Rgb::new(200, 60, 60), 40));

        for pair in palette.swatches.windows(2) {
            assert!(
                pair[0].weight >= pair[1].weight,
                "swatches are out of order: {pair:?}"
            );
        }

        let total: f32 = palette.swatches.iter().map(|s| s.weight).sum();
        assert!(
            (total - 1.0).abs() < 0.01,
            "weights should cover the image, summed to {total}"
        );
    }

    /// Backgrounds want the dark common colour, not the bright rare one.
    #[test]
    fn the_backdrop_is_darker_than_the_accent() {
        let palette = from_image(&cover(Rgb::new(16, 20, 34), Rgb::new(240, 120, 60), 46));

        let accent = palette.accent.expect("there is an orange here");
        let backdrop = palette.backdrop().expect("and a field behind it");

        assert!(
            backdrop.luminance() < accent.luminance(),
            "backdrop {backdrop} is not darker than accent {accent}"
        );
    }

    #[test]
    fn palettes_round_trip_through_the_cache_format() {
        let palette = from_image(&cover(Rgb::new(25, 35, 45), Rgb::new(210, 90, 150), 55));

        let text = encode(&palette);
        let back = decode(&text).expect("what we just wrote must parse");

        assert_eq!(palette.accent, back.accent);
        assert_eq!(palette.swatches.len(), back.swatches.len());
        for (a, b) in palette.swatches.iter().zip(&back.swatches) {
            assert_eq!(a.colour, b.colour);
            assert!((a.weight - b.weight).abs() < 0.001);
        }
    }

    #[test]
    fn a_palette_with_no_accent_round_trips_too() {
        let palette = CoverPalette {
            accent: None,
            swatches: vec![Swatch {
                colour: Rgb::new(10, 10, 10),
                weight: 1.0,
            }],
        };

        assert_eq!(decode(&encode(&palette)), Some(palette));
    }

    #[test]
    fn a_future_format_is_ignored_rather_than_guessed_at() {
        assert!(decode("v9\naccent #FF0000\n").is_none());
        assert!(decode("").is_none());
        assert!(decode("v1\n").is_none());
    }

    #[test]
    fn a_flat_image_does_not_hang_or_panic() {
        let flat = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            64,
            64,
            image::Rgb([77, 77, 77]),
        ));
        let palette = from_image(&flat);

        assert!(palette.accent.is_none(), "flat grey has no accent");
        assert_eq!(palette.swatches.len(), 1, "one colour, one swatch");
    }

    #[test]
    fn corrupt_bytes_are_an_error_not_a_panic() {
        assert!(from_encoded(b"not an image at all").is_err());
    }
}
