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

use smithay::output::Output;

use crate::config::{BorderEffect, WindowStyle};
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
uniform vec4 rubix_effect_color;
uniform float rubix_effect;
uniform float rubix_time;
uniform float rubix_depth;

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

// One full turn, for the effect functions below.
#define TAU 6.2831853

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

// Normalised position around the ring, 0.0 to 1.0, measured as arc length
// along the rounded rectangle rather than as a polar angle.
//
// A polar angle is constant along a ray from the centre, so its iso-lines meet
// a long edge obliquely: a travelling effect reads as a slanted streak that
// ignores the edge it is crossing, and it sweeps the short edges faster than
// the long ones. Arc length's iso-lines are the rounded rectangle's own
// normals, so a wavefront meets every edge square-on and travels at one speed
// the whole way round.
//
// Fold to the first quadrant, measure along that quadrant's boundary, unfold.
// `inner` is the box the corner arcs are centred on and `d` the offset from
// its corner, which is the same decomposition `roundRectSdf` uses.
float perimeterCoord(vec2 centred, vec2 winHalf, float r) {
    vec2 inner = winHalf - vec2(r);
    vec2 d = abs(centred) - inner;

    // One straight run along each axis plus one corner arc. Floored off zero
    // because a window can be degenerate before it is first configured, and
    // the division below is the only place that would show it.
    float quarter = max(inner.x + inner.y + r * (TAU * 0.25), 1.0 / 65536.0);

    float u;
    if (d.x > 0.0 && d.y > 0.0) {
        // Corner arc. atan(0, 0) is undefined per the GLSL ES 100 spec; both
        // components are strictly positive inside this branch, so the case
        // never arises.
        u = inner.y + r * atan(d.y, d.x);
    } else if (d.x > d.y) {
        // Off the vertical edge, or inside and nearer to it. `d.y <= 0` here,
        // so the height is already within the straight run.
        u = abs(centred.y);
    } else {
        // Off the horizontal edge, measured backwards from the corner.
        u = inner.y + r * (TAU * 0.25) + (inner.x - abs(centred.x));
    }

    // `u` runs from the +x axis crossing towards the +y one, so the quadrants
    // that traverse it backwards subtract rather than add.
    float t;
    if (centred.y >= 0.0) {
        t = (centred.x >= 0.0) ? u : 2.0 * quarter - u;
    } else {
        t = (centred.x >= 0.0) ? 4.0 * quarter - u : 2.0 * quarter + u;
    }
    return t / (4.0 * quarter);
}

// The effect's instantaneous amount, 0.0 to 1.0. Zero for `None`, which makes
// the mix below collapse to the ring's own colour at full alpha -- the static
// ring is bit-identical to what it was before effects existed.
//
// `centred` is in element-centred pixels, the same space the SDF works in, so
// the travelling effects can share the ring's own geometry rather than
// re-deriving it.
float effectAmount(vec2 centred, vec2 winHalf, float r) {
    if (rubix_effect < 0.5) {
        return 0.0;
    }
    float phase = rubix_time * TAU;
    if (rubix_effect < 1.5) {
        // Pulse
        return 0.5 + 0.5 * sin(phase);
    }
    if (rubix_effect < 2.5) {
        // Breathing: the same swell squared, which stretches the trough and
        // compresses the peak. A raw sine spends as long bright as dark and
        // reads as a blink; breathing wants to sit mostly low.
        float s = 0.5 + 0.5 * sin(phase);
        return s * s;
    }
    float p = perimeterCoord(centred, winHalf, r);
    if (rubix_effect < 3.5) {
        // GradientFlow: two wavelengths around the ring, drifting.
        return 0.5 + 0.5 * sin((p * 2.0 - rubix_time) * TAU);
    }
    // Comet: fract() gives a sawtooth around the perimeter; raising it to a
    // high power crushes all but the leading edge, leaving a bright head and a
    // fading tail. pow() rather than exp() so the tail reaches exactly zero at
    // the wrap point instead of leaving a seam.
    float f = fract(p - rubix_time);
    return pow(f, 8.0);
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

    // Gated to where the ring actually puts something. `a` is exactly zero
    // across the window's interior and outside the glow, and noise added there
    // is not dithering, it is light -- the element covers the window inflated
    // by border and glow and draws *above* it, so that light lands on the
    // window's own pixels.
    //
    // It is invisible in SDR, which is why it went unnoticed: a quarter of an
    // 8-bit step. Under PQ it is not, because the curve is near-vertical at
    // black. Premultiplied against a 1200-nit border colour, a lift of 1/1024
    // in alpha is ~1.2 cd/m^2, which PQ encodes at about 16% of the code
    // range. Measured on a black interior before this gate: mean 5.7% of range,
    // peaks at 15.7%. That is the "film grain over HDR windows".
    float lit = smoothstep(0.0, 1.0 / 255.0, a);
    a = clamp(a + dither(v_coords * size) * (1.0 / 512.0) * lit, 0.0, 1.0);

    float m = effectAmount(centred, winHalf, r);

    // Colour travels from the ring's own colour to its effect peak. On an SDR
    // output, or with no `effect_luminance_nits`, the two endpoints are the
    // same colour and this mix is a no-op -- the alpha term below is then the
    // only thing that moves, which is what makes the effect visible at all
    // without HDR headroom.
    vec4 c = mix(rubix_color, rubix_effect_color, m);

    // Alpha dip. Gated on the effect being live so that `None` is exactly the
    // old behaviour rather than the old behaviour times (1.0 - depth).
    float k = (rubix_effect < 0.5) ? 1.0 : mix(1.0 - rubix_depth, 1.0, m);

    // Premultiplied, which is what the renderer blends for.
    gl_FragColor = vec4(c.rgb * c.a * a * k, c.a * a * k) * alpha;
}
"#;

/// Everything that decides what a ring looks like. Compared frame to frame so
/// an unchanged ring can be reused rather than rebuilt.
#[derive(PartialEq, Clone, Copy)]
struct RingInputs {
    area: Rectangle<i32, Logical>,
    color: (f32, f32, f32, f32),
    radius: f32,
    border: f32,
    glow: f32,
    falloff: f32,
    opacity: f32,
    effect: f32,
    effect_color: (f32, f32, f32, f32),
    depth: f32,
    /// The quantised clock, as a tick count rather than a float.
    ///
    /// An integer is what makes the frame-to-frame `PartialEq` exact: two
    /// frames inside the same tick compare equal, reuse the cached element,
    /// keep its `Id` and commit counter, and generate no damage at all. A
    /// float derived from `Instant::elapsed` would differ in its low bits
    /// every frame and defeat the whole cache.
    tick: i64,
}

thread_local! {
    // One cached ring per window.
    //
    // Not an optimization -- `PixelShaderElement::new` mints a fresh `Id`, and
    // an element whose id changes every frame is a brand-new element to the
    // damage tracker, which then fully damages its area every frame. With a
    // glow the ring's area is the window inflated on all sides, so every
    // window redamages a band around itself on every frame and partial-damage
    // repaints stop being possible at all. (Stale state is pruned by the
    // tracker, so this cost time rather than memory.)
    //
    // Reusing the element keeps its id and commit counter stable while nothing
    // about it changes; a real change rebuilds, which correctly damages.
    static RING_CACHE: RefCell<HashMap<u32, (RingInputs, PixelShaderElement)>> =
        RefCell::new(HashMap::new());

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
                        UniformName::new("rubix_effect_color", UniformType::_4f),
                        UniformName::new("rubix_effect", UniformType::_1f),
                        UniformName::new("rubix_time", UniformType::_1f),
                        UniformName::new("rubix_depth", UniformType::_1f),
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
/// asking for 350 nits gets exactly 350 nits -- the number is the border's own
/// emitted luminance, not a white reference the colour then lands some
/// fraction below. Everywhere else it is the configured sRGB colour, untouched
/// -- `luminance_nits` is not consulted at all, so a machine with no HDR
/// display behaves as though none of this exists.
///
/// With no `luminance_nits` set there is no requested level, so the colour
/// falls back to being white-referred against the session's SDR white and
/// renders at the brightness any other piece of SDR chrome would.
pub(crate) fn resolved_color(style: &WindowStyle, hdr: bool, sdr_white_nits: f32) -> Color32F {
    if !hdr {
        return style.color;
    }
    match style.luminance_nits {
        Some(nits) => crate::hdr_shaders::srgb_at_luminance(style.color, nits),
        None => crate::hdr_shaders::srgb_to_bt2020_abs10k(style.color, sdr_white_nits),
    }
}

/// Numeric code for the shader's `rubix_effect < N.5` band chain.
///
/// Must stay in step with the bands `effectAmount` in `BORDER_RING` tests
/// against -- a mismatch here does not fail to compile or link, it just
/// silently plays the wrong effect.
fn effect_code(effect: BorderEffect) -> f32 {
    match effect {
        BorderEffect::None => 0.0,
        BorderEffect::Pulse => 1.0,
        BorderEffect::Breathing => 2.0,
        BorderEffect::GradientFlow => 3.0,
        BorderEffect::Comet => 4.0,
    }
}

/// What fraction of `target` is covered by `occluders`, 0.0 to 1.0.
///
/// Exact rather than estimated: the occluders are subtracted from the target
/// and what survives is the uncovered area. Overlapping occluders therefore
/// cannot double-count, which a naive sum of intersection areas would do --
/// and would report a window as more covered than it is, making it flicker
/// between faded and not as windows move.
pub(crate) fn covered_fraction(
    target: Rectangle<i32, Logical>,
    occluders: &[Rectangle<i32, Logical>],
) -> f32 {
    let total = target.size.w as i64 * target.size.h as i64;
    if total <= 0 {
        return 0.0;
    }
    let uncovered: i64 = target
        .subtract_rects(occluders.iter().copied())
        .iter()
        .map(|r| r.size.w as i64 * r.size.h as i64)
        .sum();
    (1.0 - uncovered as f32 / total as f32).clamp(0.0, 1.0)
}

/// How covered each window on `output` is, keyed by window id.
///
/// Walks front-to-back accumulating the windows already passed, so each window
/// is tested only against the ones stacked above it.
///
/// Occluders count regardless of their own opacity, which is deliberate. The
/// point of fading a covered window is not that it cannot be seen -- if the
/// window above were opaque it could not be seen anyway, and the renderer
/// would already be culling it. The point is that the window above is
/// *translucent*, so a covered window bleeds through it and makes it muddy.
pub(crate) fn occlusion_map(state: &RubixState, output: &Output) -> HashMap<u32, f32> {
    let mut map = HashMap::new();
    if state.config.decoration.obscured_opacity >= 1.0 {
        return map;
    }
    let Some(region) = state.space.output_geometry(output) else {
        return map;
    };
    let mut occluders: Vec<Rectangle<i32, Logical>> = Vec::new();
    for window in state.space.elements().rev() {
        let Some(location) = state.space.element_location(window) else { continue };
        let rect = Rectangle::new(location, window.geometry().size);
        if !region.overlaps(rect) {
            continue;
        }
        // Menus and tooltips take no part in this, in either direction. They
        // are never faded themselves (they carry no decoration at all), and
        // they must not fade what they cover: a dropdown is transient chrome,
        // and treating it as an occluder makes the window underneath dim and
        // undim every time the user opens a menu.
        if crate::state::is_unmanaged(window) {
            continue;
        }
        if let Some(id) = state
            .windows
            .iter()
            .find_map(|(id, w)| (w == window).then_some(*id))
        {
            map.insert(id, covered_fraction(rect, &occluders));
        }
        occluders.push(rect);
    }
    map
}

/// The style in force for one window right now.
///
/// Resolved once per window per frame and shared by the window's own surfaces
/// (for opacity) and its border, so the two can never disagree about which
/// rule matched.
///
/// Fullscreen windows are forced fully opaque regardless of what a rule says.
/// A translucent window cannot take direct primary-plane scanout, which is the
/// path a fullscreen window exists to get -- the same trade borders and
/// rounding already make.
pub(crate) fn style_for_window(state: &RubixState, id: u32, covered: f32) -> WindowStyle {
    let deco = &state.config.decoration;
    let (app_id, title) = state.window_identity(id);
    let mut style = deco.style_for(
        app_id.as_deref(),
        title.as_deref(),
        state.focused_window_id() == Some(id),
    );
    // `min`, not assignment: a window a rule already made more transparent
    // stays that way, and being covered can only ever fade it further. That
    // keeps the interaction monotone, so no rule can be surprised into becoming
    // *more* visible by being obscured.
    if covered >= deco.obscured_threshold {
        style.opacity = style.opacity.min(deco.obscured_opacity);
    }
    // A window nobody can see does not get to keep the compositor awake. The
    // occlusion fraction is already computed for the opacity fade above, so
    // this is free, and it is the difference between "one animated border" and
    // "an animated border behind every maximised window still driving a
    // repaint every tick".
    if covered >= deco.obscured_threshold {
        style.effect = BorderEffect::None;
    }
    // Applied last so it wins over everything: a translucent window cannot take
    // direct primary-plane scanout, which is the path a fullscreen window
    // exists to get.
    if state.fullscreen_windows.contains(&id) {
        style.opacity = 1.0;
    }
    // The wallpaper-derived borders, when enabled, replace both configured
    // colours -- the contrasting glow on the focused window, the muted border
    // role on every other. Leaving the unfocused ring on a fixed colour would
    // strand it as the one thing on screen not following the wallpaper, and it
    // costs nothing to theme: focus is already marked by the chroma gap between
    // the two roles and by the glow margin, which unfocused windows do not get.
    //
    // Done here rather than at the draw site because this is the single funnel
    // every style passes through, so nothing can resolve a style and miss it.
    if let Some((focused, unfocused)) = state.theme_border_colors {
        let themed = if state.focused_window_id() == Some(id) { focused } else { unfocused };
        style.color = smithay::backend::renderer::Color32F::new(
            themed.r(),
            themed.g(),
            themed.b(),
            style.color.a(),
        );
    }
    style
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
    style: &WindowStyle,
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
    let glow = style.glow_margin as i32;
    // Nothing to draw: no ring and no glow, or fully transparent.
    if (width <= 0 && glow <= 0) || style.color.a() <= 0.0 {
        return None;
    }
    let program = ring_program(renderer.gles_renderer())?;
    let color = resolved_color(style, hdr, state.sdr_white_nits);

    // The effect's peak colour. On an HDR output with an effect luminance set
    // this is the SAME sRGB colour resolved at a different absolute
    // luminance, so the animation is a pure brightness swing at constant hue
    // -- a 200-to-800-nit swell that keeps its saturation, and both ends mean
    // the nits they say regardless of how saturated the colour is. Everywhere
    // else it is the base colour and the mix in the shader does nothing.
    let effect_color = match (hdr, style.effect_luminance_nits) {
        (true, Some(nits)) if style.effect != BorderEffect::None => {
            crate::hdr_shaders::srgb_at_luminance(style.color, nits)
        }
        _ => color,
    };

    // Quantise the clock to the effect's own rate. `start_time` is the same
    // monotonic origin the frame callbacks use, so every output animates in
    // phase rather than drifting apart per-CRTC.
    let (effect_id, tick) = match style.effect {
        BorderEffect::None => (0.0_f32, 0_i64),
        effect => {
            let fps = style.effect_fps.max(1) as f64;
            let elapsed = state.start_time.elapsed().as_secs_f64();
            (effect_code(effect), (elapsed * fps).floor() as i64)
        }
    };

    // The element covers the window inflated by ring + glow; the shader
    // measures everything from the centre of that area, so the two must agree.
    let area = ring_area(window_rect, width, glow);
    // The corner radius the ring follows is the window's own, so the two stay
    // concentric; the shader adds the border width for the outer curve.
    let radius = deco.corner_radius as f32;

    let inputs = RingInputs {
        area,
        color: (color.r(), color.g(), color.b(), color.a()),
        radius,
        border: width as f32,
        glow: glow as f32,
        falloff: style.glow_falloff,
        // The ring fades with its window: the shader multiplies by this, so a
        // dimmed window does not keep a full-strength border.
        opacity: style.opacity,
        effect: effect_id,
        effect_color: (
            effect_color.r(),
            effect_color.g(),
            effect_color.b(),
            effect_color.a(),
        ),
        depth: style.effect_depth,
        tick,
    };

    let inner = RING_CACHE.with_borrow_mut(|cache| {
        if let Some((cached, element)) = cache.get(&id) {
            if *cached == inputs {
                // Same id, same commit counter: no damage, nothing redrawn.
                return element.clone();
            }
        }
        let element = PixelShaderElement::new(
            program,
            inputs.area,
            // No opaque regions: a ring is a band and a glow over a hole.
            None,
            inputs.opacity,
            vec![
                Uniform::new("rubix_color", inputs.color),
                Uniform::new("rubix_radius", inputs.radius),
                Uniform::new("rubix_border", inputs.border),
                Uniform::new("rubix_glow", inputs.glow),
                Uniform::new("rubix_falloff", inputs.falloff),
                Uniform::new("rubix_effect_color", inputs.effect_color),
                Uniform::new("rubix_effect", inputs.effect),
                // Speed folded in here, not in the shader: the shader's phase
                // is then just `time * TAU` and there is one fewer uniform.
                Uniform::new(
                    "rubix_time",
                    inputs.tick as f32 / style.effect_fps.max(1) as f32 * style.effect_speed,
                ),
                Uniform::new("rubix_depth", inputs.depth),
            ],
            Kind::Unspecified,
        );
        cache.insert(id, (inputs, element.clone()));
        element
    });

    Some(BorderRingElement { inner })
}

/// Drop cached rings for windows that no longer exist. Called once per frame by
/// the element gather, not once per window.
pub(crate) fn prune_ring_cache(state: &RubixState) {
    RING_CACHE.with_borrow_mut(|cache| {
        cache.retain(|id, _| state.windows.contains_key(id));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Logical> {
        Rectangle::new((x, y).into(), (w, h).into())
    }

    fn style(nits: Option<f32>) -> WindowStyle {
        WindowStyle {
            color: Color32F::new(0.4, 0.6, 0.9, 1.0),
            luminance_nits: nits,
            glow_margin: 0,
            glow_falloff: 2.0,
            opacity: 1.0,
            backdrop_tonemap: false,
            backdrop_blur: false,
            refract: false,
            effect: BorderEffect::None,
            effect_speed: 1.0,
            effect_luminance_nits: None,
            effect_depth: 0.6,
            effect_fps: 24,
        }
    }

    // The shader deflates by exactly this margin to recover the window rect.
    // A field left out of RingInputs' comparison would mean a ring that never
    // updates when that field changes -- a border stuck on the wrong colour
    // after a focus change, say. Each field is perturbed individually so a
    // missing one fails here rather than on screen.
    #[test]
    fn every_ring_input_participates_in_the_comparison() {
        let base = RingInputs {
            area: rect(0, 0, 100, 100),
            color: (0.1, 0.2, 0.3, 1.0),
            radius: 8.0,
            border: 2.0,
            glow: 12.0,
            falloff: 2.0,
            opacity: 1.0,
            effect: 1.0,
            effect_color: (0.5, 0.5, 0.5, 1.0),
            depth: 0.6,
            tick: 5,
        };
        let mutations: [(&str, fn(&mut RingInputs)); 11] = [
            ("area", |i| i.area = rect(1, 0, 100, 100)),
            ("color", |i| i.color = (0.9, 0.2, 0.3, 1.0)),
            ("radius", |i| i.radius = 9.0),
            ("border", |i| i.border = 3.0),
            ("glow", |i| i.glow = 13.0),
            ("falloff", |i| i.falloff = 3.0),
            ("opacity", |i| i.opacity = 0.5),
            ("effect", |i| i.effect = 2.0),
            ("effect_color", |i| i.effect_color = (0.1, 0.1, 0.1, 1.0)),
            ("depth", |i| i.depth = 0.1),
            ("tick", |i| i.tick = 6),
        ];
        for (name, mutate) in mutations {
            let mut changed = base;
            mutate(&mut changed);
            assert!(changed != base, "{name} is not compared, so it can go stale");
        }
    }

    // ---- occlusion ----

    #[test]
    fn nothing_above_means_nothing_covered() {
        assert_eq!(covered_fraction(rect(0, 0, 100, 100), &[]), 0.0);
    }

    #[test]
    fn an_exactly_matching_occluder_covers_everything() {
        let w = rect(10, 10, 100, 100);
        assert_eq!(covered_fraction(w, &[w]), 1.0);
    }

    #[test]
    fn a_larger_occluder_covers_everything() {
        assert_eq!(
            covered_fraction(rect(10, 10, 100, 100), &[rect(0, 0, 500, 500)]),
            1.0
        );
    }

    #[test]
    fn a_disjoint_occluder_covers_nothing() {
        assert_eq!(
            covered_fraction(rect(0, 0, 100, 100), &[rect(200, 200, 50, 50)]),
            0.0
        );
    }

    #[test]
    fn half_covering_reports_half() {
        let covered = covered_fraction(rect(0, 0, 100, 100), &[rect(0, 0, 50, 100)]);
        assert!((covered - 0.5).abs() < 1e-5, "got {covered}");
    }

    // The reason this is computed by subtraction rather than by summing
    // intersection areas: overlapping occluders would double-count, report more
    // than 100% coverage, and make a window flicker in and out of the faded
    // state as its neighbours move.
    #[test]
    fn overlapping_occluders_are_not_double_counted() {
        let covered = covered_fraction(
            rect(0, 0, 100, 100),
            &[rect(0, 0, 60, 100), rect(40, 0, 60, 100)],
        );
        assert!((covered - 1.0).abs() < 1e-5, "got {covered}");
    }

    #[test]
    fn two_partial_occluders_sum_to_their_union() {
        // Left quarter and right quarter, not touching: half in total.
        let covered = covered_fraction(
            rect(0, 0, 100, 100),
            &[rect(0, 0, 25, 100), rect(75, 0, 25, 100)],
        );
        assert!((covered - 0.5).abs() < 1e-5, "got {covered}");
    }

    #[test]
    fn a_degenerate_window_reports_no_coverage_rather_than_dividing_by_zero() {
        assert_eq!(covered_fraction(rect(0, 0, 0, 0), &[rect(0, 0, 10, 10)]), 0.0);
    }

    // A window peeking out from behind a maximized neighbour by a few pixels
    // should still fade -- which is why the default threshold is 0.9 and not
    // 1.0.
    #[test]
    fn a_nearly_covered_window_passes_the_default_threshold() {
        let covered = covered_fraction(rect(0, 0, 100, 100), &[rect(0, 0, 100, 96)]);
        assert!(covered >= 0.9, "got {covered}");
    }

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
            ("rubix_effect_color", "uniform vec4 rubix_effect_color;"),
            ("rubix_effect", "uniform float rubix_effect;"),
            ("rubix_time", "uniform float rubix_time;"),
            ("rubix_depth", "uniform float rubix_depth;"),
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

    // No `luminance_nits` means no requested level, so the colour is
    // white-referred against SDR white like any other piece of SDR chrome.
    #[test]
    fn hdr_colour_without_a_luminance_sits_at_sdr_white() {
        let implicit = resolved_color(&style(None), true, 200.0);
        let expected =
            crate::hdr_shaders::srgb_to_bt2020_abs10k(style(None).color, 200.0);
        assert!((implicit.r() - expected.r()).abs() < 1e-6);
        assert!((implicit.g() - expected.g()).abs() < 1e-6);
        assert!((implicit.b() - expected.b()).abs() < 1e-6);
    }

    // ...and setting the number to that same value is NOT the same thing, for
    // anything but white. `Some(200)` asks for 200 nits on screen; the
    // white-referred fallback puts this blue at roughly a third of that. This
    // is the distinction the config key exists to make, so it gets a test of
    // its own rather than riding on the one above.
    #[test]
    fn an_explicit_luminance_means_emitted_nits_not_a_white_reference() {
        let implicit = resolved_color(&style(None), true, 200.0);
        let explicit = resolved_color(&style(Some(200.0)), true, 200.0);
        assert!(
            explicit.r() > implicit.r() * 2.0,
            "explicit {} should sit well above white-referred {}",
            explicit.r(),
            implicit.r()
        );
        let luma = crate::hdr_shaders::bt2020_luminance([
            explicit.r(),
            explicit.g(),
            explicit.b(),
        ]) * 10000.0;
        assert!((luma - 200.0).abs() < 0.5, "emitted {luma} nits, asked for 200");
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

    // ---- effects ----

    // The shader routes on `rubix_effect < N.5` bands; `effect_code` must land
    // each variant inside the band the shader would actually pick for it, or
    // the Rust side and the GLSL silently disagree about which effect plays.
    #[test]
    fn effect_codes_match_the_shader_bands() {
        let bands: [(BorderEffect, f32); 5] = [
            (BorderEffect::None, 0.5),
            (BorderEffect::Pulse, 1.5),
            (BorderEffect::Breathing, 2.5),
            (BorderEffect::GradientFlow, 3.5),
            (BorderEffect::Comet, f32::INFINITY),
        ];
        for (effect, upper_bound) in bands {
            let code = effect_code(effect);
            assert!(
                code < upper_bound,
                "{effect:?} coded as {code} does not fall under its shader band ({upper_bound})"
            );
        }
    }

    // A `None`-effect style must produce the exact same `RingInputs` moments
    // apart, or a static ring damages every frame regardless of the effect
    // system existing at all. Mirrors the `match style.effect` branch in
    // `window_border_elements`: `None` always resolves to `(0.0, 0)`
    // regardless of how much time passed between the two builds.
    #[test]
    fn a_none_effect_produces_a_zero_tick() {
        fn quantise_for(effect: BorderEffect, fps: u32, elapsed: f64) -> (f32, i64) {
            match effect {
                BorderEffect::None => (0.0, 0),
                effect => (effect_code(effect), (elapsed * fps.max(1) as f64).floor() as i64),
            }
        }
        let (id_a, tick_a) = quantise_for(BorderEffect::None, 24, 0.001);
        std::thread::sleep(std::time::Duration::from_millis(5));
        let (id_b, tick_b) = quantise_for(BorderEffect::None, 24, 0.100);
        assert_eq!((id_a, tick_a), (id_b, tick_b));

        let mut a = RingInputs {
            area: rect(0, 0, 10, 10),
            color: (0.0, 0.0, 0.0, 1.0),
            radius: 0.0,
            border: 0.0,
            glow: 0.0,
            falloff: 0.0,
            opacity: 1.0,
            effect: id_a,
            effect_color: (0.0, 0.0, 0.0, 1.0),
            depth: 0.0,
            tick: tick_a,
        };
        let b = RingInputs { effect: id_b, tick: tick_b, ..a };
        assert!(a == b, "a None effect must compare equal across the sleep");
        a.tick = 1;
        assert!(a != b, "sanity: tick does participate in the comparison");
    }

    // Elapsed times inside the same 1/fps window must quantise to the same
    // tick; times either side of the boundary must not. This is the whole
    // mechanism that keeps an animated border from damaging every frame.
    #[test]
    fn the_tick_quantises_to_effect_fps() {
        let fps = 24.0_f64;
        let quantise = |elapsed: f64| (elapsed * fps).floor() as i64;
        assert_eq!(quantise(0.0400), quantise(0.0410));
        assert_eq!(quantise(0.0400), quantise(0.0416));
        assert_ne!(quantise(0.0416), quantise(0.0420));
    }

    // Source-level check: with no effect live, the shader's colour mix and
    // alpha dip must both be no-ops, so `BorderEffect::None` reproduces the
    // exact pre-effects rendering.
    #[test]
    fn shader_collapses_to_the_static_ring_with_no_effect() {
        assert!(
            BORDER_RING.contains("if (rubix_effect < 0.5) {\n        return 0.0;"),
            "effectAmount must guard on rubix_effect < 0.5"
        );
        assert!(
            BORDER_RING.contains("(rubix_effect < 0.5) ? 1.0 :"),
            "the alpha-dip ternary must guard on rubix_effect < 0.5 too"
        );
    }
}
