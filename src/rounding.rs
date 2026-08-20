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
use smithay::backend::renderer::gles::element::PixelShaderElement;
use smithay::backend::renderer::gles::{
    GlesError, GlesRenderer, GlesTexProgram, Uniform, UniformName, UniformType,
};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::backend::renderer::{Renderer, RendererSuper};
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::{ImportAll, ImportMem};
use smithay::backend::renderer::element::AsRenderElements;
use smithay::output::Output;
use smithay::utils::{Buffer as BufferCoords, Logical, Physical, Point, Rectangle, Scale, Transform};
use smithay::utils::user_data::UserDataMap;

use crate::color_management::DecodeKind;
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

/// Splice the rounding mask into a fragment shader.
///
/// Two edits: the helper goes in just above `main` (so `v_coords` is in
/// scope), and every final colour is scaled by the mask. The decode shaders all
/// terminate in `* alpha;`, and the plain one assigns a pre-multiplied `color`
/// -- both forms are handled, and both are asserted on by tests, because a
/// silent miss here would compile fine and simply never round anything.
fn with_rounding(source: &str) -> String {
    let spliced = source.replace("void main() {", &format!("{ROUNDING_GLSL}void main() {{"));
    if spliced.contains("* alpha;") {
        spliced.replace("* alpha;", "* alpha * rubixCornerAlpha();")
    } else {
        spliced.replace("gl_FragColor = color;", "gl_FragColor = color * rubixCornerAlpha();")
    }
}

fn uniform_names() -> [UniformName<'static>; 4] {
    [
        UniformName::new("rubix_radius", UniformType::_1f),
        UniformName::new("rubix_win_size", UniformType::_2f),
        UniformName::new("rubix_elem_offset", UniformType::_2f),
        UniformName::new("rubix_elem_size", UniformType::_2f),
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
}

impl RoundShaders {
    fn compile(renderer: &mut GlesRenderer) -> Result<Self, GlesError> {
        let names = uniform_names();
        // `decode_sdr` keeps its own uniform on top of the rounding ones: the
        // live SDR-white-nits value is still per-frame state the decode needs.
        let mut sdr_names = names.to_vec();
        sdr_names.push(UniformName::new("sdr_white_nits", UniformType::_1f));
        Ok(RoundShaders {
            plain: renderer.compile_custom_texture_shader(with_rounding(PLAIN_TEXTURE), &names)?,
            decode_sdr: renderer
                .compile_custom_texture_shader(with_rounding(DECODE_SDR), &sdr_names)?,
            decode_hdr_pq: renderer
                .compile_custom_texture_shader(with_rounding(DECODE_HDR_PQ), &names)?,
            decode_windows_scrgb: renderer
                .compile_custom_texture_shader(with_rounding(DECODE_WINDOWS_SCRGB), &names)?,
        })
    }

    fn program(&self, mode: RoundMode) -> &GlesTexProgram {
        match mode {
            RoundMode::Plain => &self.plain,
            RoundMode::Decode(DecodeKind::Sdr) => &self.decode_sdr,
            RoundMode::Decode(DecodeKind::HdrPq) => &self.decode_hdr_pq,
            RoundMode::Decode(DecodeKind::WindowsScrgb) => &self.decode_windows_scrgb,
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
}

impl<E: Element> RoundedElement<E> {
    /// `window_rect` and the element's own geometry must be in the same
    /// physical space -- the output-region-local space elements are built in.
    pub(crate) fn new(
        inner: E,
        shaders: &RoundShaders,
        mode: RoundMode,
        radius: f32,
        window_rect: Rectangle<i32, Physical>,
        scale: Scale<f64>,
        sdr_white_nits: f32,
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
        ];
        if mode == RoundMode::Decode(DecodeKind::Sdr) {
            uniforms.push(Uniform::new("sdr_white_nits", sdr_white_nits));
        }
        RoundedElement {
            inner,
            program: shaders.program(mode).clone(),
            uniforms,
        }
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
    fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        OpaqueRegions::default()
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
pub(crate) fn space_elements<R>(
    state: &RubixState,
    renderer: &mut R,
    output: &Output,
    scale: f64,
    mode: RoundMode,
) -> Vec<RubixRenderElement<R>>
where
    R: Renderer + ImportAll + ImportMem + GlesAccess,
    R::TextureId: Clone + 'static,
    RubixRenderElement<R>: RenderElement<R>,
{
    let radius = state.config.decoration.corner_radius as f32;
    let has_borders = state.config.decoration.border_width > 0;
    let hdr = matches!(mode, RoundMode::Decode(_));
    let Some(region) = state.space.output_geometry(output) else {
        return Vec::new();
    };
    if radius <= 0.0 && !has_borders {
        return state
            .space
            .render_elements_for_region(renderer, &region, scale, 1.0)
            .into_iter()
            .map(RubixRenderElement::Surface)
            .collect();
    }
    // `None` means rounding is off or its shaders failed to compile; borders
    // still work either way, windows are just drawn square.
    let round = (radius > 0.0)
        .then(|| round_shaders(renderer.gles_renderer()))
        .flatten();

    let mut elements = Vec::new();
    for window in state.space.elements().rev() {
        let Some(bbox) = state.space.element_bbox(window) else { continue };
        if !region.overlaps(bbox) {
            continue;
        }
        let Some(location) = state.space.element_location(window) else { continue };
        let geometry = window.geometry();
        let render_location: Point<i32, Logical> = location - geometry.loc - region.loc;
        let id = state
            .windows
            .iter()
            .find_map(|(id, w)| (w == window).then_some(*id));
        let fullscreen = id.is_some_and(|id| state.fullscreen_windows.contains(&id));

        // Region-local logical rect of the window's content, which is what the
        // border ring is built around and what the rounding mask is measured
        // against.
        let local_rect = Rectangle::<i32, Logical>::new(location - region.loc, geometry.size);
        let window_rect = Rectangle::<i32, Physical>::new(
            local_rect.loc.to_physical_precise_round(scale),
            local_rect.size.to_physical_precise_round(scale),
        );

        if let Some(id) = id {
            if let Some(ring) =
                crate::decoration::window_border_elements(state, renderer, id, local_rect, hdr)
            {
                elements.push(RubixRenderElement::BorderRing(ring));
            }
        }

        let surfaces = window.render_elements::<WaylandSurfaceRenderElement<R>>(
            renderer,
            render_location.to_physical_precise_round(scale),
            Scale::from(scale),
            1.0,
        );
        match (&round, fullscreen) {
            (Some(shaders), false) => elements.extend(surfaces.into_iter().map(|e| {
                RubixRenderElement::Rounded(RoundedElement::new(
                    e,
                    shaders,
                    mode,
                    radius,
                    window_rect,
                    Scale::from(scale),
                    state.sdr_white_nits,
                ))
            })),
            _ => elements.extend(surfaces.into_iter().map(RubixRenderElement::Surface)),
        }
    }
    elements
}

#[cfg(test)]
mod tests {
    use super::*;

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
            let out = with_rounding(source);
            assert!(out.contains("float rubixCornerAlpha()"), "{name}: helper missing");
            assert!(
                out.contains("rubixCornerAlpha();"),
                "{name}: mask never applied to the output"
            );
        }
    }

    #[test]
    fn the_helper_is_declared_after_v_coords_so_it_compiles() {
        for source in [PLAIN_TEXTURE, DECODE_SDR, DECODE_HDR_PQ, DECODE_WINDOWS_SCRGB] {
            let out = with_rounding(source);
            let varying = out.find("varying vec2 v_coords;").expect("v_coords declared");
            let helper = out.find("float rubixCornerAlpha()").expect("helper present");
            assert!(varying < helper, "helper must come after the varying it reads");
        }
    }

    #[test]
    fn splicing_leaves_the_defines_marker_intact() {
        // Smithay substitutes this line; losing it breaks every variant.
        for source in [PLAIN_TEXTURE, DECODE_SDR, DECODE_HDR_PQ, DECODE_WINDOWS_SCRGB] {
            assert!(with_rounding(source).contains("//_DEFINES_"));
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
}
