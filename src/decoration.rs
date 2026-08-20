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

use smithay::backend::renderer::element::{
    Element, Id, Kind, RenderElement, UnderlyingStorage,
};
use smithay::backend::renderer::gles::element::PixelShaderElement;
use smithay::backend::renderer::gles::{
    GlesPixelProgram, GlesRenderer, Uniform, UniformName, UniformType,
};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::backend::renderer::{Color32F, Renderer};
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{
    Buffer as BufferCoords, Logical, Physical, Point, Rectangle, Scale, Transform,
};

use crate::config::BorderStyle;
use crate::rounding::GlesAccess;
use crate::RubixState;

/// Fragment shader for the border ring and its glow.
///
/// One rounded-rectangle SDF drives both: the ring is the band between the
/// window's rounded edge and that edge inflated by the border width, and the
/// glow is an outward decay from the ring's outer boundary. Evaluating both
/// from the same distance field is what keeps the glow concentric with the
/// corner curve instead of pooling at the corners.
///
/// Colour arrives **already in the destination's colour space** -- sRGB-encoded
/// for an SDR framebuffer, linear BT.2020 at absolute nits for the HDR
/// working space. That is the whole reason this shader can express HDR chrome
/// at all: a pixel shader bypasses both the solid-colour transform and the
/// texture decode, so what we hand it is what gets written. It also means the
/// luminance pre-compensation the four-rect version needed is gone.
///
/// Where an effect library plugs in later: everything below `ringAlpha` is a
/// pure function of distance, so a future `pulse`/`breathing`/`snake` variant
/// only has to modulate `a` (and take a time uniform) without touching the
/// geometry. Deliberately not abstracted yet -- there is one effect.
///
/// No `#version` directive: smithay's pixel-shader path interprets these as
/// GLSL 100 and prepends its own defines.
const BORDER_RING: &str = r#"
precision highp float;

varying vec2 v_coords;
uniform vec2 size;
uniform float alpha;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

uniform vec4 rubix_color;
uniform float rubix_radius;
uniform float rubix_border;
uniform float rubix_glow;
uniform float rubix_falloff;

float roundRectSdf(vec2 p, vec2 halfSize, float r) {
    vec2 q = abs(p) - (halfSize - vec2(r));
    return length(max(q, vec2(0.0))) + min(max(q.x, q.y), 0.0) - r;
}

// How far the ring's inner edge is pushed underneath the window, in pixels.
//
// Without it the corners fringe. The window's own rounded mask leaves partial
// alpha along the curve, and the ring's inner edge ramps up from zero over the
// same pixels; two partial coverages composited over the background do not add
// to one, so the wallpaper shows through as a thin seam. The ring draws above
// its window, so overlapping inward puts fully-opaque ring exactly where the
// window's antialiasing is weakest, and the seam closes.
#define RING_INNER_OVERLAP 1.0

// Coverage at one point, in element-centred pixel coordinates.
float ringCoverage(vec2 centred, vec2 winHalf, float r) {
    float dInner = roundRectSdf(centred, winHalf, r);
    float dOuter = roundRectSdf(centred, winHalf + vec2(rubix_border), r + rubix_border);

    float ring = (1.0 - smoothstep(-0.5, 0.5, dOuter))
        * smoothstep(-0.5, 0.5, dInner + RING_INNER_OVERLAP);

    float glow = 0.0;
    if (rubix_glow > 0.0) {
        float t = clamp(dOuter / rubix_glow, 0.0, 1.0);
        // Gated by a smoothstep rather than step(): a hard cut here is a
        // discontinuity right where the glow is brightest.
        glow = pow(1.0 - t, rubix_falloff) * smoothstep(-0.5, 0.5, dOuter);
    }

    // Union, not max(). max() creases where the two curves cross -- the
    // derivative jumps -- and because the crossing region widens with
    // curvature, that crease is worst exactly at the corners. Compositing the
    // glow under the ring is continuous everywhere.
    return ring + glow * (1.0 - ring);
}

// Cheap hash, used to dither the final alpha. The glow is a long smooth ramp
// from a high absolute luminance down to nothing, which is precisely the shape
// that contours into visible bands; a fraction of a step of noise breaks the
// bands up without being visible as grain.
float dither(vec2 p) {
    return fract(sin(dot(p, vec2(12.9898, 78.233))) * 43758.5453) - 0.5;
}

void main() {
    // The element covers the window inflated by border + glow, so the window
    // itself is centred within it.
    vec2 centred = v_coords * size - size * 0.5;
    vec2 winHalf = size * 0.5 - vec2(rubix_border + rubix_glow);

    // A radius larger than the window's own half-extent makes the distance
    // field fold in on itself and the corners break up. Clamping matches what
    // a capsule would do, which is the sane reading of "rounder than possible".
    float r = min(rubix_radius, min(winHalf.x, winHalf.y));

    // 2x2 supersample. The analytic smoothstep assumes the distance field's
    // gradient has unit magnitude, which holds along the straight edges but not
    // through a corner arc, so a single sample antialiases the corners more
    // harshly than the sides. Four sub-pixel samples cost three extra distance
    // evaluations on a thin ring and even that out.
    float a = 0.0;
    a += ringCoverage(centred + vec2(-0.25, -0.25), winHalf, r);
    a += ringCoverage(centred + vec2(0.25, -0.25), winHalf, r);
    a += ringCoverage(centred + vec2(-0.25, 0.25), winHalf, r);
    a += ringCoverage(centred + vec2(0.25, 0.25), winHalf, r);
    a *= 0.25;

    a = clamp(a + dither(v_coords * size) * (1.0 / 512.0), 0.0, 1.0);

    // Premultiplied, which is what the renderer blends for.
    gl_FragColor = vec4(rubix_color.rgb * rubix_color.a * a, rubix_color.a * a) * alpha;
}
"#;

thread_local! {
    // Compiled once, same single-GL-context assumption as rounding.rs's cache.
    // A failure is remembered rather than retried so a broken shader warns once
    // instead of every frame forever; windows simply go unbordered.
    static RING_PROGRAM: RefCell<Option<Option<GlesPixelProgram>>> = const { RefCell::new(None) };
}

fn ring_program(renderer: &mut GlesRenderer) -> Option<GlesPixelProgram> {
    RING_PROGRAM.with_borrow_mut(|cache| {
        cache
            .get_or_insert_with(|| {
                match renderer.compile_custom_pixel_shader(
                    BORDER_RING,
                    &[
                        UniformName::new("rubix_color", UniformType::_4f),
                        UniformName::new("rubix_radius", UniformType::_1f),
                        UniformName::new("rubix_border", UniformType::_1f),
                        UniformName::new("rubix_glow", UniformType::_1f),
                        UniformName::new("rubix_falloff", UniformType::_1f),
                    ],
                ) {
                    Ok(program) => Some(program),
                    Err(e) => {
                        tracing::warn!("border shader compile failed, borders disabled: {e:?}");
                        None
                    }
                }
            })
            .clone()
    })
}

/// Wraps a `PixelShaderElement`, which smithay only implements `RenderElement`
/// for against `GlesRenderer`. The wrapper routes the draw through
/// [`GlesAccess`] so it works on udev's `MultiRenderer` too.
pub(crate) struct BorderRingElement {
    inner: PixelShaderElement,
}

impl Element for BorderRingElement {
    fn id(&self) -> &Id {
        self.inner.id()
    }
    fn current_commit(&self) -> CommitCounter {
        self.inner.current_commit()
    }
    fn location(&self, scale: Scale<f64>) -> Point<i32, Physical> {
        self.inner.location(scale)
    }
    fn src(&self) -> Rectangle<f64, BufferCoords> {
        self.inner.src()
    }
    fn transform(&self) -> Transform {
        self.inner.transform()
    }
    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.inner.geometry(scale)
    }
    fn damage_since(&self, scale: Scale<f64>, commit: Option<CommitCounter>) -> DamageSet<i32, Physical> {
        self.inner.damage_since(scale, commit)
    }
    fn alpha(&self) -> f32 {
        self.inner.alpha()
    }
    fn kind(&self) -> Kind {
        self.inner.kind()
    }
    /// A ring is mostly transparent -- it is a band and a glow over a hole.
    fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        OpaqueRegions::default()
    }
}

impl<R> RenderElement<R> for BorderRingElement
where
    R: Renderer + GlesAccess,
{
    fn draw(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, BufferCoords>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), R::Error> {
        R::draw_pixel_shader(frame, &self.inner, src, dst, damage);
        Ok(())
    }

    fn underlying_storage(&self, _renderer: &mut R) -> Option<UnderlyingStorage<'_>> {
        None
    }
}

/// The colour to hand the ring shader, already in the destination's space.
///
/// On an HDR output this is linear BT.2020 scaled to absolute nits, so a rule
/// asking for 350 nits gets exactly 350 nits. Everywhere else it is the
/// configured sRGB colour, untouched -- `luminance_nits` is not consulted at
/// all, so a machine with no HDR display behaves as though none of this exists.
pub(crate) fn resolved_color(style: &BorderStyle, hdr: bool, sdr_white_nits: f32) -> Color32F {
    if !hdr {
        return style.color;
    }
    let nits = style.luminance_nits.unwrap_or(sdr_white_nits);
    crate::hdr_shaders::srgb_to_bt2020_abs10k(style.color, nits)
}

/// The element area a ring occupies: the window inflated by the ring width
/// plus the glow reach, on every side.
///
/// Extracted and tested because the shader measures from the centre of this
/// area and reconstructs the window rect by deflating it by the same amount.
/// If the two ever disagree the ring silently drifts off the window edge, which
/// looks like a rendering bug rather than an arithmetic one.
pub(crate) fn ring_area(
    window_rect: Rectangle<i32, Logical>,
    border: i32,
    glow: i32,
) -> Rectangle<i32, Logical> {
    let margin = border.max(0) + glow.max(0);
    Rectangle::new(
        (window_rect.loc.x - margin, window_rect.loc.y - margin).into(),
        (
            window_rect.size.w + 2 * margin,
            window_rect.size.h + 2 * margin,
        )
            .into(),
    )
}

/// The border ring for ONE window, whose logical rect is already
/// output-region-local (the same space `Space::render_elements_for_region`
/// positions elements in).
///
/// Per window rather than per output because borders have to be emitted
/// *interleaved with* the windows they belong to. Drawn as one group above the
/// whole space -- which is what this did originally -- every window's border
/// floats above every other window, so a maximized window ends up covered in
/// the borders of the windows hidden behind it.
///
/// Returns nothing for a fullscreen window: a border above a fullscreen window
/// disqualifies it from direct primary-plane scanout, and there is no gap to
/// draw it in anyway.
///
/// `hdr` says whether this output is compositing through the linear HDR path
/// this frame; when false the luminance mechanism is entirely inert.
pub(crate) fn window_border_elements<R>(
    state: &RubixState,
    renderer: &mut R,
    id: u32,
    window_rect: Rectangle<i32, Logical>,
    hdr: bool,
) -> Option<BorderRingElement>
where
    R: GlesAccess,
{
    let deco = &state.config.decoration;
    let width = deco.border_width as i32;
    if state.fullscreen_windows.contains(&id) {
        return None;
    }
    let (app_id, title) = state.window_identity(id);
    let style = deco.style_for(
        app_id.as_deref(),
        title.as_deref(),
        state.focused_window_id() == Some(id),
    );
    let glow = style.glow_margin as i32;
    // Nothing to draw: no ring and no glow, or fully transparent.
    if (width <= 0 && glow <= 0) || style.color.a() <= 0.0 {
        return None;
    }
    let program = ring_program(renderer.gles_renderer())?;
    let color = resolved_color(&style, hdr, state.sdr_white_nits);

    // The element covers the window inflated by ring + glow; the shader
    // measures everything from the centre of that area, so the two must agree.
    let area = ring_area(window_rect, width, glow);
    // The corner radius the ring follows is the window's own, so the two stay
    // concentric; the shader adds the border width for the outer curve.
    let radius = deco.corner_radius as f32;

    Some(BorderRingElement {
        inner: PixelShaderElement::new(
            program,
            area,
            // No opaque regions: a ring is a band and a glow over a hole.
            None,
            1.0,
            vec![
                Uniform::new("rubix_color", (color.r(), color.g(), color.b(), color.a())),
                Uniform::new("rubix_radius", radius),
                Uniform::new("rubix_border", width as f32),
                Uniform::new("rubix_glow", glow as f32),
                Uniform::new("rubix_falloff", style.glow_falloff),
            ],
            Kind::Unspecified,
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Logical> {
        Rectangle::new((x, y).into(), (w, h).into())
    }

    fn style(nits: Option<f32>) -> BorderStyle {
        BorderStyle {
            color: Color32F::new(0.4, 0.6, 0.9, 1.0),
            luminance_nits: nits,
            glow_margin: 0,
            glow_falloff: 2.0,
        }
    }

    // The shader deflates by exactly this margin to recover the window rect.
    #[test]
    fn ring_area_inflates_by_border_plus_glow_on_every_side() {
        let area = ring_area(rect(10, 20, 100, 50), 2, 8);
        assert_eq!((area.loc.x, area.loc.y), (0, 10));
        assert_eq!((area.size.w, area.size.h), (120, 70));
    }

    #[test]
    fn ring_area_is_the_window_itself_with_no_border_and_no_glow() {
        let window = rect(10, 20, 100, 50);
        assert_eq!(ring_area(window, 0, 0), window);
    }

    #[test]
    fn ring_area_ignores_negative_inputs_rather_than_shrinking() {
        let window = rect(10, 20, 100, 50);
        assert_eq!(ring_area(window, -4, 0), window);
    }

    #[test]
    fn ring_area_stays_centred_on_its_window() {
        let window = rect(10, 20, 100, 50);
        let area = ring_area(window, 3, 7);
        let window_centre = (
            window.loc.x * 2 + window.size.w,
            window.loc.y * 2 + window.size.h,
        );
        let area_centre = (area.loc.x * 2 + area.size.w, area.loc.y * 2 + area.size.h);
        assert_eq!(window_centre, area_centre, "shader assumes a shared centre");
    }

    // Every uniform the Rust side declares must exist in the GLSL, and vice
    // versa. A mismatch compiles and links fine and simply never takes effect.
    #[test]
    fn shader_declares_every_uniform_the_element_sets() {
        for (name, decl) in [
            ("rubix_color", "uniform vec4 rubix_color;"),
            ("rubix_radius", "uniform float rubix_radius;"),
            ("rubix_border", "uniform float rubix_border;"),
            ("rubix_glow", "uniform float rubix_glow;"),
            ("rubix_falloff", "uniform float rubix_falloff;"),
        ] {
            assert!(
                BORDER_RING.contains(decl),
                "{name} is not declared in the border shader"
            );
            // And actually used, not just declared -- an unused uniform is
            // optimised out and the driver reports no such location.
            assert!(
                BORDER_RING.matches(name).count() >= 2,
                "{name} is declared but never read"
            );
        }
    }

    // GLSL requires declaration before use, and a violation here fails at
    // shader-compile time on a machine with a GPU -- which no test has. Pinning
    // the order is the only place this can be caught early.
    #[test]
    fn shader_declares_things_before_it_uses_them() {
        let mut last = 0;
        for token in [
            "uniform vec2 size;",
            "uniform float rubix_border;",
            "uniform float rubix_glow;",
            "uniform float rubix_falloff;",
            "float roundRectSdf",
            "float ringCoverage",
            "float dither",
            "void main",
        ] {
            let at = BORDER_RING.find(token).unwrap_or_else(|| panic!("{token} missing"));
            assert!(at > last, "{token} appears before something that uses it");
            last = at;
        }
    }

    // The glow is a long ramp from a high absolute luminance to nothing, and
    // max() would crease where it meets the ring. Both mitigations are easy to
    // undo by accident while editing the shader.
    #[test]
    fn coverage_is_a_union_and_is_supersampled() {
        assert!(
            BORDER_RING.contains("ring + glow * (1.0 - ring)"),
            "glow must composite under the ring, not max() with it"
        );
        assert_eq!(
            BORDER_RING.matches("ringCoverage(centred").count(),
            4,
            "coverage should be sampled at four sub-pixel offsets"
        );
        assert!(BORDER_RING.contains("dither("), "banding mitigation removed");
    }

    #[test]
    fn shader_has_no_version_directive() {
        // Smithay's pixel-shader path prepends its own; a #version here fails
        // to compile at runtime, where there is no test to catch it.
        assert!(!BORDER_RING.contains("#version"));
    }

    #[test]
    fn shader_reads_the_varying_and_size_smithay_provides() {
        assert!(BORDER_RING.contains("varying vec2 v_coords;"));
        assert!(BORDER_RING.contains("uniform vec2 size;"));
        assert!(BORDER_RING.contains("uniform float alpha;"));
    }

    #[test]
    fn colour_passes_straight_through_on_a_non_hdr_output() {
        let out = resolved_color(&style(Some(600.0)), false, 200.0);
        assert_eq!((out.r(), out.g(), out.b()), (0.4, 0.6, 0.9));
    }

    // 350 nits must land at 350/10000 of the working space's full scale,
    // whatever SDR white happens to be -- that is the point of absolute
    // luminance, and it is what lets a focus ring sit above SDR white.
    #[test]
    fn hdr_colour_is_absolute_not_relative_to_sdr_white() {
        let at_200 = resolved_color(&style(Some(350.0)), true, 200.0);
        let at_300 = resolved_color(&style(Some(350.0)), true, 300.0);
        assert!(
            (at_200.r() - at_300.r()).abs() < 1e-6,
            "an absolute nit value must not move when SDR white does"
        );
    }

    #[test]
    fn hdr_colour_without_a_luminance_sits_at_sdr_white() {
        let implicit = resolved_color(&style(None), true, 200.0);
        let explicit = resolved_color(&style(Some(200.0)), true, 200.0);
        assert!((implicit.r() - explicit.r()).abs() < 1e-6);
    }

    #[test]
    fn a_brighter_rule_produces_a_brighter_colour() {
        let dim = resolved_color(&style(Some(200.0)), true, 200.0);
        let bright = resolved_color(&style(Some(600.0)), true, 200.0);
        assert!(bright.r() > dim.r());
    }

    #[test]
    fn hdr_colour_preserves_alpha() {
        let mut s = style(Some(350.0));
        s.color = Color32F::new(0.4, 0.6, 0.9, 0.5);
        assert!((resolved_color(&s, true, 200.0).a() - 0.5).abs() < 1e-6);
    }
}
