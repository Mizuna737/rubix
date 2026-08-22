//! Wallpaper palette extraction.
//!
//! ## Why this lives in the compositor
//!
//! Every palette generator in the wild -- pywal, wallust, matugen -- takes an
//! 8-bit sRGB image from a file path. Handing one a PQ / BT.2020 AVIF gets a
//! palette derived from code values interpreted through the wrong transfer
//! function: the picture is not dark, but its *encoding* looks dark to anything
//! that assumes sRGB, so the extracted colours come out crushed and desaturated.
//! There is no tool to adopt for this, because nothing else in the stack knows
//! what the pixels mean.
//!
//! The compositor already does. [`crate::wallpaper::Decoded`] has resolved the
//! transfer function from the file's CICP block before this module is called,
//! so extraction starts from pixels whose luminance is *known* rather than
//! guessed.
//!
//! ## Why ICtCp
//!
//! k-means needs a space where euclidean distance approximates "looks
//! different". For 8-bit sRGB that is CIELAB or Oklab. Neither is defined for
//! absolute luminance: both normalise to a diffuse white, so a 4000-nit
//! specular highlight and a 200-nit sky both arrive as "white" and cluster
//! together -- exactly the distinction an HDR wallpaper exists to make.
//!
//! ICtCp (Rec. BT.2100) is the same idea built for this input. It applies the
//! PQ curve -- the one already encoding these files -- to an LMS cone response,
//! so it is perceptually uniform *in absolute luminance* across the full
//! 0-10000 cd/m² range and over BT.2020 primaries. The wallpaper is already in
//! the space ICtCp is derived from, so the conversion is two matrices and the
//! curve, with nothing thrown away in between.
//!
//! SDR wallpapers reach the same space through
//! [`crate::hdr_shaders::srgb_to_bt2020_abs10k`] -- the identical conversion the
//! SDR decode shader applies -- placed at the live `sdr_white_nits`. So an sRGB
//! PNG yields a palette derived from precisely the pixels the user is shown,
//! and the HDR and SDR paths differ only in how they get to abs10k.
//!
//! ## Why this k-means
//!
//! `kmeans_colors` is used with `default-features = false`, which reduces it to
//! a generic k-means over `rand` -- both of which the tree already builds. It
//! is here for its [`Calculate`] trait rather than its colour support: because
//! the trait is implemented on a caller-owned point type, clustering happens in
//! ICtCp directly instead of being routed through a crate colour type that
//! cannot represent absolute luminance.

use kmeans_colors::{get_kmeans, Calculate};
use rand::Rng;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::Color32F;
use smithay::utils::{Buffer as BufferCoords, Size};

use crate::color_management::DecodeKind;

/// How many pixels to cluster. The full image is strided down to roughly this
/// many samples: a 3440x1440 wallpaper is 5M pixels, and k-means over all of
/// them costs seconds while changing the result below visible threshold.
/// Sampling is a fixed stride rather than a random draw so the same wallpaper
/// always yields the same palette.
const SAMPLE_TARGET: usize = 40_000;

/// Clusters to solve for. Larger than the number of colours ultimately used:
/// over-clustering then selecting is what keeps a small vivid region (the one
/// the eye actually reads as "the colour of this wallpaper") from being
/// averaged into the dominant background.
const DEFAULT_CLUSTERS: usize = 12;

/// k-means iteration cap and convergence threshold. ICtCp's I axis spans 0..1
/// rather than CIELAB's 0..100, so the threshold is scaled down accordingly
/// from the crate's Lab default of 5.0.
const MAX_ITER: usize = 20;
const CONVERGE: f32 = 1e-4;

/// A point in ICtCp. `i` is PQ-encoded intensity (0 at black, 1 at 10000
/// cd/m²); `ct` and `cp` are the blue-yellow and red-green chroma axes,
/// nominally within about +/-0.5.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Ictcp {
    pub i: f32,
    pub ct: f32,
    pub cp: f32,
}

impl Ictcp {
    fn add(self, other: Self) -> Self {
        Self { i: self.i + other.i, ct: self.ct + other.ct, cp: self.cp + other.cp }
    }

    fn sub(self, other: Self) -> Self {
        Self { i: self.i - other.i, ct: self.ct - other.ct, cp: self.cp - other.cp }
    }

    fn scale(self, factor: f32) -> Self {
        Self { i: self.i * factor, ct: self.ct * factor, cp: self.cp * factor }
    }

    fn norm_squared(self) -> f32 {
        self.i * self.i + self.ct * self.ct + self.cp * self.cp
    }
}

impl Calculate for Ictcp {
    fn get_closest_centroid(buffer: &[Self], centroids: &[Self], indices: &mut Vec<u8>) {
        for point in buffer {
            let mut index = 0usize;
            let mut min = f32::MAX;
            for (idx, centroid) in centroids.iter().enumerate() {
                let diff = Self::difference(point, centroid);
                if diff < min {
                    min = diff;
                    index = idx;
                }
            }
            // `get_kmeans` caps k at 256 before this is reached, so the cast
            // cannot wrap; the crate's own implementations do the same.
            indices.push(index as u8);
        }
    }

    fn recalculate_centroids(
        rng: &mut impl Rng,
        buf: &[Self],
        centroids: &mut [Self],
        indices: &[u8],
    ) {
        for (idx, centroid) in centroids.iter_mut().enumerate() {
            let mut sum = Self::default();
            let mut count = 0u64;
            for (&assigned, &point) in indices.iter().zip(buf) {
                if usize::from(assigned) == idx {
                    sum = sum.add(point);
                    count += 1;
                }
            }
            *centroid = if count == 0 {
                // An emptied cluster is re-seeded rather than dropped, so k
                // stays constant across iterations.
                Self::create_random(rng)
            } else {
                sum.scale(1.0 / count as f32)
            };
        }
    }

    fn check_loop(centroids: &[Self], old_centroids: &[Self]) -> f32 {
        let mut delta = Self::default();
        for (&new, &old) in centroids.iter().zip(old_centroids) {
            delta = delta.add(new.sub(old));
        }
        delta.norm_squared()
    }

    fn create_random(rng: &mut impl Rng) -> Self {
        Self {
            i: rng.random_range(0.0..=1.0),
            ct: rng.random_range(-0.5..=0.5),
            cp: rng.random_range(-0.5..=0.5),
        }
    }

    fn difference(c1: &Self, c2: &Self) -> f32 {
        c1.sub(*c2).norm_squared()
    }
}

/// One cluster of the wallpaper.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Swatch {
    /// The cluster centroid, in the compositor's shared working space: linear
    /// BT.2020 where 1.0 is 10000 cd/m². Kept in absolute terms rather than
    /// converted to sRGB here so the contrast pass can reason about how bright
    /// this colour actually is; the conversion to a displayable hex triple is
    /// the last step, not this one.
    pub abs10k: [f32; 3],
    /// Share of sampled pixels landing in this cluster, 0..1.
    pub weight: f32,
    /// The centroid's PQ-encoded intensity -- `Ictcp::i`, retained because
    /// sorting and contrast selection both want it and recomputing it from
    /// `abs10k` means running the curve again.
    pub intensity: f32,
}

/// The result of extraction: the wallpaper's clusters plus the distribution
/// facts a contrast pass needs.
#[derive(Clone, Debug)]
pub(crate) struct Palette {
    /// Clusters, most populous first.
    pub swatches: Vec<Swatch>,
    /// Median PQ intensity over the sampled pixels.
    pub intensity_p50: f32,
    /// 95th-percentile PQ intensity. This, not the mean, is what a contrast
    /// solve must clear: text sitting over the bright corner of a wallpaper
    /// fails on a background chosen for the average.
    pub intensity_p95: f32,
    /// 99th-percentile PQ intensity. p95 is blind to a bright region covering
    /// less than a twentieth of the image (see the test that pins this), which
    /// a small sun or window reflection easily is. p99 is the conservative
    /// choice for the *unblurred* backdrop an inactive window shows.
    pub intensity_p99: f32,
}

/// SMPTE ST 2084 inverse EOTF (the "OETF" direction): linear, 1.0 == 10000
/// cd/m², to PQ code 0..1. Same constants as `hdr_shaders`' `pq_oetf` GLSL.
pub(crate) fn pq_encode(value: f32) -> f32 {
    const M1: f32 = 0.159_301_76;
    const M2: f32 = 78.84375;
    const C1: f32 = 0.8359375;
    const C2: f32 = 18.851_563;
    const C3: f32 = 18.6875;
    let ym = value.max(0.0).powf(M1);
    ((C1 + C2 * ym) / (1.0 + C3 * ym)).powf(M2)
}

/// Inverse of [`pq_encode`].
pub(crate) fn pq_decode(code: f32) -> f32 {
    const M1: f32 = 0.159_301_76;
    const M2: f32 = 78.84375;
    const C1: f32 = 0.8359375;
    const C2: f32 = 18.851_563;
    const C3: f32 = 18.6875;
    let e = code.max(0.0).powf(1.0 / M2);
    let numerator = (e - C1).max(0.0);
    let denominator = C2 - C3 * e;
    if denominator <= 0.0 {
        return 1.0;
    }
    (numerator / denominator).powf(1.0 / M1)
}

/// Linear BT.2020 (abs10k) to ICtCp, per Rec. BT.2100.
pub(crate) fn abs10k_to_ictcp(rgb: [f32; 3]) -> Ictcp {
    let [r, g, b] = rgb;
    let l = (1688.0 * r + 2146.0 * g + 262.0 * b) / 4096.0;
    let m = (683.0 * r + 2951.0 * g + 462.0 * b) / 4096.0;
    let s = (99.0 * r + 309.0 * g + 3688.0 * b) / 4096.0;
    let lp = pq_encode(l);
    let mp = pq_encode(m);
    let sp = pq_encode(s);
    Ictcp {
        i: 0.5 * lp + 0.5 * mp,
        ct: (6610.0 * lp - 13613.0 * mp + 7003.0 * sp) / 4096.0,
        cp: (17933.0 * lp - 17390.0 * mp - 543.0 * sp) / 4096.0,
    }
}

/// Inverse of [`abs10k_to_ictcp`]. Round-trip fidelity is what the tests pin:
/// a centroid is only meaningful if it can be carried back to a colour.
pub(crate) fn ictcp_to_abs10k(point: Ictcp) -> [f32; 3] {
    let Ictcp { i, ct, cp } = point;
    let lp = i + 0.008_609_037 * ct + 0.111_029_625 * cp;
    let mp = i - 0.008_609_037 * ct - 0.111_029_625 * cp;
    let sp = i + 0.560_031_1 * ct - 0.320_627_11 * cp;
    let l = pq_decode(lp);
    let m = pq_decode(mp);
    let s = pq_decode(sp);
    [
        3.436_607 * l - 2.506_452_5 * m + 0.069_845_2 * s,
        -0.791_329_2 * l + 1.983_600_2 * m - 0.192_271 * s,
        -0.025_949_2 * l - 0.098_913_8 * m + 1.124_863 * s,
    ]
}

/// Unpack one pixel to linear BT.2020 abs10k, or `None` if the format is not
/// one the wallpaper decoder produces.
fn unpack(bytes: &[u8], fourcc: Fourcc, decode: DecodeKind, sdr_white_nits: f32) -> Option<[f32; 3]> {
    match fourcc {
        // DRM_FORMAT_XBGR8888: little-endian 0xXXBBGGRR, so memory order is
        // R, G, B, X. This is what an SDR wallpaper is uploaded as.
        Fourcc::Xbgr8888 | Fourcc::Abgr8888 => {
            let srgb = Color32F::new(
                f32::from(bytes[0]) / 255.0,
                f32::from(bytes[1]) / 255.0,
                f32::from(bytes[2]) / 255.0,
                1.0,
            );
            match decode {
                // Reuses the SDR decode shader's own CPU twin, so the palette
                // is extracted from the same values the screen is showing.
                DecodeKind::Sdr => {
                    let c = crate::hdr_shaders::srgb_to_bt2020_abs10k(srgb, sdr_white_nits);
                    Some([c.r(), c.g(), c.b()])
                }
                // An 8-bit buffer tagged HDR is not something the decoder
                // produces; treating it as SDR would silently mis-grade it.
                _ => None,
            }
        }
        // DRM_FORMAT_XBGR2101010: little-endian u32 with R in bits 0..9,
        // G in 10..19, B in 20..29. What a PQ wallpaper is uploaded as.
        Fourcc::Xbgr2101010 | Fourcc::Abgr2101010 => {
            let packed = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            let r = (packed & 0x3ff) as f32 / 1023.0;
            let g = ((packed >> 10) & 0x3ff) as f32 / 1023.0;
            let b = ((packed >> 20) & 0x3ff) as f32 / 1023.0;
            match decode {
                // PQ code straight to abs10k -- the curve is the storage
                // format, so decoding it is the whole conversion.
                DecodeKind::HdrPq => Some([pq_decode(r), pq_decode(g), pq_decode(b)]),
                // scRGB is already linear BT.709 with 1.0 == 80 cd/m².
                DecodeKind::WindowsScrgb => {
                    let c = Color32F::new(r, g, b, 1.0);
                    let c = crate::hdr_shaders::srgb_to_bt2020_abs10k(c, 80.0);
                    Some([c.r(), c.g(), c.b()])
                }
                DecodeKind::Sdr => {
                    let c = crate::hdr_shaders::srgb_to_bt2020_abs10k(
                        Color32F::new(r, g, b, 1.0),
                        sdr_white_nits,
                    );
                    Some([c.r(), c.g(), c.b()])
                }
            }
        }
        _ => None,
    }
}

/// Bytes per pixel for the formats [`unpack`] handles.
fn bytes_per_pixel(fourcc: Fourcc) -> Option<usize> {
    match fourcc {
        Fourcc::Xbgr8888
        | Fourcc::Abgr8888
        | Fourcc::Xbgr2101010
        | Fourcc::Abgr2101010 => Some(4),
        _ => None,
    }
}

/// Extract a palette from decoded wallpaper pixels.
///
/// Deliberately takes the raw parts rather than a `&Decoded`, so it needs no
/// renderer, no GPU and no wallpaper state -- which is what lets the whole
/// colour path be unit-tested and driven from the `rubix palette` subcommand
/// without starting a compositor.
///
/// Returns `None` for a format this cannot read, rather than guessing: a
/// mis-unpacked buffer would still produce a plausible-looking palette, which
/// is the failure mode hardest to notice.
pub(crate) fn extract(
    pixels: &[u8],
    size: Size<i32, BufferCoords>,
    fourcc: Fourcc,
    decode: DecodeKind,
    sdr_white_nits: f32,
) -> Option<Palette> {
    let bpp = bytes_per_pixel(fourcc)?;
    let width = usize::try_from(size.w).ok()?;
    let height = usize::try_from(size.h).ok()?;
    let total = width.checked_mul(height)?;
    if total == 0 || pixels.len() < total * bpp {
        return None;
    }

    // Fixed stride, so the same image always samples the same pixels and the
    // palette is reproducible across runs.
    let stride = (total / SAMPLE_TARGET).max(1);
    let mut points: Vec<Ictcp> = Vec::with_capacity(total / stride + 1);
    for index in (0..total).step_by(stride) {
        let offset = index * bpp;
        let abs10k = unpack(&pixels[offset..offset + bpp], fourcc, decode, sdr_white_nits)?;
        points.push(abs10k_to_ictcp(abs10k));
    }
    if points.is_empty() {
        return None;
    }

    let mut intensities: Vec<f32> = points.iter().map(|p| p.i).collect();
    intensities.sort_by(f32::total_cmp);
    let percentile = |q: f32| -> f32 {
        let last = intensities.len() - 1;
        // Clamped rather than trusted to land in range: rounding at q == 1.0
        // can produce `last + 1` on some inputs.
        let index = (((last as f32) * q).round().max(0.0) as usize).min(last);
        intensities[index]
    };

    // A fixed seed keeps the palette stable: k-means++ is randomised, and an
    // unseeded run would re-theme the desktop slightly differently every time
    // the same wallpaper came back around in the slideshow.
    let result = get_kmeans(DEFAULT_CLUSTERS, MAX_ITER, CONVERGE, false, &points, 0);

    let mut counts = vec![0usize; result.centroids.len()];
    for &index in &result.indices {
        if let Some(slot) = counts.get_mut(usize::from(index)) {
            *slot += 1;
        }
    }
    let sampled = result.indices.len().max(1) as f32;

    let mut swatches: Vec<Swatch> = result
        .centroids
        .iter()
        .zip(&counts)
        .map(|(&centroid, &count)| Swatch {
            abs10k: ictcp_to_abs10k(centroid),
            weight: count as f32 / sampled,
            intensity: centroid.i,
        })
        .collect();
    // Weight descending, with intensity as a tiebreaker so two equally
    // populous clusters do not swap order between runs.
    swatches.sort_by(|a, b| {
        b.weight
            .total_cmp(&a.weight)
            .then_with(|| a.intensity.total_cmp(&b.intensity))
    });

    Some(Palette {
        swatches,
        intensity_p50: percentile(0.50),
        intensity_p95: percentile(0.95),
        intensity_p99: percentile(0.99),
    })
}

/// A *preview* sRGB hex for a swatch, for human inspection only.
///
/// Not the colour a theme should use. This normalises absolute luminance
/// against `sdr_white_nits` and clips, which is exactly the lossy step the rest
/// of this module exists to avoid -- two swatches differing only above SDR
/// white collapse to the same string here. It is how `rubix palette` prints
/// something a person can look at; the contrast pass works from `abs10k`.
pub(crate) fn preview_hex(abs10k: [f32; 3], sdr_white_nits: f32) -> String {
    let scale = 10_000.0 / sdr_white_nits.max(1.0);
    let [r, g, b] = [abs10k[0] * scale, abs10k[1] * scale, abs10k[2] * scale];
    // BT.2020 -> BT.709, the inverse of the matrix in `srgb_to_bt2020_abs10k`.
    let lin709 = [
        1.660_491 * r - 0.587_641_1 * g - 0.072_850_1 * b,
        -0.124_550_3 * r + 1.132_899_4 * g - 0.008_349_2 * b,
        -0.018_154_3 * r - 0.100_578_8 * g + 1.118_733 * b,
    ];
    let encode = |c: f32| -> u8 {
        let c = c.clamp(0.0, 1.0);
        let srgb = if c <= 0.003_130_8 { c * 12.92 } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 };
        (srgb * 255.0).round().clamp(0.0, 255.0) as u8
    };
    format!("#{:02x}{:02x}{:02x}", encode(lin709[0]), encode(lin709[1]), encode(lin709[2]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size(w: i32, h: i32) -> Size<i32, BufferCoords> {
        Size::from((w, h))
    }

    /// Pack a linear abs10k colour as a PQ Xbgr2101010 pixel, i.e. the inverse
    /// of what `unpack` does, so tests can build buffers in the terms the rest
    /// of the module thinks in.
    fn pq_pixel(abs10k: [f32; 3]) -> [u8; 4] {
        let code = |v: f32| ((pq_encode(v).clamp(0.0, 1.0) * 1023.0).round() as u32) & 0x3ff;
        let packed = code(abs10k[0]) | (code(abs10k[1]) << 10) | (code(abs10k[2]) << 20);
        packed.to_le_bytes()
    }

    #[test]
    fn pq_round_trips_across_the_whole_range() {
        // Includes values near black, where PQ's curve is near-vertical and a
        // naive inverse loses all precision -- the same region that produced
        // the HDR border grain.
        for &v in &[0.0, 1e-5, 1e-4, 1e-3, 0.01, 0.1, 0.5, 1.0] {
            let back = pq_decode(pq_encode(v));
            assert!(
                (back - v).abs() <= 1e-4 * v.max(1e-3),
                "pq round trip failed at {v}: got {back}"
            );
        }
    }

    #[test]
    fn ictcp_round_trips_for_saturated_and_neutral_colours() {
        // The inverse matrix is the part most likely to be subtly wrong: a bad
        // one still yields plausible colours, just not the input ones.
        let cases: [[f32; 3]; 6] = [
            [0.01, 0.01, 0.01],
            [0.02, 0.0, 0.0],
            [0.0, 0.02, 0.0],
            [0.0, 0.0, 0.02],
            [0.005, 0.012, 0.03],
            [0.1, 0.1, 0.1],
        ];
        for input in cases {
            let back = ictcp_to_abs10k(abs10k_to_ictcp(input));
            for channel in 0..3 {
                assert!(
                    (back[channel] - input[channel]).abs() <= 1e-3,
                    "ictcp round trip failed for {input:?}: got {back:?}"
                );
            }
        }
    }

    #[test]
    fn a_two_tone_image_yields_both_tones_with_their_true_weights() {
        // Three quarters dim, one quarter bright. Weights are what the theme
        // ultimately keys off, so an off-by-one in the sampling stride or the
        // count would show up here rather than as a subtly wrong palette.
        let dim = [0.002, 0.002, 0.004];
        let bright = [0.05, 0.02, 0.01];
        let (w, h) = (200, 200);
        let mut pixels = Vec::with_capacity(w * h * 4);
        for index in 0..(w * h) {
            pixels.extend_from_slice(&pq_pixel(if index % 4 == 0 { bright } else { dim }));
        }

        let palette = extract(
            &pixels,
            size(w as i32, h as i32),
            Fourcc::Xbgr2101010,
            DecodeKind::HdrPq,
            203.0,
        )
        .expect("PQ buffer should extract");

        let heaviest = palette.swatches.first().expect("at least one swatch");
        assert!(
            heaviest.weight > 0.5,
            "dominant swatch should carry the 75% tone, got {}",
            heaviest.weight
        );
        // The bright tone must survive as its own cluster rather than being
        // averaged into the dominant one -- that averaging is exactly what
        // makes naive extraction produce muddy palettes.
        let bright_total: f32 = palette
            .swatches
            .iter()
            .filter(|s| s.intensity > heaviest.intensity + 0.05)
            .map(|s| s.weight)
            .sum();
        assert!(
            (bright_total - 0.25).abs() < 0.05,
            "bright tone should hold ~25% of the image, got {bright_total}"
        );
    }

    /// Build a buffer where `bright_percent` of pixels are bright and the
    /// rest are near-black.
    fn speckled(bright_percent: usize) -> (Vec<u8>, i32, i32) {
        let (w, h) = (100, 100);
        let mut pixels = Vec::with_capacity(w * h * 4);
        for index in 0..(w * h) {
            let bright = index % 100 < bright_percent;
            pixels.extend_from_slice(&pq_pixel(if bright {
                [0.4, 0.4, 0.4]
            } else {
                [0.001, 0.001, 0.001]
            }));
        }
        (pixels, w as i32, h as i32)
    }

    fn extract_speckled(bright_percent: usize) -> Palette {
        let (pixels, w, h) = speckled(bright_percent);
        extract(&pixels, size(w, h), Fourcc::Xbgr2101010, DecodeKind::HdrPq, 203.0)
            .expect("PQ buffer should extract")
    }

    #[test]
    fn percentiles_separate_a_bright_minority_from_the_median() {
        // Mostly dark with a bright tenth: the mean would say "dark and safe",
        // while p95 -- what a contrast solve must clear -- says otherwise.
        let palette = extract_speckled(10);
        assert!(
            palette.intensity_p95 > palette.intensity_p50 + 0.2,
            "p95 {} should sit well above p50 {}",
            palette.intensity_p95,
            palette.intensity_p50
        );
    }

    #[test]
    fn p95_cannot_see_a_bright_region_smaller_than_a_twentieth() {
        // Pinning a real limitation rather than asserting a behaviour: by
        // definition p95 sits inside the darkest 95% of the image, so a bright
        // patch covering less than that is invisible to it and text over that
        // patch would be sized for the dark majority.
        //
        // This is why a contrast solve must eventually sample the wallpaper
        // region a window actually covers, rather than a whole-image statistic:
        // a small bright corner is a local problem, and a global percentile is
        // the wrong instrument for it.
        let palette = extract_speckled(3);
        assert!(
            (palette.intensity_p95 - palette.intensity_p50).abs() < 1e-6,
            "a 3% bright patch is expected to be invisible to p95: p95 {} p50 {}",
            palette.intensity_p95,
            palette.intensity_p50
        );
    }

    #[test]
    fn an_sdr_buffer_extracts_through_the_shader_s_own_conversion() {
        // Mid grey at the default SDR reference. The point is not the exact
        // value but that the SDR path lands in the same absolute space as the
        // PQ path, so both can be compared and themed identically.
        let (w, h) = (64, 64);
        let mut pixels = Vec::with_capacity(w * h * 4);
        for _ in 0..(w * h) {
            pixels.extend_from_slice(&[128, 128, 128, 255]);
        }
        let palette = extract(
            &pixels,
            size(w as i32, h as i32),
            Fourcc::Xbgr8888,
            DecodeKind::Sdr,
            203.0,
        )
        .expect("SDR buffer should extract");
        let expected = crate::hdr_shaders::srgb_to_bt2020_abs10k(
            Color32F::new(128.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0, 1.0),
            203.0,
        );
        let dominant = palette.swatches.first().expect("at least one swatch");
        assert!(
            (dominant.abs10k[1] - expected.g()).abs() < 1e-3,
            "SDR grey should match the decode shader's CPU twin: {:?} vs {}",
            dominant.abs10k,
            expected.g()
        );
    }

    #[test]
    fn an_unreadable_format_is_refused_rather_than_guessed() {
        // A mis-unpacked buffer still yields a plausible palette, which is the
        // failure mode hardest to notice -- so this must be None, not garbage.
        let pixels = vec![0u8; 64 * 64 * 4];
        assert!(
            extract(&pixels, size(64, 64), Fourcc::Yuyv, DecodeKind::Sdr, 203.0).is_none()
        );
    }

    #[test]
    fn a_truncated_buffer_is_refused() {
        let pixels = vec![0u8; 8];
        assert!(
            extract(&pixels, size(64, 64), Fourcc::Xbgr8888, DecodeKind::Sdr, 203.0).is_none()
        );
    }

    #[test]
    fn the_same_image_always_yields_the_same_palette() {
        // k-means++ is randomised; without the fixed seed and fixed sampling
        // stride the desktop would re-theme slightly differently every time the
        // slideshow came back around to the same wallpaper.
        let (w, h) = (80, 80);
        let mut pixels = Vec::with_capacity(w * h * 4);
        for index in 0..(w * h) {
            let t = (index % 40) as f32 / 40.0;
            pixels.extend_from_slice(&pq_pixel([0.01 + t * 0.05, 0.02, 0.03 * t]));
        }
        let first = extract(&pixels, size(w as i32, h as i32), Fourcc::Xbgr2101010, DecodeKind::HdrPq, 203.0).unwrap();
        let second = extract(&pixels, size(w as i32, h as i32), Fourcc::Xbgr2101010, DecodeKind::HdrPq, 203.0).unwrap();
        assert_eq!(first.swatches, second.swatches);
    }
}
