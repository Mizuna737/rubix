//! Window border rendering (HDR Phase 5b).
//!
//! Borders are drawn **outside** the client rect: a window's tile keeps its
//! full size and the border ring is laid over the surrounding gap. That is a
//! deliberate trade -- the alternative (shrinking each tile by the border
//! width) puts layout geometry and decoration in the same calculation, and the
//! model owns layout. The visible consequence is that borders eat into
//! `inner_gap`/`outer_gap`: at `inner_gap = 0` adjacent borders overlap, and
//! at `outer_gap = 0` the outermost ring clips at the screen edge. Both are
//! consequences of the choice, not bugs; raise the gaps to give borders room.
//!
//! Fullscreen windows never get a border. That is not cosmetic: an element
//! stacked above a fullscreen window disqualifies it from direct primary-plane
//! scanout, which is exactly the path the HDR gaming work exists to keep. A
//! fullscreen window also has no gap to draw into.
//!
//! ## Luminance
//!
//! On an HDR output the compositor can author chrome *brighter than SDR white*
//! -- a focus ring at 350 nits against a 200-nit desktop is a focus indication
//! that SDR chrome physically cannot express. That is what `luminance_nits`
//! buys, and it is per-rule so a rule can shout (an urgent window) or whisper.
//!
//! Everything here degrades to plain sRGB when it has to. `luminance_nits` is
//! consulted **only** when the output is actually compositing through the HDR
//! path; on an SDR output, on SDR hardware, or with `hdr = false`, the border
//! is its configured color and nothing else happens. A user with no HDR
//! monitor sees ordinary borders and pays nothing.

use std::cell::RefCell;
use std::collections::HashMap;

use smithay::backend::renderer::element::solid::{SolidColorBuffer, SolidColorRenderElement};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::Color32F;
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, Scale};

use crate::config::BorderStyle;
use crate::RubixState;

thread_local! {
    // One `SolidColorBuffer` per (window id, ring side), kept across frames.
    //
    // Not an optimization -- a correctness requirement. A render element's id
    // must be stable frame to frame or the damage tracker sees a brand-new
    // element every frame and repaints the whole output; and its commit
    // counter must advance when the element's appearance changes or the
    // tracker sees no damage and leaves a stale border on screen (the visible
    // symptom would be a focus ring that never moves). `SolidColorBuffer` owns
    // both: a stable `Id` for its lifetime, and an `update` that increments the
    // commit counter only when size or color actually changed.
    //
    // Thread-local because the compositor's event loop is single-threaded --
    // the same reasoning (and the same shape) as `cursor.rs`'s CURSOR_CACHE.
    // Pruned against live window ids on each call so closed windows don't
    // accumulate.
    static BORDER_BUFFERS: RefCell<HashMap<(u32, usize), SolidColorBuffer>> =
        RefCell::new(HashMap::new());
}

/// The four rects forming a border ring around `inner`, expanded outward by
/// `width`. Corners belong to the top and bottom bars, so the four rects
/// tile the ring exactly once -- no overlap, which matters because a
/// translucent border would otherwise double-blend at the corners.
///
/// Returns an empty vec for a non-positive width so callers can stay
/// branch-free.
pub(crate) fn ring_rects(
    inner: Rectangle<i32, Logical>,
    width: i32,
) -> Vec<Rectangle<i32, Logical>> {
    if width <= 0 {
        return Vec::new();
    }
    let (x, y) = (inner.loc.x, inner.loc.y);
    let (w, h) = (inner.size.w, inner.size.h);
    // Full-width top and bottom (they own the corners); left and right span
    // only the inner height between them.
    vec![
        Rectangle::new((x - width, y - width).into(), (w + 2 * width, width).into()),
        Rectangle::new((x - width, y + h).into(), (w + 2 * width, width).into()),
        Rectangle::new((x - width, y).into(), (width, h).into()),
        Rectangle::new((x + w, y).into(), (width, h).into()),
    ]
}

/// Rescale an sRGB-encoded color so its linear luminance is multiplied by
/// `ratio`, returning a new sRGB-encoded color.
///
/// This exists to defeat a pass-wide transform. In the HDR composite,
/// `hdr_shaders::sdr_solid_transform` is installed via
/// `set_solid_color_transform` for the whole SDR decode pass, so *every*
/// solid color -- clear color, borders, everything -- is linearized and
/// scaled to `sdr_white_nits`. There is no per-element hook. To land a border
/// at some other absolute luminance, we pre-compensate here by exactly the
/// factor that transform will later apply, so the two cancel.
///
/// Encoded values above 1.0 are produced and expected whenever `ratio > 1`
/// (that is the entire point). Nothing clamps them: the transform is a CPU
/// closure whose sRGB EOTF is an ordinary `powf` that extends fine past 1.0,
/// and its output is scaled by `nits / 10000` -- so a 350-nit border leaves
/// the transform at 0.035, nowhere near a ceiling.
pub(crate) fn scale_srgb_luminance(color: Color32F, ratio: f32) -> Color32F {
    fn to_linear(c: f32) -> f32 {
        if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    }
    fn from_linear(c: f32) -> f32 {
        if c <= 0.0031308 { c * 12.92 } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 }
    }
    if ratio == 1.0 {
        return color;
    }
    Color32F::new(
        from_linear(to_linear(color.r()) * ratio),
        from_linear(to_linear(color.g()) * ratio),
        from_linear(to_linear(color.b()) * ratio),
        color.a(),
    )
}

/// The color to actually hand the renderer for `style` on this output.
///
/// `sdr_white_nits` is the live value the HDR encode pass is using this frame.
/// When the output is not compositing in HDR, or the style asks for no
/// specific luminance, the configured color passes through untouched -- the
/// SDR path is byte-for-byte what it would be with no HDR support at all.
pub(crate) fn resolved_color(style: &BorderStyle, hdr: bool, sdr_white_nits: f32) -> Color32F {
    match style.luminance_nits {
        Some(nits) if hdr && sdr_white_nits > 0.0 => {
            scale_srgb_luminance(style.color, nits / sdr_white_nits)
        }
        _ => style.color,
    }
}

/// Border elements for every window on `output`, front-to-back, positioned
/// relative to the output region exactly as `Space::render_elements_for_region`
/// positions the windows they surround.
///
/// `hdr` says whether this output is compositing through the linear HDR path
/// this frame -- the winit backend and every SDR output pass `false`, which
/// makes the whole luminance mechanism inert.
pub(crate) fn border_elements(
    state: &RubixState,
    output: &Output,
    scale: f64,
    hdr: bool,
) -> Vec<SolidColorRenderElement> {
    let deco = &state.config.decoration;
    if deco.border_width == 0 {
        return Vec::new();
    }
    let Some(region) = state.space.output_geometry(output) else {
        return Vec::new();
    };
    let focused = state.focused_window_id();
    let width = deco.border_width as i32;

    let mut elements = Vec::new();
    for (id, window) in state.windows.iter() {
        // See the module docs: a border over a fullscreen window would cost
        // direct scanout, and there is no gap to draw it in anyway.
        if state.fullscreen_windows.contains(id) {
            continue;
        }
        let Some(geo) = state.space.element_geometry(window) else { continue };
        if !region.overlaps(geo) {
            continue;
        }
        let (app_id, title) = state.window_identity(*id);
        let style = deco.style_for(app_id.as_deref(), title.as_deref(), focused == Some(*id));
        if style.color.a() <= 0.0 {
            continue;
        }
        let color = resolved_color(&style, hdr, state.sdr_white_nits);
        for (index, rect) in ring_rects(geo, width).into_iter().enumerate() {
            // Translate into region-local space the same way the space's own
            // element positioning does, then round to physical once.
            let loc: Point<i32, Logical> = rect.loc - region.loc;
            BORDER_BUFFERS.with_borrow_mut(|buffers| {
                let buffer = buffers.entry((*id, index)).or_default();
                buffer.update(rect.size, color);
                elements.push(SolidColorRenderElement::from_buffer(
                    buffer,
                    loc.to_physical_precise_round::<f64, i32>(scale),
                    Scale::from(scale),
                    1.0,
                    Kind::Unspecified,
                ));
            });
        }
    }

    BORDER_BUFFERS.with_borrow_mut(|buffers| {
        buffers.retain(|(id, _), _| state.windows.contains_key(id));
    });
    elements
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Logical> {
        Rectangle::new((x, y).into(), (w, h).into())
    }

    #[test]
    fn ring_is_empty_for_zero_width() {
        assert!(ring_rects(rect(0, 0, 100, 100), 0).is_empty());
        assert!(ring_rects(rect(0, 0, 100, 100), -1).is_empty());
    }

    #[test]
    fn ring_lies_entirely_outside_the_inner_rect() {
        let inner = rect(10, 20, 100, 50);
        for r in ring_rects(inner, 3) {
            assert!(!r.overlaps(inner), "{r:?} overlaps the client rect");
        }
    }

    #[test]
    fn ring_rects_do_not_overlap_each_other() {
        let ring = ring_rects(rect(10, 20, 100, 50), 4);
        for (i, a) in ring.iter().enumerate() {
            for b in ring.iter().skip(i + 1) {
                assert!(!a.overlaps(*b), "{a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn ring_area_equals_the_frame_it_should_cover() {
        let (w, h, bw) = (100, 50, 4);
        let ring = ring_rects(rect(10, 20, w, h), bw);
        let area: i32 = ring.iter().map(|r| r.size.w * r.size.h).sum();
        // (outer area) - (inner area), with no double-counted corners.
        let expected = (w + 2 * bw) * (h + 2 * bw) - w * h;
        assert_eq!(area, expected);
    }

    #[test]
    fn ring_expands_the_bounding_box_by_exactly_the_border_width() {
        let inner = rect(10, 20, 100, 50);
        let ring = ring_rects(inner, 3);
        let min_x = ring.iter().map(|r| r.loc.x).min().unwrap();
        let min_y = ring.iter().map(|r| r.loc.y).min().unwrap();
        let max_x = ring.iter().map(|r| r.loc.x + r.size.w).max().unwrap();
        let max_y = ring.iter().map(|r| r.loc.y + r.size.h).max().unwrap();
        assert_eq!((min_x, min_y), (7, 17));
        assert_eq!((max_x, max_y), (113, 73));
    }

    #[test]
    fn luminance_scaling_is_identity_at_ratio_one() {
        let c = Color32F::new(0.5, 0.25, 0.75, 1.0);
        let out = scale_srgb_luminance(c, 1.0);
        assert_eq!((out.r(), out.g(), out.b(), out.a()), (0.5, 0.25, 0.75, 1.0));
    }

    #[test]
    fn luminance_scaling_preserves_alpha_and_brightens_monotonically() {
        let c = Color32F::new(0.5, 0.5, 0.5, 0.8);
        let up = scale_srgb_luminance(c, 1.75);
        assert!((up.a() - 0.8).abs() < 1e-6, "alpha must not be touched");
        assert!(up.r() > c.r(), "ratio > 1 must brighten");
        let down = scale_srgb_luminance(c, 0.5);
        assert!(down.r() < c.r(), "ratio < 1 must darken");
    }

    /// The whole point of the pre-compensation: pushing a color through
    /// `scale_srgb_luminance` and then through the pass-wide solid transform
    /// must land at the requested absolute luminance, not at `sdr_white_nits`.
    #[test]
    fn precompensation_cancels_the_pass_wide_solid_transform() {
        let sdr_white = 200.0_f32;
        let target = 350.0_f32;
        let base = Color32F::new(1.0, 1.0, 1.0, 1.0);

        // The exact closure `udev` installs via `set_solid_color_transform`,
        // so this tests the real pass behaviour rather than a restatement of it.
        let transform = crate::hdr_shaders::sdr_solid_transform(sdr_white);
        let plain = transform(base);
        let boosted = transform(scale_srgb_luminance(base, target / sdr_white));

        // White at 200 nits lands at 200/10000; the same white pre-scaled for
        // 350 nits must land at 350/10000 -- i.e. exactly target/sdr_white
        // times as bright, in the linear working space.
        let ratio = boosted.r() / plain.r();
        assert!(
            (ratio - target / sdr_white).abs() < 1e-3,
            "expected {}x, got {ratio}x",
            target / sdr_white
        );
    }

    #[test]
    fn luminance_is_ignored_when_the_output_is_not_hdr() {
        let style = BorderStyle {
            color: Color32F::new(0.4, 0.6, 0.9, 1.0),
            luminance_nits: Some(600.0),
        };
        let out = resolved_color(&style, false, 200.0);
        assert_eq!((out.r(), out.g(), out.b()), (0.4, 0.6, 0.9));
    }

    #[test]
    fn luminance_applies_when_the_output_is_hdr() {
        let style = BorderStyle {
            color: Color32F::new(0.4, 0.6, 0.9, 1.0),
            luminance_nits: Some(600.0),
        };
        let out = resolved_color(&style, true, 200.0);
        assert!(out.r() > 0.4, "an HDR output should brighten the border");
    }

    #[test]
    fn a_style_without_luminance_passes_its_color_through_on_hdr() {
        let style = BorderStyle {
            color: Color32F::new(0.4, 0.6, 0.9, 1.0),
            luminance_nits: None,
        };
        let out = resolved_color(&style, true, 200.0);
        assert_eq!((out.r(), out.g(), out.b()), (0.4, 0.6, 0.9));
    }
}
