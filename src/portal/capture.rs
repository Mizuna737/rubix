//! Portal-local capture helper: render a [`CaptureTarget`] (a whole monitor or
//! a single window) into a CPU-side `Xrgb8888` buffer.
//!
//! Adapted from `screencopy.rs`'s `fulfill_pending`/`copy_output` render->readback
//! pattern (`create_buffer` -> `bind` -> `OutputDamageTracker::render_output` ->
//! `copy_framebuffer` -> `map_texture`), but reading back into an owned `Vec<u8>`
//! instead of a client's `wl_shm` buffer, and offering a whole-window variant
//! that renders a `Window`'s own element list at its natural size rather than
//! cropping a region out of an output. `screencopy.rs` itself is untouched --
//! this module duplicates the small element-building/readback logic instead of
//! exporting from it, so the live wlr-screencopy path (xdpw's backing
//! implementation) can't be perturbed by portal changes.
//!
//! `Xrgb8888` is a deliberate match for the M2 PipeWire format
//! (`SPA_VIDEO_FORMAT_BGRx`): both are 4 bytes/px with B,G,R,X byte order in
//! little-endian memory, so no pixel-format conversion is needed between a
//! captured frame and the negotiated PipeWire buffer -- only the GL
//! bottom-up/top-down flip (same caveat `screencopy.rs` documents) is handled
//! here.

use std::sync::{Arc, Mutex};

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            damage::OutputDamageTracker,
            element::{surface::WaylandSurfaceRenderElement, AsRenderElements, RenderElement},
            gles::GlesRenderbuffer,
            Bind, ExportMem, ImportAll, ImportMem, Offscreen, Renderer, Texture, TextureMapping,
        },
    },
    desktop::layer_map_for_output,
    utils::{Buffer as BufferCoord, Physical, Point, Rectangle, Scale, Size, Transform},
};

use crate::cursor::RubixRenderElement;
use crate::state::RubixState;

/// Shared latest-frame slot: the capture cadence (loop thread) writes into it,
/// the PipeWire `process` callback (pw thread) reads out of it. `Arc<FrameBuffer>`
/// so a read is one atomic clone, not a byte copy -- the actual per-cycle copy
/// into the dequeued PipeWire buffer happens on the pw thread, once, in
/// `pipewire_stream::process`.
pub type FrameSlot = Arc<Mutex<Option<Arc<FrameBuffer>>>>;

/// What a session streams. Monitors are identified by output *name* (not the
/// `Output` object itself) so this stays plain `Send + Sync` data shareable
/// with the zbus thread's session registry without dragging smithay's `Output`
/// across the thread boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureTarget {
    Monitor(String),
    Window(u32),
}

/// One captured frame, CPU-side, `Xrgb8888` (== `BGRx` byte order), top-down.
#[derive(Clone)]
pub struct FrameBuffer {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

/// Resolve a target's current physical size, without rendering anything.
/// `Start` uses this to advertise the stream's `size` and to size the
/// PipeWire buffers; the capture timer uses it to detect a resize (window
/// moved/resized, output mode change) between captures.
pub fn target_size(state: &RubixState, target: &CaptureTarget) -> Option<(u32, u32)> {
    match target {
        CaptureTarget::Monitor(name) => {
            let output = state.space.outputs().find(|o| &o.name() == name)?;
            let mode = output.current_mode()?;
            let size = output.current_transform().transform_size(mode.size);
            Some((size.w.max(1) as u32, size.h.max(1) as u32))
        }
        CaptureTarget::Window(id) => {
            let window = state.windows.get(id)?;
            let size = window.geometry().size;
            Some((size.w.max(1) as u32, size.h.max(1) as u32))
        }
    }
}

/// Render `target` and read it back into an owned, top-down `Xrgb8888` buffer.
pub fn capture_frame<R>(
    state: &RubixState,
    renderer: &mut R,
    target: &CaptureTarget,
) -> Result<FrameBuffer, String>
where
    R: Renderer + ImportAll + ImportMem + ExportMem + Offscreen<GlesRenderbuffer> + Bind<GlesRenderbuffer>,
    R::TextureId: Texture + Clone + Send + 'static,
    RubixRenderElement<R>: RenderElement<R>,
{
    match target {
        CaptureTarget::Monitor(name) => capture_monitor(state, renderer, name),
        CaptureTarget::Window(id) => capture_window(state, renderer, *id),
    }
}

/// Mirrors `screencopy::build_elements`'s output-wide walk (layers, top to
/// bottom, then mapped windows), minus cursor overlay -- portal consumers
/// don't get the compositor's software cursor composited in.
fn capture_monitor<R>(
    state: &RubixState,
    renderer: &mut R,
    output_name: &str,
) -> Result<FrameBuffer, String>
where
    R: Renderer + ImportAll + ImportMem + ExportMem + Offscreen<GlesRenderbuffer> + Bind<GlesRenderbuffer>,
    R::TextureId: Texture + Clone + Send + 'static,
    RubixRenderElement<R>: RenderElement<R>,
{
    let output = state
        .space
        .outputs()
        .find(|o| o.name() == output_name)
        .ok_or_else(|| format!("output {output_name:?} no longer present"))?
        .clone();

    let mode = output.current_mode().ok_or("output has no mode")?;
    let phys = output.current_transform().transform_size(mode.size);
    let scale = 1.0_f64;

    let mut background: Vec<WaylandSurfaceRenderElement<R>> = Vec::new();
    let mut bottom: Vec<WaylandSurfaceRenderElement<R>> = Vec::new();
    let mut top: Vec<WaylandSurfaceRenderElement<R>> = Vec::new();
    let mut overlay: Vec<WaylandSurfaceRenderElement<R>> = Vec::new();
    {
        use smithay::wayland::shell::wlr_layer::Layer;
        let map = layer_map_for_output(&output);
        for layer in map.layers() {
            let Some(geo) = map.layer_geometry(layer) else { continue };
            let loc = geo.loc.to_physical_precise_round(scale);
            let elems = layer.render_elements::<WaylandSurfaceRenderElement<R>>(
                renderer,
                loc,
                Scale::from(scale),
                1.0,
            );
            match layer.layer() {
                Layer::Background => background.extend(elems),
                Layer::Bottom => bottom.extend(elems),
                Layer::Top => top.extend(elems),
                Layer::Overlay => overlay.extend(elems),
            }
        }
    }

    let space_elements: Vec<WaylandSurfaceRenderElement<R>> = state
        .space
        .output_geometry(&output)
        .map(|geo| state.space.render_elements_for_region(renderer, &geo, scale, 1.0))
        .unwrap_or_default();

    let mut elements: Vec<RubixRenderElement<R>> = Vec::new();
    elements.extend(overlay.into_iter().map(RubixRenderElement::Surface));
    elements.extend(top.into_iter().map(RubixRenderElement::Surface));
    // Borders are part of the desktop, so a screencast shows them. `hdr = false`:
    // the readback destination is an 8-bit SDR buffer, so the border wants its
    // plain color, not one pre-compensated for a transform this path never runs.
    elements.extend(
        crate::decoration::border_elements(state, &output, scale, false)
            .into_iter()
            .map(RubixRenderElement::Solid),
    );
    elements.extend(space_elements.into_iter().map(RubixRenderElement::Surface));
    elements.extend(bottom.into_iter().map(RubixRenderElement::Surface));
    elements.extend(background.into_iter().map(RubixRenderElement::Surface));

    render_and_readback(renderer, phys, &elements)
}

/// Renders a single window's own surface tree at its natural size,
/// position/stacking/occlusion independent: the offscreen is sized to the
/// window's *geometry* (its logical content rect, which may crop CSD
/// shadows/negative subsurface offsets), and the surface tree's origin is
/// shifted so that geometry's top-left lands at (0, 0) in the capture --
/// exactly mirroring how `Space::render_elements_for_region` derives
/// `render_location = position - geometry.loc` for mapped windows.
fn capture_window<R>(state: &RubixState, renderer: &mut R, id: u32) -> Result<FrameBuffer, String>
where
    R: Renderer + ImportAll + ImportMem + ExportMem + Offscreen<GlesRenderbuffer> + Bind<GlesRenderbuffer>,
    R::TextureId: Texture + Clone + Send + 'static,
    RubixRenderElement<R>: RenderElement<R>,
{
    let window = state.windows.get(&id).ok_or_else(|| format!("window {id} not found"))?.clone();
    let geo = window.geometry();
    let scale = 1.0_f64;

    let phys = Size::<i32, Physical>::from((geo.size.w.max(1), geo.size.h.max(1)));
    let origin: Point<i32, smithay::utils::Logical> = Point::from((0, 0)) - geo.loc;
    let loc = origin.to_physical_precise_round(scale);

    let surface_elems: Vec<WaylandSurfaceRenderElement<R>> =
        window.render_elements(renderer, loc, Scale::from(scale), 1.0);
    let elements: Vec<RubixRenderElement<R>> =
        surface_elems.into_iter().map(RubixRenderElement::Surface).collect();

    render_and_readback(renderer, phys, &elements)
}

/// Shared render-to-offscreen + readback tail: create an offscreen sized to
/// `phys`, render `elements` into it (`Normal` transform -- capture targets
/// are content, not a physical display), read the whole thing back, and
/// normalize to top-down (GL readback is bottom-up; see `screencopy.rs`'s
/// `write_shm` for the same normalization on the live wlr-screencopy path).
fn render_and_readback<R>(
    renderer: &mut R,
    phys: Size<i32, Physical>,
    elements: &[RubixRenderElement<R>],
) -> Result<FrameBuffer, String>
where
    R: Renderer + ImportAll + ImportMem + ExportMem + Offscreen<GlesRenderbuffer> + Bind<GlesRenderbuffer>,
    R::TextureId: Texture + Clone + Send + 'static,
    RubixRenderElement<R>: RenderElement<R>,
{
    let scale = 1.0_f64;
    let buffer_size = Size::<i32, BufferCoord>::from((phys.w, phys.h));
    let mut target =
        renderer.create_buffer(Fourcc::Xrgb8888, buffer_size).map_err(|e| format!("create_buffer: {e:?}"))?;

    let width = phys.w.max(0) as u32;
    let height = phys.h.max(0) as u32;
    let stride = width * 4;

    let (mapping_flipped, mapped): (bool, Vec<u8>) = {
        let mut fb = renderer.bind(&mut target).map_err(|e| format!("bind: {e:?}"))?;
        let mut damage_tracker = OutputDamageTracker::new(phys, scale, Transform::Normal);
        damage_tracker
            .render_output(renderer, &mut fb, 0, elements, [0.0, 0.0, 0.0, 1.0])
            .map_err(|e| format!("render_output: {e:?}"))?;

        let region = Rectangle::<i32, BufferCoord>::new(Point::from((0, 0)), (phys.w, phys.h).into());
        let mapping =
            renderer.copy_framebuffer(&fb, region, Fourcc::Xrgb8888).map_err(|e| format!("copy_framebuffer: {e:?}"))?;
        let mapping_flipped = mapping.flipped();
        crate::screencopy::log_readback_orientation("portal", mapping_flipped);
        let src = renderer.map_texture(&mapping).map_err(|e| format!("map_texture: {e:?}"))?;
        (mapping_flipped, src.to_vec())
    };

    let mut data = vec![0u8; (stride as usize) * (height as usize)];
    let src_stride = stride as usize;
    let rows = height as usize;
    for y in 0..rows {
        let src_row = crate::screencopy::source_row(y, rows, mapping_flipped);
        let s = src_row * src_stride;
        let d = y * src_stride;
        if s + src_stride <= mapped.len() && d + src_stride <= data.len() {
            data[d..d + src_stride].copy_from_slice(&mapped[s..s + src_stride]);
        }
    }

    Ok(FrameBuffer { data, width, height, stride })
}
