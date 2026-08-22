//! Contrast-solved theme generation.
//!
//! ## The problem this exists to solve
//!
//! Rubix draws windows translucent over a refracted wallpaper, so what sits
//! behind a glyph is not the application's background colour -- it is
//!
//! ```text
//!     background * opacity + backdrop * (1 - opacity)
//! ```
//!
//! No application can reason about that. Kitty does not know what is behind
//! Kitty. The compositor is the only component holding both the text colour and
//! what it will be composited over, which is what makes contrast a
//! compositor-shaped problem rather than a theming afterthought.
//!
//! So colours here are not picked and hoped for. A foreground is *solved*: its
//! intensity is raised until it clears a contrast target against the worst
//! background it will realistically meet, or until it runs out of range and
//! reports that it could not.
//!
//! ## Why APCA
//!
//! WCAG 2's contrast ratio is a fixed formula that systematically overstates
//! the legibility of light-on-dark -- precisely this theme's case. APCA (the
//! WCAG 3 draft model) is polarity-aware and perceptually derived, so a solve
//! against it corresponds to something a person can actually read. `Lc` values
//! run 0..106; roughly, 90 is a floor for small text, 75 for body text, 60 for
//! large or secondary text, and 45 for non-text such as borders.
//!
//! APCA is defined over sRGB display values, so the solve normalises absolute
//! luminance against `sdr_white_nits` first. That is deliberate and not a
//! shortcut: text is SDR-range content, and its legibility is governed by the
//! SDR portion of the display's range even when the wallpaper behind it is not.
//!
//! ## Which background is "worst"
//!
//! Not the average. A theme keyed to the mean wallpaper luminance fails
//! everywhere the wallpaper is brighter than its mean, which is half of it.
//! The solve uses a high percentile, and picks *which* percentile from how the
//! backdrop is actually composited:
//!
//! * A **blurred** backdrop has already averaged small bright specks away, so
//!   p95 describes it well.
//! * A **sharp** backdrop has not, so a small bright highlight survives at full
//!   strength and p99 is the honest figure.
//!
//! The theme is emitted once per wallpaper and shared by every application, so
//! the worst case has to be global. Per-window sampling would be more precise
//! but cannot be acted on: applications are themed as a whole, not per window
//! position.

use crate::palette::{Palette, Swatch};

/// APCA 0.1.9 constants. Named as in the reference implementation so they can
/// be checked against it directly.
mod apca {
    pub const MAIN_TRC: f32 = 2.4;
    pub const R_CO: f32 = 0.2126729;
    pub const G_CO: f32 = 0.7151522;
    pub const B_CO: f32 = 0.0721750;
    pub const NORM_BG: f32 = 0.56;
    pub const NORM_TXT: f32 = 0.57;
    pub const REV_TXT: f32 = 0.62;
    pub const REV_BG: f32 = 0.65;
    pub const BLK_THRS: f32 = 0.022;
    pub const BLK_CLMP: f32 = 1.414;
    pub const SCALE: f32 = 1.14;
    pub const LO_OFFSET: f32 = 0.027;
    pub const LO_CLIP: f32 = 0.1;
}

/// Chroma multiplier and floor for the glow. A glow is the one element allowed
/// to be genuinely saturated: it is thin, it is transient, and nothing is read
/// through it, so the legibility cost that keeps text near-neutral does not
/// apply. The floor matters for near-monochrome wallpapers, where scaling a
/// swatch's own chroma alone would produce a grey glow.
const GLOW_CHROMA_GAIN: f32 = 1.8;

/// Minimum PQ intensity for a swatch to be usable as a border colour. Below
/// this a swatch is effectively black, and its hue is rounding error.
const GLOW_MIN_SOURCE_INTENSITY: f32 = 0.28;

/// How bright the finished border must be, as its largest display-sRGB
/// channel.
///
/// Saturation and luminance trade against each other at the edge of the gamut:
/// a hue that sRGB can only just represent stays in gamut at high chroma *only*
/// while it is very dark. Maximising chroma first therefore picks the dark end
/// of that trade and yields a near-black border -- which no amount of HDR
/// luminance can rescue, because scaling a black colour leaves it black.
/// Brightness is the requirement and chroma is what gets spent to reach it.
const GLOW_MIN_BRIGHTNESS: f32 = 0.62;
const GLOW_CHROMA_FLOOR: f32 = 0.07;
const GLOW_CHROMA_CEILING: f32 = 0.22;

/// Largest sRGB channel the glow may reach before it counts as clipped.
///
/// The glow is not solved for contrast the way text is. It is drawn as an HDR
/// element at `active_luminance_nits`, so its separation from the wallpaper
/// comes from luminance the compositor applies afterwards -- driving intensity
/// up here to win a contrast target instead just clips every channel and the
/// hue washes out to white, which is what a first attempt did on four of six
/// wallpapers. What the solve owes is the most luminous version of the chosen
/// hue that still fits in gamut, and the compositor supplies the brightness.
const GLOW_CLIP_LIMIT: f32 = 0.995;

/// Contrast targets, in APCA `Lc`.
pub(crate) const LC_BODY: f32 = 75.0;
pub(crate) const LC_SECONDARY: f32 = 60.0;
pub(crate) const LC_NONTEXT: f32 = 45.0;

/// Inputs to the solve, all of which the compositor already knows.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ThemeParams {
    /// Window opacity, as a transmittance: `1.0 - opacity` of the backdrop
    /// shows through. The *lower* of the active and inactive opacities is the
    /// binding one, since that window shows the most wallpaper.
    pub opacity: f32,
    /// Ceiling applied to the backdrop, in cd/m². `None` when the backdrop is
    /// not tone-mapped, in which case the wallpaper's own luminance stands --
    /// which is the case that forces the theme darkest.
    pub backdrop_cap_nits: Option<f32>,
    /// Whether the backdrop is blurred, which decides whether p95 or p99 is
    /// the honest worst case.
    pub backdrop_blurred: bool,
    pub sdr_white_nits: f32,
    /// Contrast target for body text.
    pub target_lc: f32,
}

impl Default for ThemeParams {
    fn default() -> Self {
        Self {
            opacity: 0.85,
            backdrop_cap_nits: Some(100.0),
            backdrop_blurred: true,
            sdr_white_nits: 203.0,
            target_lc: LC_BODY,
        }
    }
}

/// A solved colour, kept in absolute terms with the contrast it achieved.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ThemeColor {
    pub abs10k: [f32; 3],
    /// APCA `Lc` against the effective background. Zero for the background
    /// itself. Negative magnitude is normalised away -- this is always the
    /// absolute contrast.
    pub lc: f32,
}

/// The generated theme. Semantic roles rather than pywal's numbered palette:
/// consumers ask for "the colour of body text", not "colour 7".
#[derive(Clone, Debug)]
pub(crate) struct Theme {
    /// Window content background -- what applications paint.
    pub background: ThemeColor,
    /// A slightly raised background for panels, code blocks, inactive tabs.
    pub surface: ThemeColor,
    /// Body text.
    pub foreground: ThemeColor,
    /// Secondary text: comments, hints, inactive labels.
    pub muted: ThemeColor,
    /// The wallpaper's own accent, solved to stay legible as text.
    pub accent: ThemeColor,
    /// Borders and separators -- non-text, so a lower target.
    pub border: ThemeColor,
    /// The focused window's border glow. Unlike every other role this one is
    /// meant to *stand out* rather than recede, so it sits away from the
    /// accent on the hue circle and is solved against the wallpaper rather
    /// than against the window background -- a glow spills onto the desktop,
    /// not onto the content.
    pub glow: ThemeColor,
    /// Terminal semantics, in ANSI order: red, green, yellow, blue, magenta,
    /// cyan. Hue-anchored so red stays recognisably red, but pulled toward the
    /// wallpaper's chroma so the set reads as one family.
    pub ansi: [ThemeColor; 6],
    /// The effective background the solve worked against, for diagnostics.
    pub effective_background: [f32; 3],
    /// True when every solved colour met its target. False means the wallpaper
    /// could not support the requested contrast and the closest miss stands --
    /// reported rather than silently accepted.
    pub met_targets: bool,
}

/// Relative luminance in APCA's sense, from display-referred sRGB 0..1.
fn apca_luminance(srgb: [f32; 3]) -> f32 {
    apca::R_CO * srgb[0].max(0.0).powf(apca::MAIN_TRC)
        + apca::G_CO * srgb[1].max(0.0).powf(apca::MAIN_TRC)
        + apca::B_CO * srgb[2].max(0.0).powf(apca::MAIN_TRC)
}

/// Soft-clamp near black, per APCA: without it, contrast against very dark
/// backgrounds is overstated -- the same region where PQ makes rounding error
/// visible.
fn soft_clamp(y: f32) -> f32 {
    if y < apca::BLK_THRS {
        y + (apca::BLK_THRS - y).powf(apca::BLK_CLMP)
    } else {
        y
    }
}

/// APCA `Lc` for text over a background, both display-referred sRGB 0..1.
/// Returned as a magnitude: polarity is handled internally, and callers here
/// only ever ask "is there enough contrast".
pub(crate) fn apca_lc(text_srgb: [f32; 3], bg_srgb: [f32; 3]) -> f32 {
    let y_txt = soft_clamp(apca_luminance(text_srgb));
    let y_bg = soft_clamp(apca_luminance(bg_srgb));
    let sapc = if y_bg > y_txt {
        // Dark text on a light background.
        (y_bg.powf(apca::NORM_BG) - y_txt.powf(apca::NORM_TXT)) * apca::SCALE
    } else {
        // Light text on a dark background.
        (y_bg.powf(apca::REV_BG) - y_txt.powf(apca::REV_TXT)) * apca::SCALE
    };
    let output = if sapc.abs() < apca::LO_CLIP {
        0.0
    } else if sapc > 0.0 {
        sapc - apca::LO_OFFSET
    } else {
        sapc + apca::LO_OFFSET
    };
    (output * 100.0).abs()
}

/// abs10k (linear BT.2020) to display-referred sRGB 0..1, normalised against
/// SDR white. Clipping here is correct rather than lossy: APCA describes what
/// an SDR display shows, and anything above SDR white is already clipped by the
/// time text is read against it.
pub(crate) fn abs10k_to_display_srgb(abs10k: [f32; 3], sdr_white_nits: f32) -> [f32; 3] {
    let scale = 10_000.0 / sdr_white_nits.max(1.0);
    let [r, g, b] = [abs10k[0] * scale, abs10k[1] * scale, abs10k[2] * scale];
    let lin709 = [
        1.660_491 * r - 0.587_641_1 * g - 0.072_850_1 * b,
        -0.124_550_3 * r + 1.132_899_4 * g - 0.008_349_2 * b,
        -0.018_154_3 * r - 0.100_578_8 * g + 1.118_733 * b,
    ];
    let encode = |c: f32| {
        let c = c.clamp(0.0, 1.0);
        if c <= 0.003_130_8 {
            c * 12.92
        } else {
            1.055 * c.powf(1.0 / 2.4) - 0.055
        }
    };
    [encode(lin709[0]), encode(lin709[1]), encode(lin709[2])]
}

/// The background a glyph is actually composited over.
pub(crate) fn effective_background(
    content_bg: [f32; 3],
    backdrop: [f32; 3],
    opacity: f32,
) -> [f32; 3] {
    let o = opacity.clamp(0.0, 1.0);
    [
        content_bg[0] * o + backdrop[0] * (1.0 - o),
        content_bg[1] * o + backdrop[1] * (1.0 - o),
        content_bg[2] * o + backdrop[2] * (1.0 - o),
    ]
}

/// Hue and chroma of an ICtCp point, as a polar pair.
fn hue_chroma(point: crate::palette::Ictcp) -> (f32, f32) {
    (point.ct.atan2(point.cp), (point.ct * point.ct + point.cp * point.cp).sqrt())
}

/// Rebuild an ICtCp point from intensity plus a polar hue/chroma.
fn from_polar(intensity: f32, hue: f32, chroma: f32) -> crate::palette::Ictcp {
    crate::palette::Ictcp {
        i: intensity,
        ct: chroma * hue.sin(),
        cp: chroma * hue.cos(),
    }
}

/// How far the terminal palette's hues are pulled toward the wallpaper's own.
///
/// Zero leaves the reference hues untouched, which makes the ANSI set identical
/// under every wallpaper -- the palette stops participating in the theme, and
/// anything drawn mostly from it (a `neofetch` block, a shell prompt, syntax
/// highlighting) looks static no matter what the desktop is doing. One pulls
/// every hue onto the accent, collapsing red, green and blue into a single
/// colour and destroying the semantics.
///
/// A fraction alone is not safe: it is measured against the distance to the
/// accent, so a wallpaper on the far side of the wheel drags a hue much further
/// in absolute terms than a near one. At a third, a blue wallpaper turned red
/// into magenta and collapsed it onto the magenta slot exactly.
///
/// So the pull is a fraction *and* a hard ceiling on how far any hue may move.
/// The ceiling is what preserves identity in the worst case; the fraction keeps
/// nearer wallpapers from all snapping to the same offset.
const ANSI_HUE_PULL: f32 = 0.18;

/// Maximum hue rotation, in radians (~14 degrees). Enough to shift a red
/// warmer or cooler; not enough to make it any other colour. Pinned by
/// `terminal_hues_stay_near_their_references_under_every_accent`, which walks
/// the whole hue circle rather than trusting a few sampled wallpapers.
const ANSI_HUE_PULL_CEILING: f32 = 0.25;

/// Rotate `hue` toward `target` by `amount`, along the shorter way round.
///
/// Hue is an angle, so naive interpolation between, say, 175 degrees and -175
/// travels the long way through the entire wheel and lands on the opposite
/// colour. Normalising the delta into [-pi, pi] first is what keeps a red near
/// the wrap point from turning cyan.
fn rotate_toward(hue: f32, target: f32, amount: f32) -> f32 {
    use std::f32::consts::{PI, TAU};
    let mut delta = (target - hue) % TAU;
    if delta > PI {
        delta -= TAU;
    } else if delta < -PI {
        delta += TAU;
    }
    hue + (delta * amount).clamp(-ANSI_HUE_PULL_CEILING, ANSI_HUE_PULL_CEILING)
}

/// Raise a colour's intensity until it clears `target_lc` against `bg_srgb`.
///
/// Binary search rather than a formula: APCA has no closed-form inverse, and
/// the conversion from ICtCp intensity to display sRGB passes through a matrix
/// and two curves. Contrast is monotonic in intensity once hue and chroma are
/// fixed, which is what makes the search valid.
///
/// Returns the dimmest colour meeting the target. If even full intensity
/// misses -- a wallpaper so bright that nothing readable sits on it -- the
/// brightest attempt is returned and the caller learns it fell short from the
/// reported `lc`.
fn solve_intensity(
    hue: f32,
    chroma: f32,
    bg_srgb: [f32; 3],
    target_lc: f32,
    sdr_white_nits: f32,
    floor: f32,
) -> ThemeColor {
    let evaluate = |intensity: f32| -> ([f32; 3], f32) {
        let abs10k = crate::palette::ictcp_to_abs10k(from_polar(intensity, hue, chroma));
        let abs10k = [abs10k[0].max(0.0), abs10k[1].max(0.0), abs10k[2].max(0.0)];
        let lc = apca_lc(abs10k_to_display_srgb(abs10k, sdr_white_nits), bg_srgb);
        (abs10k, lc)
    };

    let (top_abs10k, top_lc) = evaluate(1.0);
    if top_lc < target_lc {
        // Nothing in range clears the bar; hand back the best available and
        // let the caller report the shortfall rather than pretending.
        return ThemeColor { abs10k: top_abs10k, lc: top_lc };
    }

    let (mut lo, mut hi) = (floor.clamp(0.0, 1.0), 1.0f32);
    // 24 halvings resolve intensity far below a display code step, so the
    // result is exact for any purpose downstream.
    for _ in 0..24 {
        let mid = 0.5 * (lo + hi);
        if evaluate(mid).1 >= target_lc {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    let (abs10k, lc) = evaluate(hi);
    ThemeColor { abs10k, lc }
}

/// Solve for contrast like `solve_intensity`, but back off chroma until the
/// result is a colour sRGB can actually show.
///
/// A wallpaper made of neon -- pure reds, pure cyans -- yields an accent whose
/// chroma sits outside the display gamut. Solving intensity against it drives
/// two channels past 1.0 and a third below 0, and the clamp turns whatever hue
/// was asked for into a screaming primary: one such wallpaper produced
/// `#ff00ff`. Reducing chroma keeps the hue and gives up only the saturation
/// the display could not have shown in the first place.
fn solve_intensity_in_gamut(
    hue: f32,
    chroma: f32,
    bg_srgb: [f32; 3],
    target_lc: f32,
    sdr_white_nits: f32,
    floor: f32,
) -> ThemeColor {
    // Descends all the way to zero chroma, which is always in gamut, so this
    // cannot fall through to a clamped colour. Stopping short left a wallpaper
    // of pure primaries with no reachable option and handed back the clipped
    // result anyway.
    const STEPS: usize = 12;
    let mut last = None;
    for step in 0..=STEPS {
        let attempt = chroma * (1.0 - step as f32 / STEPS as f32);
        let solved = solve_intensity(hue, attempt, bg_srgb, target_lc, sdr_white_nits, floor);
        let raw = abs10k_to_display_srgb_unclamped(solved.abs10k, sdr_white_nits);
        if raw.iter().all(|&c| (-0.002..=GLOW_CLIP_LIMIT).contains(&c)) {
            return solved;
        }
        last = Some(solved);
    }
    // Unreachable in practice -- zero chroma is a neutral -- but a neutral is
    // still the right answer if it ever is.
    last.unwrap_or_else(|| {
        solve_intensity(hue, 0.0, bg_srgb, target_lc, sdr_white_nits, floor)
    })
}

/// The most luminous in-gamut form of a hue, for elements whose brightness is
/// applied later rather than solved here.
///
/// Raising intensity at fixed hue and chroma eventually pushes a channel past
/// 1.0, and clamping that channel is what turns a saturated colour white. So
/// the search looks for the highest intensity that clips nothing. Chroma is
/// stepped down only if no intensity works at all, which happens when the
/// requested chroma sits outside the display gamut for that hue.
fn solve_vivid(hue: f32, chroma: f32, sdr_white_nits: f32) -> ([f32; 3], f32) {
    let brightest_in_gamut = |attempt: f32| -> Option<([f32; 3], f32)> {
        let in_gamut = |intensity: f32| -> Option<[f32; 3]> {
            let abs10k = crate::palette::ictcp_to_abs10k(from_polar(intensity, hue, attempt));
            let srgb = abs10k_to_display_srgb_unclamped(abs10k, sdr_white_nits);
            let fits = srgb.iter().all(|&c| (-0.002..=GLOW_CLIP_LIMIT).contains(&c));
            fits.then_some(abs10k)
        };
        // The in-gamut band for a saturated hue is an interval, not everything
        // below a ceiling: a dark saturated colour is out of gamut too. Scan
        // for the band first, then refine its top edge.
        let mut anchor = None;
        for coarse in 0..40 {
            let intensity = 0.05 + 0.9 * (coarse as f32) / 39.0;
            if in_gamut(intensity).is_some() {
                anchor = Some(intensity);
            } else if anchor.is_some() {
                break; // past the top of the band
            }
        }
        let anchor = anchor?;
        let (mut lo, mut hi) = (anchor, 1.0f32);
        for _ in 0..24 {
            let mid = 0.5 * (lo + hi);
            if in_gamut(mid).is_some() {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        let abs10k = in_gamut(lo)?;
        let brightness = abs10k_to_display_srgb(abs10k, sdr_white_nits)
            .iter()
            .copied()
            .fold(0.0f32, f32::max);
        Some((abs10k, brightness))
    };

    // Descending chroma, taking the first attempt bright enough to read as a
    // border. Chroma is spent to buy brightness rather than the other way
    // round -- see `GLOW_MIN_BRIGHTNESS`.
    let mut best: Option<([f32; 3], f32, f32)> = None;
    for step in 0..10 {
        let attempt = chroma * (1.0 - 0.09 * step as f32);
        let Some((abs10k, brightness)) = brightest_in_gamut(attempt) else { continue };
        if brightness >= GLOW_MIN_BRIGHTNESS {
            return (abs10k, attempt);
        }
        if best.is_none_or(|(_, _, best_brightness)| brightness > best_brightness) {
            best = Some((abs10k, attempt, brightness));
        }
    }
    if let Some((abs10k, attempt, _)) = best {
        return (abs10k, attempt);
    }
    // Every chroma failed, which means the hue itself is unreachable; fall back
    // to a neutral rather than returning something out of gamut.
    let abs10k = crate::palette::ictcp_to_abs10k(from_polar(0.5, hue, 0.0));
    (abs10k, 0.0)
}

/// Like `abs10k_to_display_srgb` but without the clamp, so a caller can tell
/// whether a colour actually fits in gamut instead of being handed the clipped
/// version and no way to know.
fn abs10k_to_display_srgb_unclamped(abs10k: [f32; 3], sdr_white_nits: f32) -> [f32; 3] {
    let scale = 10_000.0 / sdr_white_nits.max(1.0);
    let [r, g, b] = [abs10k[0] * scale, abs10k[1] * scale, abs10k[2] * scale];
    let lin709 = [
        1.660_491 * r - 0.587_641_1 * g - 0.072_850_1 * b,
        -0.124_550_3 * r + 1.132_899_4 * g - 0.008_349_2 * b,
        -0.018_154_3 * r - 0.100_578_8 * g + 1.118_733 * b,
    ];
    let encode = |c: f32| {
        if c <= 0.003_130_8 {
            c * 12.92
        } else {
            1.055 * c.max(0.0).powf(1.0 / 2.4) - 0.055
        }
    };
    [encode(lin709[0]), encode(lin709[1]), encode(lin709[2])]
}

/// The worst background a glyph will meet: a neutral at the relevant
/// percentile of wallpaper intensity, with the backdrop ceiling applied.
fn worst_case_backdrop(palette: &Palette, params: &ThemeParams) -> [f32; 3] {
    // Blur has already averaged small bright specks away, so p95 describes a
    // blurred backdrop; a sharp one keeps them and needs p99.
    let intensity = if params.backdrop_blurred {
        palette.intensity_p95
    } else {
        palette.intensity_p99
    };
    let luminance = crate::palette::pq_decode(intensity);
    let capped = match params.backdrop_cap_nits {
        Some(cap) => luminance.min(cap.max(0.0) / 10_000.0),
        None => luminance,
    };
    [capped, capped, capped]
}

/// The swatch that best represents "the colour of this wallpaper".
///
/// Weight alone picks the background, which is usually a desaturated
/// near-neutral -- the least characteristic colour present. Chroma alone picks
/// a stray vivid pixel. The product favours a colour that is both saturated
/// and actually present, which is what a person means by the wallpaper's
/// colour.
/// The swatch the border takes its colour from.
///
/// A colour that is actually *in* the wallpaper, rather than one computed to
/// oppose it: a synthesized complement stands out, but it reads as an
/// unrelated colour laid on top, because nothing else on screen is that hue.
/// Picking a real swatch keeps the border in the same family as everything
/// else the wallpaper produced.
///
/// The most populous swatch is skipped -- that is the wallpaper's own
/// background, the one colour guaranteed not to stand out against itself.
/// Among the rest the scoring is the accent's: chroma weighted by how much of
/// the image carries it, so a vivid colour that is genuinely present beats both
/// a stray pixel and a large dull field.
fn glow_swatch(palette: &Palette) -> Option<&Swatch> {
    // A near-black swatch still reports a hue, but it is noise: at that
    // luminance the channel differences are a handful of code values, and no
    // amount of brightening recovers a colour that was never there. Such
    // swatches are skipped so the border comes from a colour the eye can
    // actually see in the image.
    let bright_enough: Vec<&Swatch> = palette
        .swatches
        .iter()
        .filter(|s| s.intensity >= GLOW_MIN_SOURCE_INTENSITY)
        .collect();
    // Falling back to everything rather than returning None: a wallpaper that
    // is genuinely all shadow should still get a border, just a dim one.
    let mut candidates: Vec<&Swatch> = if bright_enough.is_empty() {
        palette.swatches.iter().collect()
    } else {
        bright_enough
    };
    // `swatches` is weight-descending, so the first entry is the background.
    // Guarded: with only one swatch there is nothing else to choose, and a
    // border in the background's own colour is better than none.
    if candidates.len() > 1 {
        candidates.remove(0);
    }
    candidates.into_iter().max_by(|a, b| {
        let score = |s: &Swatch| {
            let (_, chroma) = hue_chroma(crate::palette::abs10k_to_ictcp(s.abs10k));
            chroma * s.weight.sqrt()
        };
        score(a).total_cmp(&score(b))
    })
}

fn accent_swatch(palette: &Palette) -> Option<&Swatch> {
    palette
        .swatches
        .iter()
        .max_by(|a, b| {
            let score = |s: &Swatch| {
                let (_, chroma) = hue_chroma(crate::palette::abs10k_to_ictcp(s.abs10k));
                chroma * s.weight.sqrt()
            };
            score(a).total_cmp(&score(b))
        })
}

/// Reference hues for the terminal palette, as sRGB. Only their hue is used --
/// intensity and chroma are re-solved -- but anchoring to recognisable values
/// is what keeps "red" red once it has been pulled toward the wallpaper.
const ANSI_REFERENCE: [[f32; 3]; 6] = [
    [0.86, 0.20, 0.18], // red
    [0.30, 0.72, 0.31], // green
    [0.90, 0.71, 0.20], // yellow
    [0.26, 0.52, 0.88], // blue
    [0.72, 0.33, 0.82], // magenta
    [0.24, 0.74, 0.78], // cyan
];

/// Solve a full theme from a wallpaper palette.
pub(crate) fn solve(palette: &Palette, params: &ThemeParams) -> Option<Theme> {
    let accent_source = accent_swatch(palette)?;
    let (accent_hue, accent_chroma) =
        hue_chroma(crate::palette::abs10k_to_ictcp(accent_source.abs10k));

    let backdrop = worst_case_backdrop(palette, params);

    // The content background carries the wallpaper's hue at low chroma and low
    // intensity: enough to feel related to the wallpaper, not enough to tint
    // text sitting on it. Intensity is fixed rather than derived -- a
    // background that tracked the wallpaper's own darkness would swing the
    // whole theme's contrast with every slideshow step.
    const BACKGROUND_INTENSITY: f32 = 0.18;
    const SURFACE_INTENSITY: f32 = 0.24;
    // Chroma carried by the background and by text. These are what decide
    // whether the theme reads as "made from this wallpaper" or as a generic
    // dark scheme with one coloured cursor: too low and every wallpaper, from a
    // desert to a night sky, yields the same near-black on near-white.
    //
    // The background can take a lot of tint before it stops reading as
    // neutral, because there is nothing behind it to clash with. Text can take
    // much less -- chroma in a glyph costs legibility that the intensity solve
    // then has to buy back -- but it needs enough to look related to the
    // background rather than dropped on top of it.
    let background_chroma = (accent_chroma * 0.60).min(0.09);
    let background_abs10k =
        crate::palette::ictcp_to_abs10k(from_polar(BACKGROUND_INTENSITY, accent_hue, background_chroma));
    let background_abs10k = [
        background_abs10k[0].max(0.0),
        background_abs10k[1].max(0.0),
        background_abs10k[2].max(0.0),
    ];
    let surface_abs10k =
        crate::palette::ictcp_to_abs10k(from_polar(SURFACE_INTENSITY, accent_hue, background_chroma));
    let surface_abs10k = [
        surface_abs10k[0].max(0.0),
        surface_abs10k[1].max(0.0),
        surface_abs10k[2].max(0.0),
    ];

    // Everything below is solved against the *composite*, not against
    // `background_abs10k` -- that is the whole point of doing this here.
    let effective = effective_background(background_abs10k, backdrop, params.opacity);
    let effective_srgb = abs10k_to_display_srgb(effective, params.sdr_white_nits);

    // Text carries a trace of the wallpaper hue but must not read as coloured.
    let text_chroma = (accent_chroma * 0.35).min(0.045);
    let foreground = solve_intensity(
        accent_hue,
        text_chroma,
        effective_srgb,
        params.target_lc,
        params.sdr_white_nits,
        BACKGROUND_INTENSITY,
    );
    let muted = solve_intensity(
        accent_hue,
        text_chroma,
        effective_srgb,
        LC_SECONDARY,
        params.sdr_white_nits,
        BACKGROUND_INTENSITY,
    );
    let accent = solve_intensity_in_gamut(
        accent_hue,
        accent_chroma.max(0.03),
        effective_srgb,
        LC_SECONDARY,
        params.sdr_white_nits,
        BACKGROUND_INTENSITY,
    );
    let border = solve_intensity(
        accent_hue,
        (accent_chroma * 0.5).min(0.05),
        effective_srgb,
        LC_NONTEXT,
        params.sdr_white_nits,
        BACKGROUND_INTENSITY,
    );

    // Solved against the wallpaper, not the composited window background: the
    // glow is drawn outside the window, so the backdrop is all there is behind
    // it.
    let backdrop_srgb = abs10k_to_display_srgb(backdrop, params.sdr_white_nits);
    // Hue and chroma both come from a real swatch; only the luminance is
    // solved, so the border is a colour lifted out of the wallpaper rather
    // than one derived from it.
    let (glow_hue, glow_source_chroma) = glow_swatch(palette)
        .map(|s| hue_chroma(crate::palette::abs10k_to_ictcp(s.abs10k)))
        .unwrap_or((accent_hue, accent_chroma));
    let (glow_abs10k, _) = solve_vivid(
        glow_hue,
        (glow_source_chroma * GLOW_CHROMA_GAIN).clamp(GLOW_CHROMA_FLOOR, GLOW_CHROMA_CEILING),
        params.sdr_white_nits,
    );
    // Reported for diagnostics only -- nothing is solved against it. A glow
    // sits over the wallpaper, so this says how well it separates from it.
    let glow = ThemeColor {
        abs10k: glow_abs10k,
        lc: apca_lc(
            abs10k_to_display_srgb(glow_abs10k, params.sdr_white_nits),
            backdrop_srgb,
        ),
    };

    let mut ansi = [ThemeColor { abs10k: [0.0; 3], lc: 0.0 }; 6];
    for (slot, reference) in ansi.iter_mut().zip(ANSI_REFERENCE) {
        // The reference is an sRGB triple; route it through the same absolute
        // space as everything else so its hue is measured, not assumed.
        let reference_abs10k = {
            let c = crate::hdr_shaders::srgb_to_bt2020_abs10k(
                smithay::backend::renderer::Color32F::new(
                    reference[0],
                    reference[1],
                    reference[2],
                    1.0,
                ),
                params.sdr_white_nits,
            );
            [c.r(), c.g(), c.b()]
        };
        let (hue, chroma) = hue_chroma(crate::palette::abs10k_to_ictcp(reference_abs10k));
        // Halfway to the wallpaper's chroma: enough to belong to the same
        // family, not so much that green and cyan converge.
        let blended = 0.5 * chroma + 0.5 * accent_chroma.max(0.02);
        // Chroma alone is not enough -- it changes how vivid the palette is,
        // not what colour it is. Without the hue pull the six stay put under
        // every wallpaper.
        let pulled = rotate_toward(hue, accent_hue, ANSI_HUE_PULL);
        *slot = solve_intensity(
            pulled,
            blended,
            effective_srgb,
            LC_SECONDARY,
            params.sdr_white_nits,
            BACKGROUND_INTENSITY,
        );
    }

    let met_targets = foreground.lc >= params.target_lc
        && muted.lc >= LC_SECONDARY
        && accent.lc >= LC_SECONDARY
        && border.lc >= LC_NONTEXT
        && ansi.iter().all(|c| c.lc >= LC_SECONDARY);

    Some(Theme {
        background: ThemeColor { abs10k: background_abs10k, lc: 0.0 },
        surface: ThemeColor { abs10k: surface_abs10k, lc: 0.0 },
        foreground,
        muted,
        accent,
        border,
        // Deliberately absent from `met_targets`: a glow that cannot clear its
        // target against a blazing wallpaper is a cosmetic disappointment, not
        // a readability failure, and folding it in would report a perfectly
        // legible theme as broken.
        glow,
        ansi,
        effective_background: effective,
        met_targets,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::Swatch;

    fn palette_of(swatches: &[([f32; 3], f32)], p50: f32, p95: f32, p99: f32) -> Palette {
        Palette {
            swatches: swatches
                .iter()
                .map(|&(abs10k, weight)| Swatch {
                    abs10k,
                    weight,
                    intensity: crate::palette::abs10k_to_ictcp(abs10k).i,
                })
                .collect(),
            intensity_p50: p50,
            intensity_p95: p95,
            intensity_p99: p99,
        }
    }

    /// A muted blue-grey wallpaper, mostly dark -- the ordinary case.
    fn ordinary_palette() -> Palette {
        palette_of(
            &[
                ([0.0004, 0.0005, 0.0008], 0.55),
                ([0.0030, 0.0038, 0.0055], 0.25),
                ([0.0090, 0.0100, 0.0120], 0.20),
            ],
            0.30,
            0.45,
            0.50,
        )
    }

    #[test]
    fn apca_matches_the_reference_implementation_at_the_extremes() {
        // The two values every APCA implementation is checked against. Getting
        // these right is what says the constants and the polarity split were
        // transcribed correctly.
        let white = [1.0, 1.0, 1.0];
        let black = [0.0, 0.0, 0.0];
        // The reference pair, and note the asymmetry -- APCA is polarity-aware,
        // which is the entire reason it is used here instead of WCAG 2's
        // symmetric ratio. Light-on-dark and dark-on-light are not the same
        // problem, and this theme is the former.
        let white_on_black = apca_lc(white, black);
        let black_on_white = apca_lc(black, white);
        assert!(
            (black_on_white - 106.04).abs() < 0.5,
            "black on white should be Lc ~106.04, got {black_on_white}"
        );
        assert!(
            (white_on_black - 107.88).abs() < 0.5,
            "white on black should be Lc ~107.88 in magnitude, got {white_on_black}"
        );
    }

    #[test]
    fn apca_reports_no_contrast_for_a_colour_on_itself() {
        let grey = [0.5, 0.5, 0.5];
        assert_eq!(apca_lc(grey, grey), 0.0);
    }

    #[test]
    fn the_effective_background_is_the_composite_not_the_content_colour() {
        // 15% bleed-through of a backdrop ten times brighter than the content
        // background more than doubles what text actually sits on. A theme
        // solved against the content colour alone would be wrong by that much.
        let content = [0.001, 0.001, 0.001];
        let backdrop = [0.010, 0.010, 0.010];
        let effective = effective_background(content, backdrop, 0.85);
        assert!((effective[0] - 0.00235).abs() < 1e-6, "got {effective:?}");
    }

    #[test]
    fn body_text_clears_its_contrast_target() {
        let theme = solve(&ordinary_palette(), &ThemeParams::default()).expect("solvable");
        assert!(
            theme.foreground.lc >= LC_BODY,
            "foreground Lc {} missed the {LC_BODY} target",
            theme.foreground.lc
        );
        assert!(theme.met_targets, "an ordinary dark wallpaper should be fully solvable");
    }

    #[test]
    fn the_solve_returns_the_dimmest_colour_that_clears_the_target() {
        // Not merely "bright enough" -- overshooting would make every theme
        // near-white and throw away the wallpaper's character.
        let theme = solve(&ordinary_palette(), &ThemeParams::default()).expect("solvable");
        assert!(
            theme.foreground.lc < LC_BODY + 2.0,
            "foreground overshot to Lc {}, expected to land just above {LC_BODY}",
            theme.foreground.lc
        );
    }

    #[test]
    fn more_bleed_through_forces_brighter_text() {
        // The load-bearing behaviour: a more transparent window shows more
        // wallpaper, so text must brighten to stay readable. If this does not
        // hold, the solve is not actually reading the composite.
        let palette = ordinary_palette();
        let opaque = ThemeParams { opacity: 0.95, ..ThemeParams::default() };
        let sheer = ThemeParams { opacity: 0.60, ..ThemeParams::default() };
        let a = solve(&palette, &opaque).unwrap();
        let b = solve(&palette, &sheer).unwrap();
        let luminance = |c: &ThemeColor| c.abs10k[1];
        assert!(
            luminance(&b.foreground) > luminance(&a.foreground),
            "sheer window foreground {:?} should outshine opaque {:?}",
            b.foreground.abs10k,
            a.foreground.abs10k
        );
    }

    #[test]
    fn an_uncapped_sharp_backdrop_is_treated_as_worse_than_a_capped_blurred_one() {
        // This is exactly the active-versus-inactive difference in the live
        // config: inactive windows show more wallpaper, unblurred and
        // uncapped, so they bind the theme harder than active ones.
        let palette = ordinary_palette();
        let active = ThemeParams {
            opacity: 0.85,
            backdrop_cap_nits: Some(100.0),
            backdrop_blurred: true,
            ..ThemeParams::default()
        };
        let inactive = ThemeParams {
            opacity: 0.75,
            backdrop_cap_nits: None,
            backdrop_blurred: false,
            ..ThemeParams::default()
        };
        let a = solve(&palette, &active).unwrap();
        let b = solve(&palette, &inactive).unwrap();
        assert!(
            b.effective_background[1] > a.effective_background[1],
            "inactive composite {:?} should be brighter than active {:?}",
            b.effective_background,
            a.effective_background
        );
    }

    #[test]
    fn a_blurred_backdrop_uses_p95_and_a_sharp_one_uses_p99() {
        // Pins the choice of statistic to the compositing path, rather than
        // leaving it as an unexplained constant.
        let palette = palette_of(&[([0.001, 0.001, 0.001], 1.0)], 0.30, 0.40, 0.80);
        let blurred = ThemeParams { backdrop_blurred: true, backdrop_cap_nits: None, ..ThemeParams::default() };
        let sharp = ThemeParams { backdrop_blurred: false, backdrop_cap_nits: None, ..ThemeParams::default() };
        let a = solve(&palette, &blurred).unwrap();
        let b = solve(&palette, &sharp).unwrap();
        assert!(
            b.effective_background[1] > a.effective_background[1] * 2.0,
            "the p99 tail should dominate: sharp {:?} vs blurred {:?}",
            b.effective_background,
            a.effective_background
        );
    }

    #[test]
    fn an_unreadably_bright_wallpaper_reports_failure_rather_than_pretending() {
        // A wallpaper at full PQ peak with a nearly transparent window: no
        // colour in range is readable. The theme must say so, because silently
        // returning white would look like success.
        let palette = palette_of(&[([0.9, 0.9, 0.9], 1.0)], 0.99, 1.0, 1.0);
        let params = ThemeParams {
            opacity: 0.1,
            backdrop_cap_nits: None,
            backdrop_blurred: false,
            ..ThemeParams::default()
        };
        let theme = solve(&palette, &params).expect("still returns a theme");
        assert!(!theme.met_targets, "should report that the targets were missed");
    }

    #[test]
    fn the_accent_favours_a_vivid_colour_over_the_dominant_neutral() {
        // The most common colour in a wallpaper is usually a desaturated
        // background. Picking it as the accent is the classic failure that
        // makes generated themes look grey.
        let palette = palette_of(
            &[
                ([0.0010, 0.0010, 0.0011], 0.80), // dominant near-neutral
                ([0.0090, 0.0012, 0.0015], 0.20), // vivid red minority
            ],
            0.30,
            0.45,
            0.50,
        );
        let theme = solve(&palette, &ThemeParams::default()).expect("solvable");
        assert!(
            theme.accent.abs10k[0] > theme.accent.abs10k[2] * 1.5,
            "accent {:?} should keep the red character of the minority swatch",
            theme.accent.abs10k
        );
    }

    #[test]
    fn the_border_colour_is_one_the_wallpaper_actually_contains() {
        // A synthesized complement stands out but reads as an unrelated colour
        // laid on top, because nothing else on screen is that hue. The border
        // has to be a colour lifted out of the image.
        use std::f32::consts::{PI, TAU};
        let palette = ordinary_palette();
        let theme = solve(&palette, &ThemeParams::default()).expect("solvable");
        let glow_hue = hue_chroma(crate::palette::abs10k_to_ictcp(theme.glow.abs10k)).0;

        let angle_between = |a: f32, b: f32| {
            let mut d = (a - b) % TAU;
            if d > PI {
                d -= TAU;
            } else if d < -PI {
                d += TAU;
            }
            d.abs()
        };

        let nearest = palette
            .swatches
            .iter()
            .map(|s| angle_between(glow_hue, hue_chroma(crate::palette::abs10k_to_ictcp(s.abs10k)).0))
            .fold(f32::MAX, f32::min);
        assert!(
            nearest < 0.15,
            "glow hue matches no swatch in the wallpaper; nearest is {nearest} rad away"
        );
    }

    #[test]
    fn the_border_is_never_sourced_from_a_near_black_swatch() {
        // A dark swatch reports a hue, but at that luminance it is rounding
        // error, and brightening a black colour leaves it black -- seven of
        // ninety-seven real wallpapers produced a #0b0000 border this way.
        let mostly_shadow = palette_of(
            &[
                ([0.00004, 0.00002, 0.00002], 0.62), // near-black, faint red cast
                ([0.00008, 0.00003, 0.00003], 0.24), // also near-black
                ([0.0070, 0.0052, 0.0016], 0.14),    // the one colour actually visible
            ],
            0.20,
            0.40,
            0.46,
        );
        let theme = solve(&mostly_shadow, &ThemeParams::default()).expect("solvable");
        let srgb = abs10k_to_display_srgb(theme.glow.abs10k, 203.0);
        let brightest = srgb.iter().copied().fold(0.0f32, f32::max);
        assert!(
            brightest > 0.5,
            "border came out at {srgb:?}; no luminance setting rescues a black colour"
        );
    }

    #[test]
    fn a_neon_wallpaper_does_not_clip_the_accent_to_a_primary() {
        // Pure reds and cyans put the accent's chroma outside the sRGB gamut.
        // Solving intensity against it drives two channels past 1.0 and one
        // below 0, and the clamp turns the hue into a screaming primary --
        // one real wallpaper produced #ff00ff.
        let neon = palette_of(
            &[
                ([0.0004, 0.0006, 0.0016], 0.36),
                ([0.0113, 0.0000, 0.0004], 0.30), // pure red
                ([0.0000, 0.0117, 0.0139], 0.34), // pure cyan
            ],
            0.30,
            0.50,
            0.60,
        );
        let theme = solve(&neon, &ThemeParams::default()).expect("solvable");
        let raw = abs10k_to_display_srgb_unclamped(theme.accent.abs10k, 203.0);
        assert!(
            raw.iter().all(|&c| (-0.01..=1.01).contains(&c)),
            "accent {raw:?} sits outside the gamut and will clamp to a primary"
        );
    }

    #[test]
    fn the_border_avoids_the_wallpapers_own_background_colour() {
        // The most populous swatch is the wallpaper's background. A border in
        // that colour cannot stand out, because it is the thing behind it.
        use std::f32::consts::{PI, TAU};
        let palette = palette_of(
            &[
                ([0.0010, 0.0010, 0.0011], 0.80), // dominant near-neutral
                ([0.0090, 0.0012, 0.0015], 0.20), // vivid red minority
            ],
            0.30,
            0.45,
            0.50,
        );
        let theme = solve(&palette, &ThemeParams::default()).expect("solvable");
        let glow_hue = hue_chroma(crate::palette::abs10k_to_ictcp(theme.glow.abs10k)).0;
        let dominant_hue =
            hue_chroma(crate::palette::abs10k_to_ictcp(palette.swatches[0].abs10k)).0;
        let mut delta = (glow_hue - dominant_hue) % TAU;
        if delta > PI {
            delta -= TAU;
        } else if delta < -PI {
            delta += TAU;
        }
        assert!(
            delta.abs() > 0.2,
            "border took the wallpaper's own background hue; it cannot stand out against it"
        );
    }

    #[test]
    fn the_two_border_roles_stay_tellable_apart() {
        // Both borders are themed now, so focus is marked by the difference
        // between them rather than by one being coloured and one not. If the
        // glow and the border role converge, focus becomes invisible on any
        // window whose glow margin is zero.
        use std::f32::consts::{PI, TAU};
        let theme = solve(&ordinary_palette(), &ThemeParams::default()).expect("solvable");
        let (glow_hue, glow_chroma) =
            hue_chroma(crate::palette::abs10k_to_ictcp(theme.glow.abs10k));
        let (border_hue, border_chroma) =
            hue_chroma(crate::palette::abs10k_to_ictcp(theme.border.abs10k));

        let mut delta = (glow_hue - border_hue) % TAU;
        if delta > PI {
            delta -= TAU;
        } else if delta < -PI {
            delta += TAU;
        }
        // Either a clear hue separation or a clear chroma one will do: what
        // must not happen is the two being close on both counts.
        let separated_by_hue = delta.abs() > 1.5;
        let separated_by_chroma = glow_chroma > border_chroma * 2.0;
        assert!(
            separated_by_hue || separated_by_chroma,
            "focused and unfocused borders are too alike: hue delta {} rad, chroma {} vs {}",
            delta.abs(),
            glow_chroma,
            border_chroma
        );
    }

    #[test]
    fn the_glow_keeps_its_colour_instead_of_clipping_to_white() {
        // The first implementation solved the glow for contrast the way text is
        // solved, which drove intensity until every channel clipped and the
        // hue washed out. Four of six real wallpapers produced pure white.
        // A glow gets its separation from being drawn at HDR luminance, so what
        // matters here is that it stays a colour at all.
        for step in 0..24 {
            let hue = -std::f32::consts::PI
                + std::f32::consts::TAU * (step as f32) / 24.0;
            let (abs10k, chroma) = solve_vivid(hue, 0.12, 203.0);
            assert!(chroma > 0.0, "hue {hue} fell back to neutral");
            let srgb = abs10k_to_display_srgb(abs10k, 203.0);
            let max = srgb.iter().copied().fold(0.0f32, f32::max);
            let min = srgb.iter().copied().fold(1.0f32, f32::min);
            assert!(
                max - min > 0.15,
                "hue {hue} produced a near-neutral {srgb:?}; the glow has no colour"
            );
            assert!(
                min < 0.97,
                "hue {hue} clipped to white: {srgb:?}"
            );
        }
    }

    #[test]
    fn a_near_monochrome_wallpaper_still_gets_a_coloured_glow() {
        // Scaling the accent's chroma alone yields a grey glow when the
        // wallpaper has almost none of its own, which is exactly when a border
        // most needs to be visible.
        let grey = palette_of(
            &[([0.0012, 0.0012, 0.0012], 0.7), ([0.0040, 0.0041, 0.0040], 0.3)],
            0.30,
            0.42,
            0.46,
        );
        let theme = solve(&grey, &ThemeParams::default()).expect("solvable");
        let srgb = abs10k_to_display_srgb(theme.glow.abs10k, 203.0);
        let max = srgb.iter().copied().fold(0.0f32, f32::max);
        let min = srgb.iter().copied().fold(1.0f32, f32::min);
        assert!(
            max - min > 0.10,
            "glow {srgb:?} is grey on a grey wallpaper; the chroma floor did not hold"
        );
    }

    #[test]
    fn terminal_hues_stay_near_their_references_under_every_accent() {
        // Sampling a handful of wallpapers hid this: a cool accent dragged red
        // into magenta and landed it on the magenta slot exactly. Sweeping the
        // full circle is what catches the worst accent rather than an average
        // one.
        use std::f32::consts::{PI, TAU};
        let references: Vec<f32> = ANSI_REFERENCE
            .iter()
            .map(|&rgb| {
                let c = crate::hdr_shaders::srgb_to_bt2020_abs10k(
                    smithay::backend::renderer::Color32F::new(rgb[0], rgb[1], rgb[2], 1.0),
                    203.0,
                );
                hue_chroma(crate::palette::abs10k_to_ictcp([c.r(), c.g(), c.b()])).0
            })
            .collect();

        let separation = |a: f32, b: f32| {
            let mut d = (a - b) % TAU;
            if d > PI {
                d -= TAU;
            } else if d < -PI {
                d += TAU;
            }
            d.abs()
        };

        for step in 0..72 {
            let accent = -PI + TAU * (step as f32) / 72.0;
            let pulled: Vec<f32> = references
                .iter()
                .map(|&h| rotate_toward(h, accent, ANSI_HUE_PULL))
                .collect();

            for (index, (&moved, &original)) in pulled.iter().zip(&references).enumerate() {
                assert!(
                    separation(moved, original) <= ANSI_HUE_PULL_CEILING + 1e-4,
                    "slot {index} moved {} rad at accent {accent}, past the ceiling",
                    separation(moved, original)
                );
            }
            // No two slots may converge: the six have to stay tellable apart
            // for syntax highlighting to mean anything.
            for i in 0..pulled.len() {
                for j in (i + 1)..pulled.len() {
                    let before = separation(references[i], references[j]);
                    let after = separation(pulled[i], pulled[j]);
                    assert!(
                        after > before * 0.5,
                        "slots {i} and {j} converged from {before} to {after} rad at accent {accent}"
                    );
                }
            }
        }
    }

    #[test]
    fn terminal_colours_keep_their_identities() {
        // Pulling the ANSI set toward the wallpaper must not collapse it:
        // red has to stay redder than blue, and green greener than red, or a
        // syntax-highlighted buffer becomes unreadable.
        let theme = solve(&ordinary_palette(), &ThemeParams::default()).expect("solvable");
        let [red, green, _yellow, blue, _magenta, _cyan] = theme.ansi;
        assert!(
            red.abs10k[0] > blue.abs10k[0],
            "red {:?} should carry more red than blue {:?}",
            red.abs10k,
            blue.abs10k
        );
        assert!(
            green.abs10k[1] > red.abs10k[1],
            "green {:?} should carry more green than red {:?}",
            green.abs10k,
            red.abs10k
        );
        assert!(
            blue.abs10k[2] > green.abs10k[2],
            "blue {:?} should carry more blue than green {:?}",
            blue.abs10k,
            green.abs10k
        );
    }
}
