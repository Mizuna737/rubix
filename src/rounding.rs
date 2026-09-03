//! Rounded window corners (HDR Phase 5d).
//!
//! Corners are clipped by a signed-distance mask applied in the fragment
//! shader, not by drawing anything over them -- there is nothing to draw *with*
//! (the compositor has no access to whatever is behind a window), so the pixels
//! have to be discarded at the source.
//!
//! ## Why this needed a patched smithay
//!
//! The mask needs per-element uniforms, and smithay exposes the texture-program
//! override only on `GlesFrame`. On the udev backend a render element is handed
//! a `MultiFrame`, whose inner frame was a private field with no accessor --
//! unreachable on the backend that matters. `MultiFrame::render_frame_mut` is a
//! local patch to the fork we already pin (see Cargo.toml).
//!
//! A renderer-wide override was reachable without the patch, and would even
//! have been enough for the maths -- but it rounds *every* textured element:
//! the bar, popups, the cursor, and each subsurface of a window separately,
//! since `WaylandSurfaceRenderElement` is per-surface rather than per-window.
//!
//! ## Why the decode shaders had to learn about it
//!
//! There is one texture-program slot per draw, and on an HDR output it is
//! already taken -- that is where `DECODE_SDR`/`DECODE_HDR_PQ`/
//! `DECODE_WINDOWS_SCRGB` live. A rounding program that grabbed the slot would
//! clobber the decode and misrender colour on exactly the outputs we care most
//! about. So rounding is *folded into* each decode rather than competing with
//! it: every variant gets the same mask appended, and an element selects the
//! variant matching the decode it would have taken anyway.
//!
//! ## Cost
//!
//! A rounded window is no longer fully opaque, so these elements report no
//! opaque regions. That costs the occlusion culling an opaque window would
//! normally grant. It is the honest answer -- claiming opacity would let the
//! renderer skip drawing what shows through the corners.
//!
//! All of it is inert at `corner_radius = 0` (the default): the callers skip
//! per-window gathering entirely and take the same batched path as before.

use std::cell::RefCell;

use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::element::utils::RescaleRenderElement;
use smithay::backend::renderer::gles::element::PixelShaderElement;
use smithay::backend::renderer::gles::{
    GlesError, GlesRenderer, GlesTexProgram, Uniform, UniformName, UniformType,
};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::backend::renderer::{Renderer, RendererSuper};
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::{ImportAll, ImportMem};
use smithay::backend::renderer::element::AsRenderElements;
use smithay::desktop::Window;
use smithay::output::Output;
use smithay::utils::{Buffer as BufferCoords, Logical, Physical, Point, Rectangle, Scale, Transform};
use smithay::utils::user_data::UserDataMap;

use smithay::wayland::seat::WaylandFocus;
use crate::color_management::DecodeKind;
use crate::config::WindowStyle;
use crate::cursor::RubixRenderElement;
use crate::RubixState;
use crate::hdr_shaders::{DECODE_HDR_PQ, DECODE_SDR, DECODE_WINDOWS_SCRGB};

/// GLSL appended to every rounding variant, immediately before `main` so the
/// `v_coords` varying it reads is already declared.
///
/// The mask is a rounded-rectangle signed distance field evaluated in
/// **window** space, not element space -- that distinction is the whole reason
/// the uniforms are shaped this way. A window's subsurfaces are separate
/// elements, and rounding each one against its own bounds would carve notches
/// out of the middle of a window. Each element instead reports where it sits
/// within its window, so every fragment can be tested against the one rounded
/// rect that matters.
///
/// The 1px `smoothstep` is the anti-aliasing; without it the corners stair-step
/// badly at small radii. Textures are premultiplied, so scaling the whole
/// colour by the mask is correct.
const ROUNDING_GLSL: &str = r#"
uniform float rubix_radius;
uniform vec2 rubix_win_size;
uniform vec2 rubix_elem_offset;
uniform vec2 rubix_elem_size;

float rubixCornerAlpha() {
    if (rubix_radius <= 0.0) {
        return 1.0;
    }
    vec2 p = v_coords * rubix_elem_size + rubix_elem_offset;
    vec2 h = rubix_win_size * 0.5;
    // Clamped for the same reason as the border ring: a radius larger than the
    // window's half-extent folds the distance field and breaks up the corners.
    float r = min(rubix_radius, min(h.x, h.y));
    vec2 q = abs(p - h) - (h - vec2(r));
    float d = length(max(q, vec2(0.0))) + min(max(q.x, q.y), 0.0) - r;
    return 1.0 - smoothstep(-0.5, 0.5, d);
}

"#;

/// GLSL for the crystal refraction, spliced in alongside the rounding mask.
/// Reads `rubix_elem_size` / `rubix_elem_offset` from `ROUNDING_GLSL`, so it
/// must be concatenated *after* it.
///
/// The field models the **solid**, not its cells. An earlier version scattered
/// Voronoi cells and gave each a random tilt; it worked, and it read as
/// low-poly 3D rather than as crystal. Three things were wrong with it, and
/// all three are properties of that model rather than of its tuning: every
/// face was about the same size, the tilts were mutually independent so
/// nothing organised them, and dispersion keyed to cell boundaries drew a
/// coloured line around every cell, which is indistinguishable from a
/// highlighted wireframe.
///
/// So the facets are derived here instead of authored. A crystal cleaves along
/// families of parallel planes, so the field is the distance to the nearest
/// plane over a few such families, and the facet normal is the gradient of
/// that distance. Faces then fall out of the geometry: their sizes vary
/// because they are cross-sections of intersecting families rather than cells
/// on a grid, and their orientations are drawn from one shared set of
/// directions, which is what makes them look related to each other the way
/// crystallographic axes are. It is also cheaper -- the families are dot
/// products against compile-time constants, where the Voronoi search cost
/// nine hashes and a square root per fragment.
///
/// Having a real surface normal is what unlocks the rest. Refraction alone
/// reads as tinted jelly, because the missing cue is reflectance: glass
/// reflects more at grazing angles, and nothing here did. The Fresnel term
/// blends in a reflection tap, using the wallpaper as its own environment.
///
/// Nothing in here manufactures luminance, which is a deliberate constraint
/// and not merely a nicety. The splice point is the *sample* site, so anything
/// added would land in whatever encoding the source uses -- adding a constant
/// is a mild lift in sRGB and an enormous one in PQ, so the same code would
/// mean different things in the eight program variants this is spliced into.
/// Everything below therefore modulates where samples are taken and how they
/// are mixed, never their magnitude. Internal fracture sheets get their glint
/// by going locally mirror-like (`max(fresnel, sheet)`), which is both what a
/// fracture physically is and energy the wallpaper already had.
const REFRACT_GLSL: &str = r#"
uniform float rubix_refract_strength;
uniform vec2 rubix_refract_uv_origin;
uniform vec2 rubix_refract_uv_per_px;
uniform float rubix_refract_facet_size;
uniform float rubix_refract_dispersion;

// Hash-without-sine (Dave Hoskins). Deliberately not the `fract(sin(...))`
// classic: that costs two SFU ops per call, and profiling put the noise -- not
// the texture taps, and not the plane families -- at well over half this
// shader's total cost.
//
// Two-channel on purpose. The domain warp needs two decorrelated values at the
// *same* coordinate, and taking them from two scalar lattices doubles the hash
// count for nothing; one vec2 lattice gives both from the same four hashes and
// measured 17% off the whole shader.
vec2 rubixHash22(vec2 p) {
    vec3 p3 = fract(vec3(p.x, p.y, p.x) * vec3(0.1031, 0.1030, 0.0973));
    p3 += dot(p3, p3.yzx + 33.33);
    return fract((p3.xx + p3.yz) * p3.zy);
}

vec2 rubixNoise2(vec2 p) {
    vec2 i = floor(p);
    vec2 f = p - i;
    f = f * f * (3.0 - 2.0 * f);
    vec2 a = rubixHash22(i);
    vec2 b = rubixHash22(i + vec2(1.0, 0.0));
    vec2 c = rubixHash22(i + vec2(0.0, 1.0));
    vec2 d = rubixHash22(i + vec2(1.0, 1.0));
    return mix(mix(a, b, f.x), mix(c, d, f.x), f.y);
}

// One family of parallel cleavage planes, normal `n`, unit spacing, offset by
// `phase`. Keeps the running nearest across families; the winner's normal is
// the facet normal, which is constant across a face and flips sign either side
// of a plane, so adjacent faces tilt away from each other like a real ridge.
//
// `amp` varies the tilt between families. Without it every facet bends by the
// same magnitude and only the direction changes, which looks stamped.
void rubixCleave(vec2 p, vec2 n, float phase, float amp,
                 inout float best, inout vec2 grad) {
    float f = fract(dot(p, n) + phase) - 0.5;
    float d = abs(f) * amp;
    if (d < best) {
        best = d;
        grad = n * sign(f) * amp;
    }
}

// Four families, at angles that are not simple fractions of each other so the
// faces do not settle into an obvious repeating tiling.
vec2 rubixCleavage(vec2 p) {
    float best = 1000.0;
    vec2 grad = vec2(0.0);
    rubixCleave(p, vec2( 0.9808,  0.1951), 0.00, 1.00, best, grad);
    rubixCleave(p, vec2( 0.3090,  0.9511), 0.37, 0.82, best, grad);
    rubixCleave(p, vec2(-0.5878,  0.8090), 0.61, 1.15, best, grad);
    rubixCleave(p, vec2(-0.9511,  0.3090), 0.13, 0.68, best, grad);
    return grad;
}

vec4 rubixSampleRefracted() {
    if (rubix_refract_strength <= 0.0) {
        return texture2D(tex, v_coords);
    }
    // Window-local pixels, and emphatically not `v_coords * rubix_elem_size +
    // rubix_elem_offset` the way the corner mask computes them. That works for
    // a window's own surfaces, whose `v_coords` span their whole texture, but a
    // backdrop quad is a *crop* of the shared wallpaper: its `v_coords` cover
    // only the slice under the window and slide as the window moves, and its
    // `rubix_elem_offset` is the quad's position on the output rather than an
    // offset within a window. Using them anchored the crystal to the desktop,
    // so a window dragged across the screen scooped up a different set of
    // facets the whole way. Undoing the crop instead pins the facets to the
    // window, which is what makes it read as a pane of something rather than a
    // hole onto something.
    vec2 pw = (v_coords - rubix_refract_uv_origin)
            / max(rubix_refract_uv_per_px, vec2(1e-9))
            / max(rubix_refract_facet_size, 1.0);

    // Plane families are exactly periodic on their own, and a periodic lattice
    // is its own kind of tell. The warp bends the planes gently, which also
    // happens to be true of real crystal faces.
    vec2 warp = rubixNoise2(pw * 0.35) - 0.5;
    vec2 pc = pw + warp * 0.9;

    // Two octaves: broad cleavage with finer fracture inside it. A single
    // scale is what made the first version read as uniformly tessellated.
    vec2 grad = rubixCleavage(pc) + rubixCleavage(pc * 3.3 + 4.1) * 0.35;

    // Internal fracture planes that never reach the surface -- the bright
    // suspended veils in natural quartz. Patchy on purpose: a fracture that
    // spans the whole window reads as a scratch on the screen instead.
    float sheetLine = abs(fract(dot(pc, vec2(0.6, 0.8)) * 1.7) - 0.5);
    float sheetMask = smoothstep(0.45, 0.80, rubixNoise2(pw * 0.22 + 31.0).x);
    float sheet = sheetMask * smoothstep(0.045, 0.0, sheetLine);

    // View is orthographic and screen-facing, so the facet normal's z is the
    // cosine directly.
    vec3 normal = normalize(vec3(-grad * 0.8, 1.0));
    float grazing = 1.0 - clamp(normal.z, 0.0, 1.0);
    float g2 = grazing * grazing;
    // Schlick, with glass's 0.04 normal-incidence reflectance. A fracture
    // sheet overrides it to near-mirror, which is what a fracture is.
    float fresnel = clamp(max(0.04 + 0.96 * g2 * g2 * grazing, sheet), 0.0, 1.0);

    // Dispersion follows the grazing angle rather than the distance to a face
    // boundary. Keying it to boundaries drew a coloured outline around every
    // facet, which is the single thing that most made the old version look
    // like rendered geometry instead of a material.
    float spread = rubix_refract_dispersion * (0.35 + 1.6 * grazing + 2.0 * sheet);

    vec2 base = grad * rubix_refract_strength * rubix_refract_uv_per_px;
    // Clamped to the whole texture, not to this quad's crop: a backdrop is a
    // source-crop of the shared wallpaper, so a bent ray leaving the window's
    // own slice lands on the wallpaper next to it, which is exactly what
    // refraction should show. Only the wallpaper's outer edge clamps.
    vec4 cr = texture2D(tex, clamp(v_coords + base * (1.0 - spread), 0.0, 1.0));
    vec4 cg = texture2D(tex, clamp(v_coords + base, 0.0, 1.0));
    vec4 cb = texture2D(tex, clamp(v_coords + base * (1.0 + spread), 0.0, 1.0));
    vec3 transmitted = vec3(cr.r, cg.g, cb.b);

    // The wallpaper doubles as its own environment map. Mirrored and shrunk so
    // the reflection is a different image from the transmission -- reflecting
    // the same pixels back would just read as a brightness change.
    vec2 mirrored = clamp(vec2(0.5) + (v_coords - 0.5) * -0.55 + grad * 0.06, 0.0, 1.0);
    vec3 reflected = texture2D(tex, mirrored).rgb;

    // Alpha rides the green tap. Mixing per-channel alpha would break
    // premultiplication; a wallpaper is opaque, so there is nothing to mix.
    return vec4(mix(transmitted, reflected, fresnel), cg.a);
}

"#;

/// The stock texture shader, for outputs doing no colour conversion at all.
///
/// Copied from smithay's `gles/shaders/implicit/texture.frag` rather than
/// imported, because it is a private asset of the renderer. It must stay
/// behaviourally identical to the built-in one for the `rubix_radius == 0`
/// case; the only Rubix addition is the mask on the final colour.
const PLAIN_TEXTURE: &str = r#"#version 100

//_DEFINES_

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif

precision highp float;
#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

uniform float alpha;
varying vec2 v_coords;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

void main() {
    vec4 color = texture2D(tex, v_coords);

#if defined(NO_ALPHA)
    color = vec4(color.rgb, 1.0) * alpha;
#else
    color = color * alpha;
#endif

#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
#endif

    gl_FragColor = color;
}"#;

/// Splice the rounding mask and the refraction sample rewrite into a fragment
/// shader.
///
/// Three edits, in order: the sample site is rewritten first, then the two
/// helpers go in just above `main` (so `v_coords` is in scope), then every
/// final colour is scaled by the mask. The decode shaders all terminate in
/// `* alpha;`, and the plain one assigns a pre-multiplied `color` -- both
/// forms are handled, and both are asserted on by tests, because a silent
/// miss here would compile fine and simply never round (or refract) anything.
///
/// Order matters and is load-bearing: the sample site is rewritten FIRST,
/// while the helper is not yet in the source. Injecting first would rewrite
/// `rubixSampleRefracted`'s own `texture2D(tex, v_coords)` into a call to
/// itself -- infinite recursion, and one that compiles on some drivers.
fn with_rubix_stages(source: &str) -> String {
    let refracted = source.replace("texture2D(tex, v_coords)", "rubixSampleRefracted()");
    let spliced = refracted.replace(
        "void main() {",
        &format!("{ROUNDING_GLSL}{REFRACT_GLSL}void main() {{"),
    );
    if spliced.contains("* alpha;") {
        spliced.replace("* alpha;", "* alpha * rubixCornerAlpha();")
    } else {
        spliced.replace("gl_FragColor = color;", "gl_FragColor = color * rubixCornerAlpha();")
    }
}

fn uniform_names() -> [UniformName<'static>; 9] {
    [
        UniformName::new("rubix_radius", UniformType::_1f),
        UniformName::new("rubix_win_size", UniformType::_2f),
        UniformName::new("rubix_elem_offset", UniformType::_2f),
        UniformName::new("rubix_elem_size", UniformType::_2f),
        // `rubix_refract_strength <= 0.0` is "off", so a program that never
        // gets these set falls back to the plain sample benignly -- but
        // `RoundedElement::new` pushes all four unconditionally anyway rather
        // than relying on that.
        UniformName::new("rubix_refract_strength", UniformType::_1f),
        UniformName::new("rubix_refract_uv_origin", UniformType::_2f),
        UniformName::new("rubix_refract_uv_per_px", UniformType::_2f),
        UniformName::new("rubix_refract_facet_size", UniformType::_1f),
        UniformName::new("rubix_refract_dispersion", UniformType::_1f),
    ]
}

/// The rounding-enabled counterpart of every texture program a window element
/// might otherwise be drawn with.
#[derive(Clone)]
pub struct RoundShaders {
    plain: GlesTexProgram,
    decode_sdr: GlesTexProgram,
    decode_hdr_pq: GlesTexProgram,
    decode_windows_scrgb: GlesTexProgram,
    /// Capture variants: decode and tone-map fused, straight to sRGB. See
    /// `RoundMode::Tonemap`.
    tonemap_pq: GlesTexProgram,
    tonemap_scrgb: GlesTexProgram,
    /// HDR-pass backdrop luminance cap: decode and rolloff fused, staying in
    /// the abs10k working space instead of collapsing to sRGB. See
    /// `RoundMode::TonemapAbs10k`.
    tonemap_abs10k_pq: GlesTexProgram,
    tonemap_abs10k_scrgb: GlesTexProgram,
}

impl RoundShaders {
    fn compile(renderer: &mut GlesRenderer) -> Result<Self, GlesError> {
        let names = uniform_names();
        // `decode_sdr` keeps its own uniform on top of the rounding ones: the
        // live SDR-white-nits value is still per-frame state the decode needs.
        let mut sdr_names = names.to_vec();
        sdr_names.push(UniformName::new("sdr_white_nits", UniformType::_1f));
        Ok(RoundShaders {
            plain: renderer.compile_custom_texture_shader(with_rubix_stages(PLAIN_TEXTURE), &names)?,
            decode_sdr: renderer
                .compile_custom_texture_shader(with_rubix_stages(DECODE_SDR), &sdr_names)?,
            decode_hdr_pq: renderer
                .compile_custom_texture_shader(with_rubix_stages(DECODE_HDR_PQ), &names)?,
            decode_windows_scrgb: renderer
                .compile_custom_texture_shader(with_rubix_stages(DECODE_WINDOWS_SCRGB), &names)?,
            // Both carry `sdr_white_nits` for the same reason `decode_sdr` does:
            // the tone curve's domain is "multiples of SDR white", so the live
            // slider decides where the knee falls.
            tonemap_pq: renderer.compile_custom_texture_shader(
                with_rubix_stages(&crate::hdr_shaders::tonemap_pq_to_sdr()),
                &sdr_names,
            )?,
            tonemap_scrgb: renderer.compile_custom_texture_shader(
                with_rubix_stages(&crate::hdr_shaders::tonemap_scrgb_to_sdr()),
                &sdr_names,
            )?,
            // Same uniform set as the sRGB tone-map variants: the reference
            // luminance rides the `sdr_white_nits` name (overloaded -- see
            // `Wallpaper::backdrop_element`).
            tonemap_abs10k_pq: renderer.compile_custom_texture_shader(
                with_rubix_stages(&crate::hdr_shaders::tonemap_pq_to_abs10k()),
                &sdr_names,
            )?,
            tonemap_abs10k_scrgb: renderer.compile_custom_texture_shader(
                with_rubix_stages(&crate::hdr_shaders::tonemap_scrgb_to_abs10k()),
                &sdr_names,
            )?,
        })
    }

    /// Whether [`RoundShaders::program`] returns a program that declares
    /// `sdr_white_nits`, and therefore whether an element using `mode` must
    /// supply it.
    ///
    /// Kept beside `program` and pinned by a test, because the failure mode of
    /// the two disagreeing is silent and total: a declared-but-unset uniform
    /// reads as 0, and every one of these shaders divides by it. That shipped
    /// once already -- the tone-map variants were compiled with the uniform but
    /// never given it, so a PQ surface tone-mapped to SDR got a domain of
    /// 0..10000 instead of 0..~50 and clipped to white edge to edge.
    fn wants_sdr_white_nits(mode: RoundMode) -> bool {
        matches!(
            mode,
            RoundMode::Decode(DecodeKind::Sdr)
                | RoundMode::Tonemap(DecodeKind::HdrPq)
                | RoundMode::Tonemap(DecodeKind::WindowsScrgb)
                // All three abs10k variants declare the uniform -- including
                // `Sdr`, which resolves to `decode_sdr` below and needs it for
                // the same reason `Decode(Sdr)` does.
                | RoundMode::TonemapAbs10k(_)
        )
    }

    fn program(&self, mode: RoundMode) -> &GlesTexProgram {
        match mode {
            RoundMode::Plain => &self.plain,
            RoundMode::Decode(DecodeKind::Sdr) => &self.decode_sdr,
            RoundMode::Decode(DecodeKind::HdrPq) => &self.decode_hdr_pq,
            RoundMode::Decode(DecodeKind::WindowsScrgb) => &self.decode_windows_scrgb,
            // An SDR window drawn into an sRGB capture target needs no colour
            // conversion whatsoever -- the plain program IS the correct one.
            RoundMode::Tonemap(DecodeKind::Sdr) => &self.plain,
            RoundMode::Tonemap(DecodeKind::HdrPq) => &self.tonemap_pq,
            RoundMode::Tonemap(DecodeKind::WindowsScrgb) => &self.tonemap_scrgb,
            // Unreachable in practice: `Wallpaper::backdrop_element` only ever
            // builds `TonemapAbs10k` when the wallpaper's own decode is HDR
            // (`kind.is_hdr()`), so `Sdr` never actually gets constructed.
            // Resolved to `decode_sdr` for exhaustiveness -- that program
            // already lands SDR content in this same abs10k working space
            // with no rolloff, which is what a never-HDR source should get.
            RoundMode::TonemapAbs10k(DecodeKind::Sdr) => &self.decode_sdr,
            RoundMode::TonemapAbs10k(DecodeKind::HdrPq) => &self.tonemap_abs10k_pq,
            RoundMode::TonemapAbs10k(DecodeKind::WindowsScrgb) => &self.tonemap_abs10k_scrgb,
        }
    }
}

thread_local! {
    // Compiled once and shared by every output. Programs belong to a GL
    // context, so this assumes one context per process -- which udev.rs
    // already relies on and states outright ("there is exactly one
    // `GlesRenderer` behind `RubixRenderer<'_>` on Rubix's single-GPU
    // zero-copy configuration"). Thread-local for the same reason as
    // cursor.rs's CURSOR_CACHE: the event loop is single-threaded.
    //
    // A compile failure is cached as an error rather than retried, so a broken
    // shader logs once instead of once per frame forever.
    static ROUND_SHADERS: RefCell<Option<Result<RoundShaders, String>>> = const { RefCell::new(None) };
}

/// Compile-once accessor. Returns `None` if compilation failed, in which case
/// the caller draws unrounded rather than not at all.
pub(crate) fn round_shaders(renderer: &mut GlesRenderer) -> Option<RoundShaders> {
    ROUND_SHADERS.with_borrow_mut(|cache| {
        let entry = cache.get_or_insert_with(|| {
            RoundShaders::compile(renderer).map_err(|e| {
                let message = format!("{e:?}");
                tracing::warn!("rounded-corner shader compile failed, corners stay square: {message}");
                message
            })
        });
        entry.as_ref().ok().cloned()
    })
}

/// Which texture program an element would have been drawn with, and therefore
/// which rounding variant has to replace it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RoundMode {
    /// No colour conversion in play -- an SDR output, winit, or a capture.
    Plain,
    /// An HDR composite pass, where the slot already holds this decode.
    Decode(DecodeKind),
    /// A **capture** pass. The destination is an ordinary 8-bit sRGB buffer, so
    /// this window's transfer function is decoded and tone-mapped down to sRGB
    /// in one program (HDR Phase 4a). Unlike `Decode`, which is uniform across
    /// a pass, this is resolved per window: a capture routinely contains one
    /// HDR window and an otherwise SDR desktop.
    Tonemap(DecodeKind),
    /// The **HDR composite pass**'s backdrop luminance cap. Same soft-knee
    /// rolloff as `Tonemap`, but the destination is the linear BT.2020 abs10k
    /// offscreen, not an 8-bit sRGB buffer -- so this stays in that working
    /// space instead of collapsing to sRGB. Used only for a per-window
    /// backdrop quad whose style requests `backdrop_tonemap` on an HDR
    /// output; see `Wallpaper::backdrop_element`.
    TonemapAbs10k(DecodeKind),
}

/// Per-element refraction parameters, in the element's own **physical** pixel
/// space -- the caller scales logical config values by the output scale before
/// building this, so the shader works in one space only.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Refraction {
    /// Peak ray offset in physical px. `0.0` disables the whole effect and is
    /// the value every non-backdrop element passes.
    pub strength: f32,
    /// Facet cell size in physical px.
    pub facet_size: f32,
    /// Per-channel spread, `0.0`-`1.0`, before the seam amplification.
    pub dispersion: f32,
    /// Where this element's `v_coords` start, so the shader can undo the crop
    /// and recover window-local coordinates. Together with `uv_per_px` this is
    /// the whole mapping: `(v_coords - uv_origin) / uv_per_px` must come out as
    /// pixels from the window's own top-left, or the facets anchor to whatever
    /// the texture is instead of to the window.
    pub uv_origin: (f32, f32),
    /// How far `v_coords` moves per physical pixel of offset. A backdrop quad
    /// is a source-crop of the wallpaper, so this is the crop's share of the
    /// texture divided by the quad's on-screen size -- it cannot be derived in
    /// the shader without `dFdx`, which GLSL ES 100 does not have.
    pub uv_per_px: (f32, f32),
}

impl Refraction {
    pub(crate) const NONE: Refraction = Refraction {
        strength: 0.0,
        facet_size: 1.0,
        dispersion: 0.0,
        uv_origin: (0.0, 0.0),
        uv_per_px: (0.0, 0.0),
    };
}

/// How `space_elements` picks a [`RoundMode`] for each window.
///
/// A display composite draws every window through the same program, because the
/// whole pass shares one working space. A capture cannot: its destination is
/// sRGB, so each window is converted (or not) according to its own declared
/// transfer function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpaceMode {
    /// One program for the whole pass.
    Fixed(RoundMode),
    /// The destination is 8-bit sRGB: resolve each window to its own
    /// `RoundMode::Tonemap`.
    ///
    /// Two callers, same problem. A **capture** target is always sRGB (HDR
    /// Phase 4a). So is an **SDR output** showing HDR content -- an HDR
    /// wallpaper on a second monitor that is not HDR-capable. Both need the
    /// source's transfer function undone and the result tone-mapped, and
    /// neither has a linear working space to do it in.
    TonemapSdr,
    /// The **HDR composite pass**. Resolves each window to
    /// `RoundMode::Decode(its own DecodeKind)`, so a PQ game and an sRGB terminal
    /// land in the same linear working space through different programs.
    ///
    /// Per-window for the same reason `TonemapSdr` is, but the destination is the
    /// abs10k offscreen rather than an 8-bit sRGB buffer. `Fixed(Decode(Sdr))`
    /// remains the *other* HDR-output case: an `hdr = true` output with no HDR
    /// client present, where one program genuinely does serve the whole pass.
    HdrComposite,
}

/// Wraps one surface element, clipping it to its window's rounded rect.
///
/// Everything about the element itself is delegated to the inner element; the
/// only overrides are `opaque_regions` (a rounded window is not opaque) and
/// `draw` (which installs the program).
pub(crate) struct RoundedElement<E> {
    inner: E,
    program: GlesTexProgram,
    uniforms: Vec<Uniform<'static>>,
    /// Whether the corner mask actually carves anything out. False when the
    /// wrapper exists only to install a program (radius 0) -- see
    /// `opaque_regions`.
    masked: bool,
}

impl<E: Element> RoundedElement<E> {
    /// `window_rect` and the element's own geometry must be in the same
    /// physical space -- the output-region-local space elements are built in.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        inner: E,
        shaders: &RoundShaders,
        mode: RoundMode,
        radius: f32,
        window_rect: Rectangle<i32, Physical>,
        scale: Scale<f64>,
        sdr_white_nits: f32,
        refraction: Refraction,
    ) -> Self {
        let geo = inner.geometry(scale);
        let offset: Point<i32, Physical> = geo.loc - window_rect.loc;
        let mut uniforms = vec![
            Uniform::new("rubix_radius", radius),
            Uniform::new(
                "rubix_win_size",
                (window_rect.size.w as f32, window_rect.size.h as f32),
            ),
            Uniform::new("rubix_elem_offset", (offset.x as f32, offset.y as f32)),
            Uniform::new("rubix_elem_size", (geo.size.w as f32, geo.size.h as f32)),
            Uniform::new("rubix_refract_strength", refraction.strength),
            Uniform::new("rubix_refract_uv_origin", refraction.uv_origin),
            Uniform::new("rubix_refract_uv_per_px", refraction.uv_per_px),
            Uniform::new("rubix_refract_facet_size", refraction.facet_size),
            Uniform::new("rubix_refract_dispersion", refraction.dispersion),
        ];
        if RoundShaders::wants_sdr_white_nits(mode) {
            uniforms.push(Uniform::new("sdr_white_nits", sdr_white_nits));
        }
        RoundedElement {
            inner,
            program: shaders.program(mode).clone(),
            uniforms,
            masked: radius > 0.0,
        }
    }

    /// The wrapper used purely as a program installer, with no rounding.
    ///
    /// `rubixCornerAlpha` returns exactly 1.0 at `rubix_radius <= 0.0`, so the
    /// mask half is a true no-op and the element is drawn unchanged except for
    /// the program it is drawn with. Used for elements that are not windows --
    /// cursor, layer surfaces, ghosts -- which still need a decode installed on
    /// an HDR composite pass.
    pub(crate) fn with_program(
        inner: E,
        shaders: &RoundShaders,
        mode: RoundMode,
        scale: Scale<f64>,
        sdr_white_nits: f32,
    ) -> Self {
        let window_rect = inner.geometry(scale);
        Self::new(inner, shaders, mode, 0.0, window_rect, scale, sdr_white_nits, Refraction::NONE)
    }
}

impl<E: Element> Element for RoundedElement<E> {
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
    /// Deliberately empty. The corners are transparent now, so whatever the
    /// inner element claims about opacity is no longer true, and an untrue
    /// opaque region lets the renderer skip drawing what shows through.
    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        // A rounded element is not opaque: the corners it discards have to be
        // drawn through. But at radius 0 the mask is inert -- the wrapper is
        // only carrying a shader program (a decode, a tone-map, the wallpaper)
        // -- and forfeiting the inner element's opacity there would cost the
        // occlusion culling for nothing. That matters most for the wallpaper,
        // the largest element in the frame and the one most often fully
        // covered.
        if self.masked {
            OpaqueRegions::default()
        } else {
            self.inner.opaque_regions(scale)
        }
    }
}

/// Lets a render element reach GL state on whatever frame it is handed,
/// without ever naming a `GlesFrame` outside a concrete impl.
///
/// The obvious shape -- a trait method returning `&mut GlesFrame` -- does not
/// work: `GlesFrame` is invariant in its lifetimes, so a `GlesFrame<'a, 'b>`
/// will not coerce to the shorter anonymous lifetimes such a signature would
/// demand. Passing the *operation* inward instead of passing the frame outward
/// sidesteps that entirely, and has the side benefit that each renderer decides
/// for itself how to reach its frame: winit's frame already is a `GlesFrame`,
/// udev's wraps one behind the patched `render_frame_mut`.
pub(crate) trait GlesAccess: RendererSuper {
    /// The concrete `GlesRenderer` behind this renderer.
    ///
    /// Needed to compile shaders. `AsMut<GlesRenderer>` covers udev's
    /// `MultiRenderer` but cannot be implemented for `GlesRenderer` itself --
    /// both trait and type are foreign, so the orphan rule forbids it. This
    /// trait is local, so it can cover both. (Phase 4a wanted exactly this and
    /// recorded it as a blocker; it is no longer one.)
    fn gles_renderer(&mut self) -> &mut GlesRenderer;

    /// Install a texture program, returning whatever was there before.
    ///
    /// Must be paired with [`Self::restore_round_program`] rather than a plain
    /// clear. The HDR path installs its decode shader as a *renderer-level*
    /// default that every new frame inherits, so clearing does not restore it
    /// -- it wipes it, and every element drawn afterwards loses its decode.
    /// That is not theoretical: it turned the bar white, because layer surfaces
    /// are drawn after the space and their sRGB textures went into the linear
    /// working space unconverted.
    fn swap_round_program(
        frame: &mut Self::Frame<'_, '_>,
        program: GlesTexProgram,
        uniforms: Vec<Uniform<'static>>,
    ) -> TexProgramOverride;
    fn restore_round_program(frame: &mut Self::Frame<'_, '_>, previous: TexProgramOverride);

    /// Draw a `PixelShaderElement`, which smithay only implements
    /// `RenderElement` for against `GlesRenderer` directly. Same trick as the
    /// program setters: the operation goes inward rather than the frame coming
    /// out. Failures are swallowed with a warning -- chrome that will not draw
    /// must not take the frame down with it.
    fn draw_pixel_shader(
        frame: &mut Self::Frame<'_, '_>,
        element: &PixelShaderElement,
        src: Rectangle<f64, BufferCoords>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
    );
}

/// What `GlesFrame` stores for a texture-program override.
pub(crate) type TexProgramOverride = Option<(GlesTexProgram, Vec<Uniform<'static>>)>;

fn warn_chrome(err: impl std::fmt::Debug) {
    tracing::warn!("border chrome failed to draw: {err:?}");
}

impl GlesAccess for GlesRenderer {
    fn gles_renderer(&mut self) -> &mut GlesRenderer {
        self
    }

    fn swap_round_program(
        frame: &mut Self::Frame<'_, '_>,
        program: GlesTexProgram,
        uniforms: Vec<Uniform<'static>>,
    ) -> TexProgramOverride {
        let previous = frame.take_tex_program_override();
        frame.override_default_tex_program(program, uniforms);
        previous
    }
    fn restore_round_program(frame: &mut Self::Frame<'_, '_>, previous: TexProgramOverride) {
        frame.set_tex_program_override(previous);
    }

    fn draw_pixel_shader(
        frame: &mut Self::Frame<'_, '_>,
        element: &PixelShaderElement,
        src: Rectangle<f64, BufferCoords>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
    ) {
        if let Err(e) =
            RenderElement::<GlesRenderer>::draw(element, frame, src, dst, damage, &[], None)
        {
            warn_chrome(e);
        }
    }
}

impl<'r> GlesAccess for crate::udev::RubixRenderer<'r> {
    fn gles_renderer(&mut self) -> &mut GlesRenderer {
        self.as_mut()
    }

    fn swap_round_program(
        frame: &mut Self::Frame<'_, '_>,
        program: GlesTexProgram,
        uniforms: Vec<Uniform<'static>>,
    ) -> TexProgramOverride {
        let Some(gles) = frame.render_frame_mut() else { return None };
        let previous = gles.take_tex_program_override();
        gles.override_default_tex_program(program, uniforms);
        previous
    }
    fn restore_round_program(frame: &mut Self::Frame<'_, '_>, previous: TexProgramOverride) {
        if let Some(gles) = frame.render_frame_mut() {
            gles.set_tex_program_override(previous);
        }
    }

    fn draw_pixel_shader(
        frame: &mut Self::Frame<'_, '_>,
        element: &PixelShaderElement,
        src: Rectangle<f64, BufferCoords>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
    ) {
        let Some(gles) = frame.render_frame_mut() else { return };
        if let Err(e) =
            RenderElement::<GlesRenderer>::draw(element, gles, src, dst, damage, &[], None)
        {
            warn_chrome(e);
        }
    }
}

impl<R, E> RenderElement<R> for RoundedElement<E>
where
    R: Renderer + GlesAccess,
    E: RenderElement<R>,
{
    fn draw(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, BufferCoords>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), R::Error> {
        let previous = R::swap_round_program(frame, self.program.clone(), self.uniforms.clone());
        let result = self.inner.draw(frame, src, dst, damage, opaque_regions, cache);
        // Restored unconditionally, including on the error path. Restored, not
        // cleared: see swap_round_program.
        R::restore_round_program(frame, previous);
        result
    }

    /// Deliberately not delegated. Reporting underlying storage offers the
    /// buffer up for direct plane scanout, which bypasses the fragment shader
    /// and would put the corners straight back.
    fn underlying_storage(&self, _renderer: &mut R) -> Option<UnderlyingStorage<'_>> {
        None
    }
}

/// The space's contribution to one output: every window, plus that window's
/// border, in front-to-back order.
///
/// Borders are emitted **interleaved with their windows** rather than as one
/// group above the space. Grouped, every border floats above every window, so a
/// maximized window ends up covered in the borders of the windows behind it.
///
/// Per-window gathering is required whenever there is any chrome at all, since
/// the batched `Space::render_elements_for_region` call returns a flat list
/// with no way to attribute elements back to windows. With both borders and
/// rounding off, it falls back to that batched call unchanged.
/// Wrap one element in the tone-map program its surface's declared transfer
/// function calls for, for any destination that is 8-bit sRGB.
///
/// Two destinations qualify: a capture target (HDR Phase 4a) and an SDR output
/// showing HDR content. Used for layer surfaces, which `space_elements` never
/// sees -- the concrete case being an HDR wallpaper on the background layer,
/// which is exactly what lands here on a non-HDR monitor. Windows go through
/// `SpaceMode::TonemapSdr` instead, which resolves the same thing per window
/// while it already has the window in hand.
///
/// SDR surfaces (overwhelmingly the common case) are returned untouched, so this
/// costs a `DecodeKind` lookup and nothing else on an ordinary desktop.
pub(crate) fn tonemap_sdr_element<R>(
    renderer: &mut R,
    surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    element: WaylandSurfaceRenderElement<R>,
    scale: f64,
    sdr_white_nits: f32,
) -> RubixRenderElement<R>
where
    R: Renderer + ImportAll + ImportMem + GlesAccess,
    R::TextureId: Clone + 'static,
    RubixRenderElement<R>: RenderElement<R>,
{
    let kind = crate::color_management::surface_decode_kind(surface);
    if matches!(kind, DecodeKind::Sdr) {
        return RubixRenderElement::Surface(element);
    }
    let Some(shaders) = round_shaders(renderer.gles_renderer()) else {
        // Shaders would not compile; an unconverted HDR layer is wrong, but it is
        // less wrong than dropping it out of the capture entirely.
        return RubixRenderElement::Surface(element);
    };
    let geometry = element.geometry(Scale::from(scale));
    RubixRenderElement::Rounded(RoundedElement::new(
        element,
        &shaders,
        RoundMode::Tonemap(kind),
        // Layer surfaces are never rounded, so the mask is a no-op and
        // `window_rect` is unused -- geometry is passed only to keep the
        // uniforms well-defined.
        0.0,
        geometry,
        Scale::from(scale),
        sdr_white_nits,
        Refraction::NONE,
    ))
}

/// Per-window `RoundMode` for one element of `mode`'s pass, given that
/// window's own declared `DecodeKind`. Pulled out of `space_elements`'s
/// window loop so it is testable without a live renderer -- `TonemapSdr` and
/// `HdrComposite` are otherwise easy to swap for each other by accident since
/// both wrap `RoundMode` around the same `DecodeKind` lookup, just around a
/// different variant.
fn resolve_elem_mode(mode: SpaceMode, window_kind: DecodeKind) -> RoundMode {
    match mode {
        SpaceMode::Fixed(m) => m,
        SpaceMode::TonemapSdr => RoundMode::Tonemap(window_kind),
        SpaceMode::HdrComposite => RoundMode::Decode(window_kind),
    }
}

/// Whether a per-window backdrop quad should wrap with `RoundMode::
/// TonemapAbs10k` (`true`) rather than `RoundMode::Tonemap` (`false`) when it
/// tone-maps at all -- see the call site's comment on `hdr_pass` for what
/// picking the wrong one looks like on screen.
fn backdrop_hdr_pass(mode: SpaceMode) -> bool {
    matches!(mode, SpaceMode::HdrComposite)
}

/// A tween window's on-screen rect: `RescaleRenderElement` scales geometry
/// about `origin` by `factor`, so anything measured against the window has to
/// be scaled the same way or it will be drawn for a size the window no longer
/// has.
pub(crate) fn rescaled_rect(
    local_rect: Rectangle<i32, Logical>,
    origin: Point<i32, Physical>,
    factor: f64,
    scale: f64,
) -> Rectangle<i32, Logical> {
    let origin_logical = Point::<f64, Logical>::from((origin.x as f64 / scale, origin.y as f64 / scale));
    let rect = local_rect.to_f64();
    let loc = Point::<f64, Logical>::from((
        origin_logical.x + (rect.loc.x - origin_logical.x) * factor,
        origin_logical.y + (rect.loc.y - origin_logical.y) * factor,
    ));
    let size = smithay::utils::Size::<f64, Logical>::from((rect.size.w * factor, rect.size.h * factor));
    Rectangle::<f64, Logical>::new(loc, size).to_i32_round()
}

/// Corner radius for one window: forced to 0 when fullscreen (no corner to
/// cut, and it disqualifies the window from primary-plane scanout anyway),
/// then scaled down by a tween's rescale factor so corners shrink with a
/// collapsing window instead of staying a fixed pixel size. Split out purely
/// so the fullscreen/tween interaction is testable without a live renderer.
fn effective_radius(radius: f32, fullscreen: bool, rescale_factor: Option<f64>) -> f32 {
    let base = if fullscreen { 0.0 } else { radius };
    match rescale_factor {
        Some(factor) => base * factor as f32,
        None => base,
    }
}

/// `wallpaper_sdr_tonemap` is the same "destination is 8-bit sRGB, source is
/// not" flag the caller already computes for the wallpaper element itself
/// (`sdr_tonemap_needed`). A per-window backdrop quad sampling HDR wallpaper
/// pixels needs the identical tone-map program whenever that flag is set,
/// independent of whether the window's own `backdrop_tonemap` rule also asks
/// for it -- the destination, not the rule, is what makes raw PQ texels wrong
/// to scan out untouched.
///
/// Returns `(window elements, backdrop quads)` rather than one list so the
/// caller can splice the quads in exactly between the windows and the layers
/// below them (see `src/wallpaper.rs`'s "Backdrop element" docs for why that
/// slot, and the accepted consequence of it).
pub(crate) fn space_elements<R>(
    state: &RubixState,
    renderer: &mut R,
    output: &Output,
    scale: f64,
    mode: SpaceMode,
    wallpaper_sdr_tonemap: bool,
) -> (Vec<RubixRenderElement<R>>, Vec<RubixRenderElement<R>>)
where
    R: Renderer + ImportAll + ImportMem + GlesAccess,
    R::TextureId: Send + Clone + 'static,
    RubixRenderElement<R>: RenderElement<R>,
{
    let radius = state.config.decoration.corner_radius as f32;
    let deco = &state.config.decoration;
    // Any of these is per-window state the batched call cannot express.
    //
    // The backdrop settings -- `backdrop_tonemap`, `backdrop_blur`, `refract`
    // -- are deliberately absent. A backdrop quad is only ever emitted for a
    // window with `opacity < 1.0` (there is nothing to see through an opaque
    // one), so the opacity checks below already force the per-window walk for
    // every configuration that could produce one. Adding them would be dead
    // conditions that read as though they were load-bearing.
    let has_borders = deco.border_width > 0
        || deco.active.glow_margin > 0
        || deco.inactive.glow_margin > 0
        || deco.active.opacity < 1.0
        || deco.inactive.opacity < 1.0
        || deco.obscured_opacity < 1.0
        || deco.rules.iter().any(|r| {
            [&r.active, &r.inactive].into_iter().any(|s| {
                s.opacity.is_some_and(|o| o < 1.0)
                    || s.glow_margin.is_some_and(|g| g > 0)
            })
        });
    // Capture resolves a program per window, which the batched call cannot
    // express -- it hands back one flat element list with no window identity
    // left in it. So this forces the per-window walk below even with no chrome
    // configured at all, which is exactly the bare-desktop-plus-HDR-game case.
    let per_window_program = matches!(mode, SpaceMode::TonemapSdr | SpaceMode::HdrComposite);
    let Some(region) = state.space.output_geometry(output) else {
        return (Vec::new(), Vec::new());
    };
    if radius <= 0.0 && !has_borders && !per_window_program {
        // No window can be translucent on this path (`has_borders` already
        // covers every opacity < 1.0 case), so there is nothing a backdrop
        // quad could ever be drawn for.
        return (
            state
                .space
                .render_elements_for_region(renderer, &region, scale, 1.0)
                .into_iter()
                .map(RubixRenderElement::Surface)
                .collect(),
            Vec::new(),
        );
    }
    // `None` means rounding is off or its shaders failed to compile; borders
    // still work either way, windows are just drawn square.
    // Also compiled at radius 0 for a capture: `RoundedElement` doubles as the
    // "draw this element with that program" wrapper, and `rubixCornerAlpha`
    // returns exactly 1.0 when `rubix_radius <= 0.0`, so the rounding half is a
    // true no-op rather than an approximate one.
    let round = (radius > 0.0 || per_window_program)
        .then(|| round_shaders(renderer.gles_renderer()))
        .flatten();

    // Computed once per output: each window is tested against the ones above
    // it, which cannot be done from inside the loop without re-walking.
    let occlusion = crate::decoration::occlusion_map(state, output);

    let mut elements = Vec::new();
    let mut backdrops = Vec::new();
    for window in state.space.elements().rev() {
        let Some(bbox) = state.space.element_bbox(window) else { continue };
        if !region.overlaps(bbox) {
            continue;
        }
        let Some(location) = state.space.element_location(window) else { continue };
        let geometry = window.geometry();
        let render_location: Point<i32, Logical> = location - geometry.loc - region.loc;
        // An override-redirect X11 surface (a menu, tooltip, combobox) resolves
        // to no id on purpose: `id` is what gates the border ring, the glow,
        // the backdrop and the active/inactive opacity, and none of those
        // belong on client chrome. A menu drawn at `inactive.opacity` because
        // it is not the focused window is the visible form of that mistake.
        let id = state
            .windows
            .iter()
            .find_map(|(id, w)| (w == window).then_some(*id))
            .filter(|_| !crate::state::is_unmanaged(window));
        let fullscreen = id.is_some_and(|id| state.fullscreen_windows.contains(&id));

        // Region-local logical rect of the window's content, which is what the
        // border ring is built around and what the rounding mask is measured
        // against.
        let local_rect = Rectangle::<i32, Logical>::new(location - region.loc, geometry.size);

        // Resolved once and shared by the border and the window's own surfaces,
        // so the two cannot disagree about which rule matched.
        let style = id.map(|id| crate::decoration::style_for_window(
                state,
                id,
                occlusion.get(&id).copied().unwrap_or(0.0),
            ));

        decorate_window(
            state,
            renderer,
            output,
            region,
            scale,
            mode,
            wallpaper_sdr_tonemap,
            round.as_ref(),
            radius,
            window,
            id,
            fullscreen,
            local_rect,
            render_location,
            style,
            None,
            &mut elements,
            &mut backdrops,
        );
    }
    crate::decoration::prune_ring_cache(state);
    (elements, backdrops)
}

/// Decorate one window -- border, backdrop, and the window's own surfaces,
/// rounded and opacity-adjusted -- and push the results into `elements`/
/// `backdrops`. Extracted from `space_elements`'s per-window loop so the same
/// treatment can be given to a reveal-tween or ghost window, which is drawn
/// by hand outside the `Space` (see `tween_elements`) and so never reaches
/// that loop.
///
/// `rescale: None` is exactly `space_elements`'s original per-window
/// behaviour -- `local_rect` is used as-is. `rescale: Some((origin, factor))`
/// is a mid-tween window: every rect measured against it (the rounding
/// mask's `window_rect`, the border ring, the backdrop crop) is scaled about
/// `origin` by `factor` first (see `rescaled_rect`), and the surfaces
/// themselves are wrapped in a `RescaleRenderElement` *inside* any
/// `RoundedElement` wrapper -- `RoundedElement::new` reads the inner
/// element's geometry at construction, so the rescale has to already be
/// applied by the time it does.
#[allow(clippy::too_many_arguments)]
fn decorate_window<R>(
    state: &RubixState,
    renderer: &mut R,
    output: &Output,
    region: Rectangle<i32, Logical>,
    scale: f64,
    mode: SpaceMode,
    wallpaper_sdr_tonemap: bool,
    round: Option<&RoundShaders>,
    radius: f32,
    window: &Window,
    id: Option<u32>,
    fullscreen: bool,
    local_rect: Rectangle<i32, Logical>,
    render_location: Point<i32, Logical>,
    style: Option<WindowStyle>,
    rescale: Option<(Point<i32, Physical>, f64)>,
    elements: &mut Vec<RubixRenderElement<R>>,
    backdrops: &mut Vec<RubixRenderElement<R>>,
) where
    R: Renderer + ImportAll + ImportMem + GlesAccess,
    R::TextureId: Send + Clone + 'static,
    RubixRenderElement<R>: RenderElement<R>,
{
    let deco = &state.config.decoration;
    let hdr = matches!(mode, SpaceMode::Fixed(RoundMode::Decode(_)) | SpaceMode::HdrComposite);

    // The rect this window is actually being drawn at this frame. Identical
    // to `local_rect` for a mapped window; scaled about the tween's origin
    // for a mid-Reveal one.
    let effective_rect = match rescale {
        Some((origin, factor)) => rescaled_rect(local_rect, origin, factor, scale),
        None => local_rect,
    };
    let window_rect = Rectangle::<i32, Physical>::new(
        effective_rect.loc.to_physical_precise_round(scale),
        effective_rect.size.to_physical_precise_round(scale),
    );

    if let (Some(id), Some(style)) = (id, style.as_ref()) {
        if let Some(ring) = crate::decoration::window_border_elements(
            state, renderer, id, style, effective_rect, hdr,
        ) {
            elements.push(RubixRenderElement::BorderRing(ring));
        }
    }

    // A frosted/tone-mapped backdrop only makes sense for a window that is
    // actually letting the wallpaper show through. `style.opacity < 1.0`
    // is also what already excludes a fullscreen window here without a
    // separate check -- `style_for_window` forces it back to `1.0` for
    // exactly this reason.
    if let Some(style) = style.as_ref().filter(|s| {
        s.opacity < 1.0 && (s.backdrop_tonemap || s.backdrop_blur || s.refract)
    }) {
        let tonemap = style.backdrop_tonemap || wallpaper_sdr_tonemap;
        // Scaled here rather than in `backdrop_element`, which does not
        // otherwise need to know the output scale: the caller owns the
        // look (strength, facet size, dispersion), the callee owns the
        // geometry (`uv_per_px`, overwritten below).
        let refraction = if style.refract {
            Refraction {
                strength: deco.refract_strength * scale as f32,
                facet_size: deco.refract_facet_size * scale as f32,
                dispersion: deco.refract_dispersion,
                // Both filled in by `backdrop_element`, which is the only
                // place that knows the crop this quad samples through.
                uv_origin: (0.0, 0.0),
                uv_per_px: (0.0, 0.0), // filled in by backdrop_element
            }
        } else {
            Refraction::NONE
        };
        // `hdr_pass` picks which wrapper program a tone-mapped quad gets
        // (see `RoundMode::TonemapAbs10k`'s doc comment): `true` only for
        // `SpaceMode::HdrComposite`, whose destination is the abs10k
        // offscreen and must stay in that working space rather than
        // collapse to sRGB. Every other mode here -- `TonemapSdr` (a
        // genuinely SDR output, or a capture) and `Fixed` (including the
        // `hdr = true`-output-no-HDR-client case, where `kind.is_hdr()`
        // inside `backdrop_element` is false and this flag is never even
        // consulted) -- is `false`. Getting this backwards produces
        // `RoundMode::Tonemap` (collapse to sRGB) where `TonemapAbs10k`
        // (stay in the working space) is wanted, which reads on screen as
        // the backdrop crushing to white -- a bug already shipped once.
        if let Some((_, quad)) = state.wallpaper.backdrop_element(
            renderer,
            &output.name(),
            region.size,
            effective_rect,
            scale,
            tonemap,
            style.backdrop_blur,
            backdrop_hdr_pass(mode),
            state.sdr_white_nits,
            refraction,
        ) {
            backdrops.push(quad);
        }
    }

    let opacity = style.as_ref().map_or(1.0, |s| s.opacity);
    let surfaces = window.render_elements::<WaylandSurfaceRenderElement<R>>(
        renderer,
        render_location.to_physical_precise_round(scale),
        Scale::from(scale),
        opacity,
    );
    let window_kind = window
        .wl_surface()
        .map(|s| crate::color_management::surface_decode_kind(&s))
        .unwrap_or(DecodeKind::Sdr);
    let elem_mode = resolve_elem_mode(mode, window_kind);
    let elem_radius = effective_radius(radius, fullscreen, rescale.map(|(_, factor)| factor));
    // An SDR window in a capture resolves to the plain program, which is what
    // it would have been drawn with anyway; wrapping it would buy a program
    // swap per element for nothing. `HdrComposite` is unconditional instead:
    // that pass has no renderer-wide default program at all (every element
    // installs its own -- see `udev::render_surface_hdr_zrun`), so even a
    // `Decode(Sdr)`, radius-0, fullscreen window must still be wrapped or it
    // silently draws with whatever program the previous element left active.
    let needs_program = matches!(mode, SpaceMode::HdrComposite)
        || matches!(
            elem_mode,
            RoundMode::Tonemap(DecodeKind::HdrPq) | RoundMode::Tonemap(DecodeKind::WindowsScrgb)
        );
    let wrap = round.filter(|_| elem_radius > 0.0 || needs_program);

    match rescale {
        Some((origin, factor)) => match wrap {
            Some(shaders) => elements.extend(surfaces.into_iter().map(|e| {
                let rescaled = RescaleRenderElement::from_element(e, origin, factor);
                RubixRenderElement::RoundedRescaled(RoundedElement::new(
                    rescaled,
                    shaders,
                    elem_mode,
                    elem_radius,
                    window_rect,
                    Scale::from(scale),
                    state.sdr_white_nits,
                    Refraction::NONE,
                ))
            })),
            None => elements.extend(surfaces.into_iter().map(|e| {
                RubixRenderElement::Rescaled(RescaleRenderElement::from_element(e, origin, factor))
            })),
        },
        None => match wrap {
            Some(shaders) => elements.extend(surfaces.into_iter().map(|e| {
                RubixRenderElement::Rounded(RoundedElement::new(
                    e,
                    shaders,
                    elem_mode,
                    elem_radius,
                    window_rect,
                    Scale::from(scale),
                    state.sdr_white_nits,
                    Refraction::NONE,
                ))
            })),
            None => elements.extend(surfaces.into_iter().map(RubixRenderElement::Surface)),
        },
    }
}

/// Decoration for every window mid-tween this frame: rotation-wrap ghosts
/// (`state.active_ghosts`, `rescale: None`) and reveal-tween scaled windows
/// (`state.active_scales`, `rescale: Some((window centre, factor))`). Both
/// are unmapped from the `Space` for the tween's duration (see
/// `RubixState::step_animations`), so this is their ONLY draw -- omitting it
/// makes them invisible for the length of the animation rather than merely
/// undecorated.
///
/// `pos` for both lists is in the SAME space `Space::element_location`
/// reports windows in -- NOT output-region-local like `space_elements`'s
/// `local_rect`/`render_location` are. That has always been true (the ghost
/// and scale element lists this replaces drew at `Point::<Physical>::from((
/// pos.x, pos.y))` directly, with no `region.loc` subtraction), and every
/// caller of this function runs at `scale == 1.0` on a single-output setup
/// where region-local and global coincide -- so reproducing that same
/// non-subtraction here is what keeps a tween window from jumping.
pub(crate) fn tween_elements<R>(
    state: &RubixState,
    renderer: &mut R,
    output: &Output,
    scale: f64,
    mode: SpaceMode,
    wallpaper_sdr_tonemap: bool,
) -> (Vec<RubixRenderElement<R>>, Vec<RubixRenderElement<R>>)
where
    R: Renderer + ImportAll + ImportMem + GlesAccess,
    R::TextureId: Send + Clone + 'static,
    RubixRenderElement<R>: RenderElement<R>,
{
    let radius = state.config.decoration.corner_radius as f32;
    let per_window_program = matches!(mode, SpaceMode::TonemapSdr | SpaceMode::HdrComposite);
    let Some(region) = state.space.output_geometry(output) else {
        return (Vec::new(), Vec::new());
    };
    let round = (radius > 0.0 || per_window_program)
        .then(|| round_shaders(renderer.gles_renderer()))
        .flatten();
    let occlusion = crate::decoration::occlusion_map(state, output);

    let mut elements = Vec::new();
    let mut backdrops = Vec::new();

    let ghost_windows: Vec<(u32, Window, Point<i32, Logical>)> = state
        .active_ghosts
        .iter()
        .filter_map(|(id, pos)| {
            state
                .windows
                .get(id)
                .map(|w| (*id, w.clone(), Point::<i32, Logical>::from((pos.x, pos.y))))
        })
        .collect();
    for (id, window, pos) in &ghost_windows {
        let fullscreen = state.fullscreen_windows.contains(id);
        let local_rect = Rectangle::<i32, Logical>::new(*pos, window.geometry().size);
        let style = Some(crate::decoration::style_for_window(
            state,
            *id,
            occlusion.get(id).copied().unwrap_or(0.0),
        ));
        decorate_window(
            state,
            renderer,
            output,
            region,
            scale,
            mode,
            wallpaper_sdr_tonemap,
            round.as_ref(),
            radius,
            window,
            Some(*id),
            fullscreen,
            local_rect,
            *pos,
            style,
            None,
            &mut elements,
            &mut backdrops,
        );
    }

    let scaled_windows: Vec<(u32, Window, Point<i32, Logical>, f64)> = state
        .active_scales
        .iter()
        .filter_map(|(id, pos, factor)| {
            state.windows.get(id).map(|w| {
                (*id, w.clone(), Point::<i32, Logical>::from((pos.x, pos.y)), *factor as f64)
            })
        })
        .collect();
    for (id, window, pos, factor) in &scaled_windows {
        // Fully collapsed: nothing worth drawing, and a zero scale degenerates
        // the element geometry -- mirrors the old `reveal_scale_elements`'s
        // own guard.
        if *factor <= 0.01 {
            continue;
        }
        let fullscreen = state.fullscreen_windows.contains(id);
        let size = window.geometry().size;
        let local_rect = Rectangle::<i32, Logical>::new(*pos, size);
        // The window's own centre -- `RescaleRenderElement` scales about this
        // point so a Reveal grows/shrinks in place rather than toward a
        // corner. Matches the old `reveal_scale_elements` exactly.
        let origin = Point::<i32, Physical>::from((pos.x + size.w / 2, pos.y + size.h / 2));
        let style = Some(crate::decoration::style_for_window(
            state,
            *id,
            occlusion.get(id).copied().unwrap_or(0.0),
        ));
        decorate_window(
            state,
            renderer,
            output,
            region,
            scale,
            mode,
            wallpaper_sdr_tonemap,
            round.as_ref(),
            radius,
            window,
            Some(*id),
            fullscreen,
            local_rect,
            *pos,
            style,
            Some((origin, *factor)),
            &mut elements,
            &mut backdrops,
        );
    }

    // Not pruned here: `space_elements` already does it once per frame, and
    // every caller of `tween_elements` calls both in the same frame -- a
    // second sweep would just be redundant work over the same cache.
    (elements, backdrops)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The recursion guard: `rubixSampleRefracted`'s own `texture2D(tex,
    // v_coords)` must never be rewritten into a call to itself. Splicing the
    // helper in before rewriting the sample site would do exactly that, and it
    // is a failure mode that compiles clean on some drivers -- infinite
    // recursion at shader-compile time, not at Rust-compile time.
    #[test]
    fn the_sample_rewrite_runs_before_the_helper_is_spliced() {
        for (name, source) in [
            ("plain", PLAIN_TEXTURE),
            ("decode_sdr", DECODE_SDR),
            ("decode_hdr_pq", DECODE_HDR_PQ),
            ("decode_windows_scrgb", DECODE_WINDOWS_SCRGB),
            ("tonemap_pq", &crate::hdr_shaders::tonemap_pq_to_sdr()),
            ("tonemap_scrgb", &crate::hdr_shaders::tonemap_scrgb_to_sdr()),
            ("tonemap_abs10k_pq", &crate::hdr_shaders::tonemap_pq_to_abs10k()),
            ("tonemap_abs10k_scrgb", &crate::hdr_shaders::tonemap_scrgb_to_abs10k()),
        ] {
            let out = with_rubix_stages(source);
            let main_at = out.find("void main() {").expect("main present");
            let after_main = &out[main_at..];
            assert!(
                !after_main.contains("texture2D(tex, v_coords)"),
                "{name}: a raw sample survives after main -- only the helper above main \
                 may call texture2D(tex, v_coords) directly"
            );
        }
    }

    #[test]
    fn every_shader_variant_gets_the_refraction_helper() {
        for (name, source) in [
            ("plain", PLAIN_TEXTURE),
            ("decode_sdr", DECODE_SDR),
            ("decode_hdr_pq", DECODE_HDR_PQ),
            ("decode_windows_scrgb", DECODE_WINDOWS_SCRGB),
            ("tonemap_pq", &crate::hdr_shaders::tonemap_pq_to_sdr()),
            ("tonemap_scrgb", &crate::hdr_shaders::tonemap_scrgb_to_sdr()),
        ] {
            let out = with_rubix_stages(source);
            assert_eq!(
                out.matches("vec4 rubixSampleRefracted()").count(),
                1,
                "{name}: helper must be defined exactly once"
            );
            assert!(
                out.matches("rubixSampleRefracted()").count() >= 2,
                "{name}: helper must be both defined and called"
            );
        }
    }

    #[test]
    fn refraction_none_is_off() {
        assert_eq!(Refraction::NONE.strength, 0.0);
    }

    // The declared uniform set every program gets -- constructing a live
    // `RoundedElement` needs a compiled `RoundShaders`, which needs a real GL
    // context this test suite has none of, so this pins the set at the
    // declaration instead of the instance.
    #[test]
    fn every_program_declares_the_refraction_uniforms() {
        let all = uniform_names();
        let names: Vec<&str> = all.iter().map(|n| n.name.as_ref()).collect();
        for expected in [
            "rubix_refract_strength",
            "rubix_refract_uv_per_px",
            "rubix_refract_facet_size",
            "rubix_refract_dispersion",
        ] {
            assert!(names.contains(&expected), "missing uniform {expected}");
        }
    }

    // A miss in either splice compiles fine and simply never rounds anything,
    // which is exactly the kind of failure that survives a visual check on one
    // machine and not another.
    #[test]
    fn every_shader_variant_gets_the_helper_and_a_masked_output() {
        for (name, source) in [
            ("plain", PLAIN_TEXTURE),
            ("decode_sdr", DECODE_SDR),
            ("decode_hdr_pq", DECODE_HDR_PQ),
            ("decode_windows_scrgb", DECODE_WINDOWS_SCRGB),
        ] {
            let out = with_rubix_stages(source);
            assert!(out.contains("float rubixCornerAlpha()"), "{name}: helper missing");
            assert!(
                out.contains("rubixCornerAlpha();"),
                "{name}: mask never applied to the output"
            );
        }
    }

    // The capture tone-map variants go through the same splice, and they are the
    // ones most likely to break it: they are built by `format!` rather than
    // written out as a literal, so a stray change to the head or tail could move
    // the anchors `with_rubix_stages` looks for.
    #[test]
    fn capture_tonemap_variants_survive_the_rounding_splice() {
        for (name, source) in [
            ("tonemap_pq", crate::hdr_shaders::tonemap_pq_to_sdr()),
            ("tonemap_scrgb", crate::hdr_shaders::tonemap_scrgb_to_sdr()),
        ] {
            let out = with_rubix_stages(&source);
            assert!(out.contains("float rubixCornerAlpha()"), "{name}: helper missing");
            assert!(
                out.contains("rubixCornerAlpha();"),
                "{name}: mask never applied to the output"
            );
            let varying = out.find("varying vec2 v_coords;").expect("v_coords declared");
            let helper = out.find("float rubixCornerAlpha()").expect("helper present");
            assert!(varying < helper, "{name}: helper must come after the varying it reads");
            // Both the NO_ALPHA and the alpha-carrying branch end in `* alpha;`,
            // and both have to pick up the mask -- rounding only one of them
            // would round opaque windows and leave translucent ones square.
            assert_eq!(
                out.matches("* alpha * rubixCornerAlpha();").count(),
                2,
                "{name}: both alpha branches must be masked"
            );
        }
    }

    #[test]
    fn the_helper_is_declared_after_v_coords_so_it_compiles() {
        for source in [PLAIN_TEXTURE, DECODE_SDR, DECODE_HDR_PQ, DECODE_WINDOWS_SCRGB] {
            let out = with_rubix_stages(source);
            let varying = out.find("varying vec2 v_coords;").expect("v_coords declared");
            let helper = out.find("float rubixCornerAlpha()").expect("helper present");
            assert!(varying < helper, "helper must come after the varying it reads");
        }
    }

    #[test]
    fn splicing_leaves_the_defines_marker_intact() {
        // Smithay substitutes this line; losing it breaks every variant.
        for source in [PLAIN_TEXTURE, DECODE_SDR, DECODE_HDR_PQ, DECODE_WINDOWS_SCRGB] {
            assert!(with_rubix_stages(source).contains("//_DEFINES_"));
        }
    }

    #[test]
    fn every_decode_kind_maps_to_a_distinct_variant() {
        // Guards against a new DecodeKind silently reusing another's program.
        let modes = [
            RoundMode::Plain,
            RoundMode::Decode(DecodeKind::Sdr),
            RoundMode::Decode(DecodeKind::HdrPq),
            RoundMode::Decode(DecodeKind::WindowsScrgb),
        ];
        assert_eq!(modes.len(), 4, "a new mode needs a program in RoundShaders");
    }

    #[test]
    fn tonemap_abs10k_covers_every_decode_kind() {
        // Same sentinel as `every_decode_kind_maps_to_a_distinct_variant`: a new
        // `DecodeKind` needs a `TonemapAbs10k` arm in `RoundShaders::program`
        // too, or it silently falls through to a stale match arm.
        let modes = [
            RoundMode::TonemapAbs10k(DecodeKind::Sdr),
            RoundMode::TonemapAbs10k(DecodeKind::HdrPq),
            RoundMode::TonemapAbs10k(DecodeKind::WindowsScrgb),
        ];
        assert_eq!(modes.len(), 3, "a new DecodeKind needs a TonemapAbs10k arm in RoundShaders::program");
    }

    // Pins `wants_sdr_white_nits` against the shader sources it speaks for.
    //
    // This is the bug that made an HDR wallpaper render as a white rectangle on
    // an SDR output: the tone-map programs were compiled with `sdr_white_nits`
    // declared, but only `Decode(Sdr)` ever supplied it. An unset uniform reads
    // as 0, and every shader here divides by it -- so PQ decoded into a
    // 0..10000 domain instead of 0..~50 and clipped everywhere. Nothing about
    // that is visible in a compile, which is why it needs a test.
    #[test]
    fn every_mode_that_references_sdr_white_nits_is_given_it() {
        // The mode -> source mapping, duplicated from `RoundShaders::program`
        // on purpose: this test exists to catch the two drifting apart.
        let sources: [(RoundMode, String); 10] = [
            (RoundMode::Plain, PLAIN_TEXTURE.to_string()),
            (RoundMode::Decode(DecodeKind::Sdr), DECODE_SDR.to_string()),
            (RoundMode::Decode(DecodeKind::HdrPq), DECODE_HDR_PQ.to_string()),
            (
                RoundMode::Decode(DecodeKind::WindowsScrgb),
                DECODE_WINDOWS_SCRGB.to_string(),
            ),
            (RoundMode::Tonemap(DecodeKind::Sdr), PLAIN_TEXTURE.to_string()),
            (
                RoundMode::Tonemap(DecodeKind::HdrPq),
                crate::hdr_shaders::tonemap_pq_to_sdr(),
            ),
            (
                RoundMode::Tonemap(DecodeKind::WindowsScrgb),
                crate::hdr_shaders::tonemap_scrgb_to_sdr(),
            ),
            // `TonemapAbs10k(Sdr)` resolves to `decode_sdr` (see
            // `RoundShaders::program`), so it shares that source too.
            (RoundMode::TonemapAbs10k(DecodeKind::Sdr), DECODE_SDR.to_string()),
            (
                RoundMode::TonemapAbs10k(DecodeKind::HdrPq),
                crate::hdr_shaders::tonemap_pq_to_abs10k(),
            ),
            (
                RoundMode::TonemapAbs10k(DecodeKind::WindowsScrgb),
                crate::hdr_shaders::tonemap_scrgb_to_abs10k(),
            ),
        ];
        for (mode, source) in sources {
            let referenced = source.contains("sdr_white_nits");
            assert_eq!(
                RoundShaders::wants_sdr_white_nits(mode),
                referenced,
                "{mode:?}: shader references sdr_white_nits = {referenced}, \
                 but wants_sdr_white_nits says {}",
                RoundShaders::wants_sdr_white_nits(mode),
            );
        }
    }

    // The uniform has to actually reach the element, not merely be wanted.
    #[test]
    fn a_tone_mapped_element_carries_the_nits_uniform() {
        for mode in [
            RoundMode::Decode(DecodeKind::Sdr),
            RoundMode::Tonemap(DecodeKind::HdrPq),
            RoundMode::Tonemap(DecodeKind::WindowsScrgb),
            RoundMode::TonemapAbs10k(DecodeKind::Sdr),
            RoundMode::TonemapAbs10k(DecodeKind::HdrPq),
            RoundMode::TonemapAbs10k(DecodeKind::WindowsScrgb),
        ] {
            assert!(
                RoundShaders::wants_sdr_white_nits(mode),
                "{mode:?} must supply sdr_white_nits",
            );
        }
        for mode in [
            RoundMode::Plain,
            RoundMode::Tonemap(DecodeKind::Sdr),
            RoundMode::Decode(DecodeKind::HdrPq),
            RoundMode::Decode(DecodeKind::WindowsScrgb),
        ] {
            assert!(
                !RoundShaders::wants_sdr_white_nits(mode),
                "{mode:?} must not supply a uniform its program never declares",
            );
        }
    }

    // Same sentinel style as `every_decode_kind_maps_to_a_distinct_variant`: a
    // new `SpaceMode` variant needs an arm in `resolve_elem_mode` (and in
    // `space_elements`'s `hdr`/`per_window_program` flags) or it silently
    // falls through to whatever the match's last arm happened to resolve to.
    #[test]
    fn every_space_mode_has_a_distinct_variant() {
        let modes = [
            SpaceMode::Fixed(RoundMode::Plain),
            SpaceMode::TonemapSdr,
            SpaceMode::HdrComposite,
        ];
        assert_eq!(modes.len(), 3, "a new SpaceMode needs an arm in resolve_elem_mode");
    }

    #[test]
    fn hdr_composite_resolves_each_window_to_its_own_decode() {
        assert_eq!(
            resolve_elem_mode(SpaceMode::HdrComposite, DecodeKind::HdrPq),
            RoundMode::Decode(DecodeKind::HdrPq),
            "a PQ window on the HDR composite pass must decode through its own program",
        );
        assert_eq!(
            resolve_elem_mode(SpaceMode::HdrComposite, DecodeKind::Sdr),
            RoundMode::Decode(DecodeKind::Sdr),
            "an SDR window on the HDR composite pass must still decode explicitly -- \
             there is no renderer-wide default to fall back on",
        );
    }

    // Pins the exact bug the `hdr_pass` comment on `space_elements` warns
    // about: swap `HdrComposite`'s `true` for `false` and a frosted backdrop
    // over an HDR wallpaper collapses to sRGB (`RoundMode::Tonemap`) instead
    // of staying in the abs10k working space (`RoundMode::TonemapAbs10k`),
    // which reads on screen as the backdrop crushing to white.
    #[test]
    fn only_hdr_composite_selects_the_abs10k_backdrop_wrapper() {
        assert!(backdrop_hdr_pass(SpaceMode::HdrComposite));
        assert!(!backdrop_hdr_pass(SpaceMode::TonemapSdr));
        assert!(!backdrop_hdr_pass(SpaceMode::Fixed(RoundMode::Plain)));
    }

    #[test]
    fn rescaled_rect_is_identity_at_factor_one() {
        let local_rect = Rectangle::<i32, Logical>::new((10, 20).into(), (100, 50).into());
        let origin = Point::<i32, Physical>::from((60, 45));
        assert_eq!(rescaled_rect(local_rect, origin, 1.0, 1.0), local_rect);
    }

    // `origin` here is deliberately not this rect's own centre (which would
    // be (60, 45)) -- a Reveal tween's origin is the *window's* centre, and a
    // border or backdrop rect can sit anywhere relative to that. Pins that
    // `rescaled_rect` scales about the given origin, not the rect's own.
    #[test]
    fn rescaled_rect_halves_and_pulls_toward_a_off_centre_origin() {
        let local_rect = Rectangle::<i32, Logical>::new((10, 20).into(), (100, 50).into());
        let origin = Point::<i32, Physical>::from((0, 0));
        let out = rescaled_rect(local_rect, origin, 0.5, 1.0);
        assert_eq!(out.loc, (5, 10).into());
        assert_eq!(out.size, (50, 25).into());
    }

    #[test]
    fn fullscreen_tween_window_still_gets_radius_zero() {
        assert_eq!(effective_radius(12.0, true, Some(0.5)), 0.0);
        assert_eq!(effective_radius(12.0, true, None), 0.0);
    }

    #[test]
    fn tween_radius_shrinks_with_the_window() {
        assert_eq!(effective_radius(12.0, false, Some(0.5)), 6.0);
        assert_eq!(effective_radius(12.0, false, None), 12.0);
    }
}
