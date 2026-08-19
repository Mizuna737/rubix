//! TTY / DRM backend (Track B).
//!
//! Drives the compositor directly on a bare TTY: LibSeat session, udev device
//! discovery, DRM/KMS scanout over GBM buffers, an EGL/GLES context feeding the
//! same `Space` the winit backend renders, libinput input, and VT switching.
//! Structured for N outputs (a per-DRM-node device map, a per-CRTC surface map)
//! but first-light targets a single monitor.
//!
//! ## State threading (why the `Rc<RefCell<UdevData>>`)
//!
//! Unlike the winit backend -- which moves *all* its backend state into a single
//! event-source closure -- the udev backend has several long-lived calloop
//! sources (the libseat session notifier, the udev monitor, and one DRM VBlank
//! source per device) that each need shared mutable access to the *same* backend
//! state (session, GPU manager, per-device backend map). A single moved closure
//! can't serve them all, and genericizing `RubixState<B>` the way anvil does
//! (`AnvilState<UdevData>`) would spill backend bounds through every Wayland
//! delegate in `handlers.rs` -- a rewrite for a payoff we don't need with two
//! backends.
//!
//! Instead the backend state lives behind an `Rc<RefCell<UdevData>>`. Each source
//! closure captures its own clone; the clones keep the allocation alive for the
//! loop's lifetime. Every callback still receives `&mut RubixState` directly
//! (space/seat) *plus* its own `udev.borrow_mut()` -- disjoint
//! allocations, safe to hold at once. This is panic-free only because calloop
//! dispatches sources non-reentrantly and Rubix never renders synchronously from
//! input (render lives solely on the VBlank/Timer path), so two `udev` borrows
//! can never overlap.
//!
//! The compositor logic (device_added, connector_connected, render, frame_finish)
//! is written as free functions taking `(&mut UdevData, &mut RubixState, ...)`
//! rather than methods: they need disjoint mutable borrows of the GPU manager and
//! the per-device map at once (to hold a renderer and a surface simultaneously),
//! which the borrow checker only grants across direct field access.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use smithay::backend::allocator::format::FormatSet;
use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::allocator::{Buffer as _, Fourcc, Modifier};
use smithay::backend::drm::exporter::gbm::GbmFramebufferExporter;
use smithay::backend::drm::output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements};
use smithay::backend::drm::compositor::{FrameFlags, PrimaryPlaneElement};
use smithay::backend::drm::{
    Colorspace, ConnectorColorState, DrmDevice, DrmDeviceFd, DrmEvent, DrmEventMetadata, DrmNode,
    DrmSurface, NodeType,
};
use smithay::backend::egl::{EGLDevice, EGLDisplay};
use smithay::backend::egl::context::ContextPriority;
use smithay::backend::input::InputEvent;
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::renderer::damage::{Error as OutputDamageTrackerError, OutputDamageTracker};
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::texture::TextureRenderElement;
use smithay::backend::renderer::element::utils::select_dmabuf_feedback;
use smithay::backend::renderer::utils::with_renderer_surface_state;
use smithay::backend::renderer::element::{
    AsRenderElements, Element, Id, Kind, RenderElement, RenderElementPresentationState,
    RenderElementStates, RenderingReason, default_primary_scanout_output_compare,
};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture, Uniform};
use smithay::backend::renderer::multigpu::gbm::GbmGlesBackend;
use smithay::backend::renderer::multigpu::{GpuManager, MultiRenderer};
use smithay::backend::renderer::{Bind, Color32F, Frame, ImportDma, Offscreen, Renderer};
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionEvent, Session};
use smithay::backend::udev::{all_gpus, primary_gpu, UdevBackend, UdevEvent};
use smithay::backend::SwapBuffersError;
use smithay::wayland::dmabuf::get_dmabuf;
use smithay::desktop::space::SpaceRenderElements;
use smithay::desktop::utils::{
    surface_primary_scanout_output, update_surface_primary_scanout_output, OutputPresentationFeedback,
};
use smithay::desktop::{layer_map_for_output, Window};
use smithay::output::{Mode as WlMode, Output, PhysicalProperties, Subpixel};
use smithay::wayland::shell::wlr_layer::Layer;
use smithay::wayland::dmabuf::{DmabufFeedback, DmabufFeedbackBuilder};
use smithay::wayland::seat::WaylandFocus;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{LoopHandle, RegistrationToken};
use smithay::reexports::drm::control::{connector, crtc, ModeTypeFlags};
use smithay::reexports::drm::Device as BaseDrmDevice;
use smithay::reexports::input::Libinput;
use smithay::reexports::rustix::fs::OFlags;
use smithay::reexports::wayland_protocols::wp::linux_dmabuf::zv1::server::zwp_linux_dmabuf_feedback_v1;
use smithay::reexports::wayland_server::backend::GlobalId;
use smithay::utils::{
    Buffer as BufferCoord, DeviceFd, Logical, Physical, Point, Rectangle, Scale, Size, Transform,
};

use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};

use crate::color_management::{surface_decode_kind, DecodeKind};
use crate::cursor::{pointer_render_elements, RubixRenderElement};
use crate::hdr_shaders::{compile_hdr_shaders, sdr_solid_transform, HdrShaders};
use crate::state::Pos;
use crate::RubixState;

// The four `DrmOutputManager`/`DrmOutput` generics never vary in this backend:
//   A = GbmAllocator<DrmDeviceFd>          (scanout buffer allocator)
//   F = GbmFramebufferExporter<DrmDeviceFd> (turns buffers into DRM framebuffers)
//   U = Option<OutputPresentationFeedback>  (per-frame user data; None for now)
//   G = DrmDeviceFd                         (AsFd for the gbm cursor device)
// Aliasing them here avoids anvil's repetition at every use site.
type RubixDrmOutputManager = DrmOutputManager<
    GbmAllocator<DrmDeviceFd>,
    GbmFramebufferExporter<DrmDeviceFd>,
    Option<OutputPresentationFeedback>,
    DrmDeviceFd,
>;
type RubixDrmOutput = DrmOutput<
    GbmAllocator<DrmDeviceFd>,
    GbmFramebufferExporter<DrmDeviceFd>,
    Option<OutputPresentationFeedback>,
    DrmDeviceFd,
>;
/// The multi-GPU renderer, both slots the same GBM/GLES backend. For Rubix's
/// single GPU the render and target node are identical, so this is always the
/// zero-copy `single_renderer` case; the alias keeps the door open for N GPUs.
type RubixRenderer<'a> =
    MultiRenderer<'a, 'a, GbmGlesBackend<GlesRenderer, DrmDeviceFd>, GbmGlesBackend<GlesRenderer, DrmDeviceFd>>;

/// 10-bit-capable formats first, then 8-bit. `initialize_output` walks these in
/// order picking the first the hardware accepts. NVIDIA + others fall back to
/// 8-bit; listing 10-bit first is free future HDR headroom (see roadmap).
const SUPPORTED_FORMATS: &[Fourcc] = &[
    Fourcc::Abgr2101010,
    Fourcc::Argb2101010,
    Fourcc::Abgr8888,
    Fourcc::Argb8888,
];

/// HDR Phase 2's linear intermediate: the offscreen an `hdr = true` output's
/// decode pass renders into before the encode pass scans it back out through
/// [`SUPPORTED_FORMATS`]. Fallback-preference order, mirroring
/// `hdr_offscreen_probe.rs`'s own candidate list (Phase 0 finding: both bind
/// fine on this machine's NVIDIA stack; kept as a fallback for other
/// hardware). **Never a scanout format** -- `SUPPORTED_FORMATS` above stays
/// untouched; this list is the linear intermediate only, bound via
/// `Offscreen::<GlesTexture>::create_buffer`, never handed to
/// `initialize_output`.
const HDR_OFFSCREEN_FORMATS: &[Fourcc] = &[Fourcc::Abgr16161616f, Fourcc::Argb16161616f];

/// Backend-wide state, shared across calloop sources via `Rc<RefCell<_>>`.
/// `pub(crate)` (and the `gpus`/`primary_gpu` fields below) so `RubixState`'s
/// `DmabufHandler` impl (handlers/dmabuf.rs) can reach the renderer through
/// `RubixState::udev_handle`; the rest stays private to this module.
pub(crate) struct UdevData {
    /// Seat session -- owns the VT, brokers DRM/input device fds.
    session: LibSeatSession,
    /// The primary render GPU. For a single-GPU box this is the only node.
    pub(crate) primary_gpu: DrmNode,
    /// Multi-GPU renderer registry (one node registered per DRM device).
    pub(crate) gpus: GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>>,
    /// One entry per DRM device (GPU), keyed by its primary node.
    ///
    /// `pub(crate)` so `color_management.rs`'s `description_for_output` can
    /// walk it read-only to find an output's live `hdr` state.
    pub(crate) backends: HashMap<DrmNode, BackendData>,
    /// Loop handle for inserting per-device VBlank sources and frame timers from
    /// inside the udev/device callbacks.
    loop_handle: LoopHandle<'static, RubixState>,
    /// Whether we currently own the VT. Set false on `PauseSession` (VT switched
    /// away): the DRM device is paused, so rendering would only produce rejected
    /// page-flips. `render` bails while inactive and stops rescheduling; the
    /// `ActivateSession` handler re-kicks the render loop on return.
    active: bool,
}

/// Per-DRM-device state: the output manager (owns the DRM device + allocator),
/// the connector scanner, and the live per-CRTC surfaces.
pub(crate) struct BackendData {
    /// Manages DRM outputs (scanout, damage, plane assignment) for this device.
    drm_output_manager: RubixDrmOutputManager,
    /// Scans connectors→CRTCs on hotplug/probe.
    drm_scanner: DrmScanner,
    /// Live outputs on this device, keyed by CRTC.
    ///
    /// `pub(crate)` -- see `UdevData::backends`.
    pub(crate) surfaces: HashMap<crtc::Handle, SurfaceData>,
    /// The EGL render node backing this device (== primary_gpu on single-GPU).
    render_node: DrmNode,
    /// calloop token for this device's DRM VBlank source, removed on unplug.
    registration_token: RegistrationToken,
}

/// Per-CRTC (per-monitor) state: the smithay `Output`, its global, and the
/// `DrmOutput` we scan out through.
pub(crate) struct SurfaceData {
    /// The logical output mapped into the space.
    ///
    /// `pub(crate)` -- see `UdevData::backends`.
    pub(crate) output: Output,
    /// Wayland global for the output, destroyed on disconnect.
    global: Option<GlobalId>,
    /// The DRM scanout target (wraps a DrmCompositor internally).
    drm_output: RubixDrmOutput,
    /// Frame interval derived from the mode refresh, for scheduling repaint.
    frame_duration: Duration,
    /// HDR Phase 2: whether this output composites through the linear 16F
    /// pipeline (`render_surface`'s HDR branch). This is the *live* state --
    /// `toggle_hdr` flips it at runtime (gated by `hdr_capable` below) to A/B
    /// HDR vs SDR on the same output without a config-edit + TTY restart.
    ///
    /// `pub(crate)` -- see `UdevData::backends`.
    pub(crate) hdr: bool,
    /// Fixed at `connector_connected` from `OutputConfig::hdr`: whether this
    /// output may ever use HDR. Never changes live -- it's the gate that
    /// keeps `toggle_hdr` from trying to enable HDR on a non-HDR-capable
    /// output (e.g. the HDMI strip).
    hdr_capable: bool,
    /// HDR Phase 2 shader cache. Compiled once (lazily, on this output's
    /// first HDR frame) via `compile_hdr_shaders` and never recompiled.
    /// Cached HERE (per-output) rather than on `UdevData`: `render_surface`
    /// already holds `&mut SurfaceData` exclusively while `renderer: &mut
    /// RubixRenderer<'_>` only borrows `UdevData::gpus` -- caching on the
    /// whole `UdevData` would need a second live borrow of it alongside the
    /// renderer's, which the caller (`render()`, udev.rs:809-844) can't grant
    /// (the renderer is acquired via `udev_data.gpus...` while `backend`/
    /// `surface` borrow `udev_data.backends` -- disjoint fields, but a bare
    /// `&mut UdevData` here would re-merge them). `None` only until the first
    /// successful compile; `Some` forever after (compile failure is handled
    /// by returning an `Err` from `render_surface_hdr` and falling back to
    /// SDR for that frame, WITHOUT caching a failure -- see there).
    hdr_shaders: Option<HdrShaders>,
    /// HDR Phase 2 linear offscreen + its damage tracker, sized to the
    /// output's current mode. Re-created only when the mode size changes
    /// (`render_surface_hdr`'s resize check) -- never per frame.
    hdr_offscreen: Option<HdrOffscreen>,
    /// Connector color state currently staged on this output. `render_surface`
    /// is the sole owner: it computes the desired mode each frame and only
    /// touches DRM on a transition, so nothing else may call
    /// `set_hdr_output_properties` / `set_sdr_output_properties` directly.
    /// `None` at bringup so the first frame always applies.
    applied_connector_hdr: Option<bool>,
    /// Whether the previous frame's primary-plane scanout attempt promoted
    /// (direct scanout) or fell back to composite. Tracked so the
    /// direct-scanout diagnostic in `render_surface` logs only on change.
    last_scanout_promoted: Option<bool>,
    /// Which window was the fullscreen scanout candidate on the previous frame.
    ///
    /// Paired with `last_scanout_promoted` so the diagnostic fires when the
    /// *candidate* changes too, not only when promotion flips. Without it a
    /// second fullscreen client that fails exactly like the first logs nothing
    /// -- the flag is already `Some(false)` -- and that silence is
    /// indistinguishable from the diagnostic never running at all.
    last_scanout_candidate: Option<u32>,
    /// The two dmabuf feedback objects clients on this output should pick
    /// between (see `select_dmabuf_feedback`): `render_feedback` names only
    /// the primary GPU's render-optimal formats/modifiers; `scanout_feedback`
    /// additionally advertises a `Scanout`-flagged tranche naming the CRTC's
    /// plane formats (intersected with what we can also render, so there's
    /// always a fallback). `None` if building it at bringup failed (e.g. no
    /// renderer for the primary GPU) -- feedback is then simply not sent and
    /// clients fall back to the default global's formats (udev.rs ~495).
    dmabuf_feedback: Option<SurfaceDmabufFeedback>,
    /// The primary plane's raw advertised format set, captured at bringup.
    ///
    /// Deliberately NOT the intersection with render formats that
    /// `get_surface_dmabuf_feedback` builds its tranche from: this exists to
    /// answer "would KMS accept this buffer on the primary plane at all", and
    /// narrowing it first would make a format the plane genuinely supports look
    /// unsupported just because we can't render from it. Diagnostic only --
    /// nothing in the render path reads it.
    primary_plane_formats: FormatSet,
}

/// The pair of dmabuf feedback objects sent to a surface depending on whether
/// its buffer, this frame, ended up being scanned out directly or composited
/// (`select_dmabuf_feedback` decides which). See `get_surface_dmabuf_feedback`.
pub(crate) struct SurfaceDmabufFeedback {
    render_feedback: DmabufFeedback,
    scanout_feedback: DmabufFeedback,
    /// Format count of the `Scanout`-flagged tranche, cached at construction
    /// time (`DmabufFeedback` exposes no public accessor for this) purely
    /// for the Deliverable-3 diagnostic below -- lets a missing/empty
    /// tranche be told apart from a client that just ignored it.
    scanout_format_count: usize,
}

/// Builds the render/scanout feedback pair for one CRTC's surface, mirroring
/// anvil's `get_surface_dmabuf_feedback` (`anvil/src/udev.rs:697-758`).
///
/// Anvil threads a separate `render_node: Option<DrmNode>` because it
/// supports rendering on one GPU and scanning out on another. Rubix has a
/// single primary GPU (`RubixState`/`UdevData::primary_gpu`), so that branch
/// collapses: `all_render_formats` is just the primary GPU's dmabuf formats,
/// and the render-feedback builder needs no secondary preference tranche.
/// `scanout_node` is kept as its own parameter (rather than reusing
/// `primary_gpu`) because it's the CRTC's own device node, which is what the
/// plane-format tranche's fallback-render tranche should be keyed to -- see
/// anvil's `node` argument at the call site.
fn get_surface_dmabuf_feedback(
    primary_gpu: DrmNode,
    scanout_node: DrmNode,
    gpus: &mut GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>>,
    surface: &DrmSurface,
) -> Option<SurfaceDmabufFeedback> {
    let all_render_formats = gpus.single_renderer(&primary_gpu).ok()?.dmabuf_formats();

    let planes = surface.planes().clone();

    // Limit the scan-out tranche to formats we can also render from, so a
    // render fallback always exists if a given buffer turns out not to be
    // scannable (mirrors anvil's comment at udev.rs:718-721).
    let planes_formats = surface
        .plane_info()
        .formats
        .iter()
        .copied()
        .chain(planes.overlay.into_iter().flat_map(|p| p.formats))
        .collect::<FormatSet>()
        .intersection(&all_render_formats)
        .copied()
        .collect::<FormatSet>();

    let scanout_format_count = planes_formats.iter().count();

    let builder = DmabufFeedbackBuilder::new(primary_gpu.dev_id(), all_render_formats.clone());
    let render_feedback = builder.clone().build().ok()?;

    let scanout_feedback = builder
        .add_preference_tranche(
            surface.device_fd().dev_id().ok()?,
            Some(zwp_linux_dmabuf_feedback_v1::TrancheFlags::Scanout),
            planes_formats,
        )
        .add_preference_tranche(scanout_node.dev_id(), None, all_render_formats)
        .build()
        .ok()?;

    Some(SurfaceDmabufFeedback {
        render_feedback,
        scanout_feedback,
        scanout_format_count,
    })
}

/// HDR Phase 2's per-output linear intermediate: a 16F `GlesTexture` the
/// decode pass renders into and the encode pass samples back out of, plus
/// the `OutputDamageTracker` that drives the decode pass.
///
/// The encode element deliberately does NOT reuse a stable `Id` across
/// frames: the offscreen's *content* is fully re-rendered every frame, so a
/// persistent id would make `DrmCompositor`'s damage tracking on the encode
/// call see an unchanged element and report *empty* damage after the first
/// frame -- leaving every other scanout backbuffer cleared to black (the
/// "black screen, cursor still visible on its hw plane" failure). A fresh
/// `Id::new()` per frame reads as remove-old + add-new = full damage, which
/// is correct because the whole fullscreen element genuinely changes each
/// frame. See `render_surface_hdr`'s encode pass.
struct HdrOffscreen {
    texture: GlesTexture,
    /// Buffer-space size the texture was created at (mode.size verbatim --
    /// see `render_surface_hdr`'s resize check for why not
    /// `current_transform().transform_size(mode.size)`).
    size: Size<i32, BufferCoord>,
    /// Drives the decode pass (`OutputDamageTracker::new`, `Transform::
    /// Normal`, matching the same physical/untransformed convention the
    /// scene `elements` are already built in -- see `screencopy.rs`'s
    /// `copy_output` for the proven prior art of this exact "re-render into
    /// an offscreen of output size" pattern).
    damage_tracker: OutputDamageTracker,
}

/// Entry point for the TTY/DRM backend. Mirrors `winit::init_winit`'s signature
/// so `main` can dispatch to either behind [`crate::Backend`].
pub fn init_udev(
    event_loop: &mut smithay::reexports::calloop::EventLoop<'static, RubixState>,
    data: &mut RubixState,
) -> Result<(), Box<dyn std::error::Error>> {
    // ---- session ----
    let (session, notifier) = LibSeatSession::new()?;
    let seat_name = session.seat();
    tracing::info!("acquired libseat session on {seat_name}");

    // ---- primary GPU discovery ----
    // Prefer the seat's designated primary GPU, resolved to its render node;
    // fall back to the first enumerable GPU. Panics only if the box has no GPU.
    let primary = primary_gpu(&seat_name)?
        .and_then(|path| {
            DrmNode::from_path(path)
                .ok()?
                .node_with_type(NodeType::Render)?
                .ok()
        })
        .unwrap_or_else(|| {
            all_gpus(&seat_name)
                .expect("failed to enumerate GPUs")
                .into_iter()
                .find_map(|path| DrmNode::from_path(path).ok())
                .expect("no GPU found")
        });
    tracing::info!("primary GPU: {primary}");

    // ---- GPU manager ----
    let gpus = GpuManager::new(GbmGlesBackend::with_context_priority(ContextPriority::High))?;

    let udev = Rc::new(RefCell::new(UdevData {
        session,
        primary_gpu: primary,
        gpus,
        backends: HashMap::new(),
        loop_handle: event_loop.handle(),
        active: true,
    }));

    // Give `RubixState` a handle back into the renderer, so `DmabufHandler::dmabuf_imported`
    // (which fires on `RubixState`, not `UdevData`) can validate imported buffers.
    data.udev_handle = Some(udev.clone());

    // ---- udev device monitor ----
    let udev_backend = UdevBackend::new(&seat_name)?;

    // Enumerate devices already present at startup (the monitor only reports
    // *changes* from here on, not the initial set).
    for (device_id, path) in udev_backend.device_list() {
        if let Ok(node) = DrmNode::from_dev_id(device_id) {
            if let Err(e) = device_added(&udev, data, node, path) {
                tracing::warn!("skipping device {node} ({path:?}): {e}");
            }
        }
    }

    {
        let udev = udev.clone();
        event_loop
            .handle()
            .insert_source(udev_backend, move |event, _, data| match event {
                UdevEvent::Added { device_id, path } => {
                    if let Ok(node) = DrmNode::from_dev_id(device_id) {
                        if let Err(e) = device_added(&udev, data, node, &path) {
                            tracing::warn!("device_added({node}) failed: {e}");
                        }
                    }
                }
                UdevEvent::Changed { device_id } => {
                    if let Ok(node) = DrmNode::from_dev_id(device_id) {
                        device_changed(&udev, data, node);
                    }
                }
                UdevEvent::Removed { device_id } => {
                    if let Ok(node) = DrmNode::from_dev_id(device_id) {
                        device_removed(&udev, data, node);
                    }
                }
            })
            .map_err(|e| format!("failed to register udev source: {e}"))?;
    }

    // ---- libinput ----
    let mut libinput_context = Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(
        udev.borrow().session.clone().into(),
    );
    libinput_context
        .udev_assign_seat(&seat_name)
        .map_err(|_| "failed to assign libinput seat")?;
    let libinput_backend = LibinputInputBackend::new(libinput_context.clone());

    let udev_for_input = udev.clone();
    event_loop
        .handle()
        .insert_source(libinput_backend, move |event, _, data| {
            // Filter out the session-add/removed device events smithay surfaces;
            // Rubix's `process_input_event` handles the rest exactly as winit.
            if let InputEvent::DeviceAdded { .. } | InputEvent::DeviceRemoved { .. } = &event {
                return;
            }
            data.process_input_event(event);
            // A VT-switch chord sets `pending_vt` inside `process_input_event`
            // (it has the xkb state); only the backend owns the session, so we
            // perform the actual switch here. Without this the compositor holds
            // DRM master forever and Ctrl+Alt+Fn is swallowed -- trapping the TTY.
            if let Some(vt) = data.pending_vt.take() {
                tracing::info!("switching to VT {vt}");
                if let Err(e) = udev_for_input.borrow_mut().session.change_vt(vt) {
                    tracing::error!("failed to switch to VT {vt}: {e}");
                }
            }
        })
        .map_err(|e| format!("failed to register libinput source: {e}"))?;

    // ---- session VT-switch notifier ----
    {
        let udev = udev.clone();
        let mut libinput_context = libinput_context;
        event_loop
            .handle()
            .insert_source(notifier, move |event, _, _data| match event {
                SessionEvent::PauseSession => {
                    tracing::info!("session paused (VT switched away)");
                    libinput_context.suspend();
                    let mut guard = udev.borrow_mut();
                    guard.active = false;
                    for backend in guard.backends.values_mut() {
                        backend.drm_output_manager.pause();
                    }
                }
                SessionEvent::ActivateSession => {
                    tracing::info!("session activated (VT switched back)");
                    if let Err(e) = libinput_context.resume() {
                        tracing::error!("failed to resume libinput: {e:?}");
                    }
                    let nodes: Vec<(DrmNode, Vec<crtc::Handle>)> = {
                        let mut guard = udev.borrow_mut();
                        guard.active = true;
                        for backend in guard.backends.values_mut() {
                            if let Err(e) = backend.drm_output_manager.lock().activate(false) {
                                tracing::error!("failed to reactivate DRM: {e}");
                            }
                        }
                        guard
                            .backends
                            .iter()
                            .map(|(n, b)| (*n, b.surfaces.keys().copied().collect()))
                            .collect()
                    };
                    // Re-render every surface on the next loop turn.
                    for (node, crtcs) in nodes {
                        for crtc in crtcs {
                            schedule_render(&udev, node, crtc, Duration::ZERO);
                        }
                    }
                }
            })
            .map_err(|e| format!("failed to register session notifier: {e}"))?;
    }

    // Publish our socket so spawned clients connect to us, not a host compositor.
    // XDG_SESSION_TYPE=wayland steers Chromium/Electron `auto` backend detection
    // onto Wayland -- it keys off the session type, not WAYLAND_DISPLAY, and would
    // otherwise inherit `tty` from the TTY launch and land on XWayland.
    // SAFETY: set once at startup before any client threads exist (see winit).
    unsafe {
        std::env::set_var("WAYLAND_DISPLAY", &data.socket_name);
        std::env::set_var("XDG_SESSION_TYPE", "wayland");
    }

    tracing::info!("udev backend up ({} device(s))", udev.borrow().backends.len());
    Ok(())
}

/// A DRM device appeared (or was present at startup): open it through the
/// session, build its GBM allocator + DRM output manager, register a VBlank
/// source, then scan its connectors.
fn device_added(
    udev: &Rc<RefCell<UdevData>>,
    data: &mut RubixState,
    node: DrmNode,
    path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut guard = udev.borrow_mut();
    let udev_data = &mut *guard;

    // Open the device fd through the session (so VT-switch can revoke it).
    let fd = udev_data.session.open(
        path,
        OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
    )?;
    let fd = DrmDeviceFd::new(DeviceFd::from(fd));

    let (drm, drm_notifier) = DrmDevice::new(fd.clone(), true)?;
    let gbm = GbmDevice::new(fd)?;

    // EGL: find the render node backing this device.
    let egl_display = unsafe { EGLDisplay::new(gbm.clone())? };
    let egl_device = EGLDevice::device_for_display(&egl_display)?;
    let render_node = egl_device
        .try_get_render_node()
        .ok()
        .flatten()
        .unwrap_or(node);
    udev_data.gpus.as_mut().add_node(render_node, gbm.clone())?;

    // Query the render formats from a renderer on this node.
    let render_formats = udev_data
        .gpus
        .single_renderer(&render_node)?
        .as_mut()
        .egl_context()
        .dmabuf_render_formats()
        .clone();

    // Advertise zwp_linux_dmabuf_v1 once, seeded with the primary GPU's render
    // formats, so GPU-accelerated Wayland clients can hand us dmabuf buffers
    // instead of falling back to SHM. Only the primary node's formats are
    // published; secondary GPUs (if any) aren't wired into the dmabuf path yet.
    if data.dmabuf_global.is_none() && render_node == udev_data.primary_gpu {
        // Advertise dmabuf v4 with *default feedback* naming the primary render
        // node as the main device. XWayland (and other feedback-aware clients)
        // read this to discover the GPU and enable glamor/DRI3 -- without it,
        // XWayland reports "dri3 extension not supported" and GPU-accelerated X
        // clients (Chromium/Electron, games) can't create a presentation surface,
        // so they never paint. v3-and-lower clients still receive the format list
        // from the main tranche, so native dmabuf clients are unaffected.
        let feedback = DmabufFeedbackBuilder::new(render_node.dev_id(), render_formats.clone())
            .build()
            .expect("failed to build dmabuf default feedback");
        let global = data
            .dmabuf_state
            .create_global_with_default_feedback::<RubixState>(&data.display_handle, &feedback);
        data.dmabuf_global = Some(global);
        tracing::info!(
            "advertised zwp_linux_dmabuf_v1 with default feedback ({} formats, main device {})",
            render_formats.iter().count(),
            render_node.dev_id(),
        );
    }

    let allocator = GbmAllocator::new(
        gbm.clone(),
        GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
    );
    let framebuffer_exporter = GbmFramebufferExporter::new(gbm.clone(), render_node.into());

    let drm_output_manager = DrmOutputManager::new(
        drm,
        allocator,
        framebuffer_exporter,
        Some(gbm),
        SUPPORTED_FORMATS.iter().copied(),
        render_formats,
    );

    // VBlank source for this device. Each flip completion lands here as
    // DrmEvent::VBlank(crtc); we finish the frame and schedule the next.
    let udev_for_vblank = udev.clone();
    let registration_token = udev_data
        .loop_handle
        .insert_source(drm_notifier, move |event, metadata, _data| match event {
            DrmEvent::VBlank(crtc) => {
                frame_finish(&udev_for_vblank, node, crtc, metadata);
            }
            DrmEvent::Error(error) => {
                tracing::error!("DRM error on {node}: {error:?}");
            }
        })
        .map_err(|e| format!("failed to register DRM VBlank source: {e}"))?;

    udev_data.backends.insert(
        node,
        BackendData {
            drm_output_manager,
            drm_scanner: DrmScanner::new(),
            surfaces: HashMap::new(),
            render_node,
            registration_token,
        },
    );

    drop(guard);
    device_changed(udev, data, node);
    Ok(())
}

/// Re-scan a device's connectors, creating/tearing down outputs to match.
fn device_changed(udev: &Rc<RefCell<UdevData>>, data: &mut RubixState, node: DrmNode) {
    let scan = {
        let mut guard = udev.borrow_mut();
        let Some(backend) = guard.backends.get_mut(&node) else {
            return;
        };
        match backend
            .drm_scanner
            .scan_connectors(backend.drm_output_manager.device())
        {
            Ok(scan) => scan,
            Err(e) => {
                tracing::warn!("connector scan failed on {node}: {e}");
                return;
            }
        }
    };

    for event in scan {
        match event {
            DrmScanEvent::Connected {
                connector,
                crtc: Some(crtc),
            } => connector_connected(udev, data, node, connector, crtc),
            DrmScanEvent::Disconnected {
                crtc: Some(crtc), ..
            } => connector_disconnected(udev, data, node, crtc),
            _ => {}
        }
    }
}

/// A DRM device went away: tear down its outputs, drop it from the renderer, and
/// remove its VBlank source.
fn device_removed(udev: &Rc<RefCell<UdevData>>, data: &mut RubixState, node: DrmNode) {
    let crtcs: Vec<crtc::Handle> = {
        let guard = udev.borrow();
        match guard.backends.get(&node) {
            Some(b) => b.surfaces.keys().copied().collect(),
            None => return,
        }
    };
    for crtc in crtcs {
        connector_disconnected(udev, data, node, crtc);
    }

    let mut guard = udev.borrow_mut();
    let udev_data = &mut *guard;
    if let Some(backend) = udev_data.backends.remove(&node) {
        udev_data.gpus.as_mut().remove_node(&backend.render_node);
        udev_data.loop_handle.remove(backend.registration_token);
        tracing::info!("removed DRM device {node}");
    }
}

/// A connector came up on a CRTC: pick its preferred mode, build a smithay
/// `Output`, map it into the space, initialize the `DrmOutput`, and kick the
/// first render.
fn connector_connected(
    udev: &Rc<RefCell<UdevData>>,
    data: &mut RubixState,
    node: DrmNode,
    connector: connector::Info,
    crtc: crtc::Handle,
) {
    let mut guard = udev.borrow_mut();
    let udev_data = &mut *guard;
    let Some(backend) = udev_data.backends.get_mut(&node) else {
        return;
    };

    let output_name = format!(
        "{}-{}",
        connector.interface().as_str(),
        connector.interface_id()
    );
    tracing::info!("connector {output_name} connected on {node}");

    // Preferred mode, else the first advertised. If config pins a mode for this
    // connector, prefer the DRM mode matching those dimensions when one exists;
    // otherwise fall through to the same preferred/first selection.
    let output_config = data
        .config
        .outputs
        .iter()
        .find(|o| o.name == output_name);
    let output_hdr = output_config.map(|o| o.hdr).unwrap_or(false);
    let drm_mode = *output_config
        .and_then(|o| o.mode)
        .and_then(|(w, h)| {
            connector
                .modes()
                .iter()
                .find(|m| m.size() == (w as u16, h as u16))
        })
        .unwrap_or_else(|| {
            connector
                .modes()
                .iter()
                .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
                .unwrap_or_else(|| &connector.modes()[0])
        });
    let wl_mode = WlMode::from(drm_mode);

    // Position: explicit config entry wins; otherwise auto-layout left-to-right
    // by summing the widths of outputs already mapped into the space, so an
    // unconfigured connector never collides at the same origin as another.
    let position = match output_config {
        Some(o) => o.position,
        None => {
            let x: i32 = data
                .space
                .outputs()
                .map(|o| data.space.output_geometry(o).map_or(0, |geo| geo.size.w))
                .sum();
            (x, 0)
        }
    };

    let (phys_w, phys_h) = connector.size().unwrap_or((0, 0));
    let output = Output::new(
        output_name,
        PhysicalProperties {
            size: (phys_w as i32, phys_h as i32).into(),
            subpixel: Subpixel::Unknown,
            make: "Rubix".into(),
            model: "DRM".into(),
            // `display-info` (EDID name/serial parsing) is disabled -- see the
            // `default-features = false` comment on `smithay-drm-extras` in
            // Cargo.toml -- so there's no EDID serial to read here.
            serial_number: "Unknown".into(),
        },
    );
    let global = output.create_global::<RubixState>(&data.display_handle);
    output.set_preferred(wl_mode);
    let transform = output_config.map(|o| o.transform).unwrap_or(Transform::Normal);
    output.change_current_state(Some(wl_mode), Some(transform), None, Some(position.into()));
    data.space.map_output(&output, position);
    data.bind_output_monitor(&output);

    // NVIDIA breaks with overlay planes assigned -- clear them before init.
    let mut planes = backend
        .drm_output_manager
        .device()
        .planes(&crtc)
        .expect("failed to query planes");
    if let Ok(driver) = backend.drm_output_manager.device().get_driver() {
        let name = driver.name().to_string_lossy().to_lowercase();
        let desc = driver.description().to_string_lossy().to_lowercase();
        if name.contains("nvidia") || desc.contains("nvidia") {
            tracing::info!("NVIDIA driver: clearing overlay planes on {crtc:?}");
            planes.overlay = vec![];
        }
    }

    // Acquire a renderer to initialize the output's swapchain against.
    let mut renderer = udev_data
        .gpus
        .single_renderer(&backend.render_node)
        .expect("failed to get renderer for output init");

    let drm_output = match backend.drm_output_manager.lock().initialize_output::<
        _,
        SpaceRenderElements<RubixRenderer<'_>, <Window as smithay::backend::renderer::element::AsRenderElements<RubixRenderer<'_>>>::RenderElement>,
    >(
        crtc,
        drm_mode,
        &[connector.handle()],
        &output,
        Some(planes),
        &mut renderer,
        &DrmOutputRenderElements::default(),
    ) {
        Ok(o) => o,
        Err(e) => {
            tracing::error!("failed to initialize output on {crtc:?}: {e}");
            return;
        }
    };

    // Per-surface dmabuf scanout tranche (Spec B): tells this output's client
    // how to allocate a buffer the display controller can scan out directly,
    // as opposed to the render-optimal-only formats the default global
    // (above, in `device_added`) advertises. Built once here, at bringup,
    // inside `with_compositor` so `compositor.surface()` (the live
    // `DrmSurface`, with its negotiated plane formats) is reachable -- same
    // accessor anvil uses at udev.rs:1035-1045.
    let dmabuf_feedback = drm_output.with_compositor(|compositor| {
        get_surface_dmabuf_feedback(
            udev_data.primary_gpu,
            node,
            &mut udev_data.gpus,
            compositor.surface(),
        )
    });
    if dmabuf_feedback.is_none() {
        tracing::warn!(
            "failed to build dmabuf scanout feedback for {}; clients on this output will only see \
             render-optimal formats",
            output.name(),
        );
    }

    // Same accessor, kept separately and unintersected -- see the field's doc.
    let primary_plane_formats = drm_output
        .with_compositor(|compositor| compositor.surface().plane_info().formats.clone());

    // `wl_mode.refresh` is in mHz (60_000 == 60 Hz, matching winit.rs), so the
    // frame period is 1000 / refresh seconds. The guard avoids a `from_secs_f64`
    // panic (inf) on a degenerate mode that reports no refresh.
    let frame_duration = if wl_mode.refresh > 0 {
        Duration::from_secs_f64(1_000.0 / wl_mode.refresh as f64)
    } else {
        Duration::from_secs_f64(1.0 / 60.0)
    };

    // HDR groundwork: gated per-output, default off. Only reached when the
    // config explicitly opts this connector in.
    if output_hdr {
        // Diagnostic only: confirm the connector actually negotiated a
        // 10-bit scanout format -- PQ over 8-bit bands severely. Does not
        // change SUPPORTED_FORMATS or negotiation; just logs what was
        // chosen so the user can check the journal.
        let scanout_format = drm_output.format();
        if matches!(scanout_format, Fourcc::Abgr8888 | Fourcc::Argb8888) {
            tracing::warn!(
                "HDR output {}: negotiated 8-bit scanout format {scanout_format:?} \
                 (10-bit was offered) -- PQ will band",
                output.name(),
            );
        } else {
            tracing::info!(
                "HDR output {}: negotiated scanout format = {scanout_format:?}",
                output.name(),
            );
        }
        set_hdr_output_properties(&drm_output);
    }

    backend.surfaces.insert(
        crtc,
        SurfaceData {
            output,
            global: Some(global),
            drm_output,
            frame_duration,
            hdr: output_hdr,
            hdr_capable: output_hdr,
            hdr_shaders: None,
            hdr_offscreen: None,
            applied_connector_hdr: None,
            last_scanout_promoted: None,
            last_scanout_candidate: None,
            dmabuf_feedback,
            primary_plane_formats,
        },
    );

    drop(guard);
    // First frame on the next loop turn (also does the initial tiling pass).
    data.apply_layout();
    schedule_render(udev, node, crtc, Duration::ZERO);
}

/// HDR Phase 1, Tier 3: apply the `Colorspace` and `HDR_OUTPUT_METADATA`
/// connector properties for an output opted into HDR (`OutputConfig::hdr`).
///
/// BLOCKED: Smithay 0.7's `DrmSurface`/`DrmCompositor`/`DrmOutput` (see
/// `smithay-0.7.0/src/backend/drm/{surface,compositor,output}/mod.rs`) expose
/// no API to add arbitrary connector properties to the atomic commit they
/// drive. `AtomicDrmSurface` (`surface/atomic.rs`) hardcodes exactly one
/// connector property per commit (`CRTC_ID`) via a private `connector_props`
/// map built inside `commit()`/`page_flip()`; there is no builder, callback,
/// or extension point for `Colorspace`/`HDR_OUTPUT_METADATA`.
///
/// The lower layer (`drm-rs`'s `Device` trait, already in scope here as
/// `BaseDrmDevice`) *does* expose `create_property_blob` and `set_property`
/// directly on the device fd. But calling those here would mean issuing a
/// legacy `DRM_IOCTL_MODE_OBJ_SETPROPERTY` outside Smithay's atomic commit
/// cycle: on atomic-only KMS drivers (nouveau/amdgpu/i915, i.e. anything
/// Rubix targets) the kernel does not accept out-of-band single-property
/// sets for objects under atomic control, and even where it did, the next
/// `AtomicDrmSurface::commit()` recomputes its own tracked connector property
/// set with no knowledge of ours -- a race with undefined ordering against
/// Smithay's internal state, not a supported integration. That is the
/// "fighting the commit cycle" case the spec calls out to stop at.
///
/// What full integration needs from Smithay: either (a) a public hook to
/// register extra `(connector, property_name, value)` entries that
/// `AtomicDrmSurface`/`DrmCompositor` folds into every atomic commit/test
/// alongside its own managed properties, or (b) upstream support for
/// `Colorspace`/`HDR_OUTPUT_METADATA` as first-class `DrmOutput` config,
/// mirroring how `use_vrr`/`VRR_ENABLED` is already handled internally.
/// Neither exists in 0.7. Revisit if/when Smithay grows one (see
/// smithay#tracking-hdr upstream, or a version bump) -- or when this project
/// decides a raw parallel (non-atomic) property poke is an acceptable risk.
///
/// Probes the connector's colorspace/HDR-metadata/max-bpc support through the
/// `DrmCompositor` (the only path that can fold these properties into its
/// atomic commit -- see `RubixDrmOutput::with_compositor`) and, if supported,
/// stages `crate::hdr::default_hdr_color_state()` for the next commit via
/// `use_color_state`. Never panics: any missing support or fork-call error is
/// logged and the output is left on its current (SDR) color state.
fn set_hdr_output_properties(drm_output: &RubixDrmOutput) {
    drm_output.with_compositor(|comp| {
        let Some(conn) = comp.current_connectors().into_iter().next() else {
            tracing::warn!("HDR: output has no connectors attached; leaving HDR unset");
            return;
        };

        let colorspaces = match comp.supported_colorspaces(conn) {
            Ok(cs) => cs,
            Err(e) => {
                tracing::warn!("HDR: failed to query supported colorspaces for {conn:?}: {e}");
                return;
            }
        };
        if !colorspaces.contains(&Colorspace::Bt2020Rgb) {
            tracing::warn!(
                "HDR requested for {conn:?} but BT.2020 RGB colorspace is unsupported \
                 (supported: {colorspaces:?}); leaving output SDR"
            );
            return;
        }

        match comp.hdr_metadata_supported(conn) {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!(
                    "HDR requested for {conn:?} but HDR_OUTPUT_METADATA is unsupported; \
                     leaving output SDR"
                );
                return;
            }
            Err(e) => {
                tracing::warn!("HDR: failed to query HDR metadata support for {conn:?}: {e}");
                return;
            }
        }

        let mut desired = crate::hdr::default_hdr_color_state();
        match comp.max_bpc_range(conn) {
            Ok(Some(range)) => {
                if let Some(max_bpc) = desired.max_bpc {
                    if max_bpc > *range.end() {
                        tracing::info!(
                            "HDR: clamping max_bpc {max_bpc} to connector {conn:?} range end {}",
                            range.end()
                        );
                        desired.max_bpc = Some(*range.end());
                    }
                }
            }
            Ok(None) => {
                tracing::info!(
                    "HDR: {conn:?} exposes no max bpc range; proceeding without a bpc request"
                );
                desired.max_bpc = None;
            }
            Err(e) => {
                tracing::warn!("HDR: failed to query max bpc range for {conn:?}: {e}");
                return;
            }
        }

        if comp.pending_color_state() != desired {
            match comp.use_color_state(desired) {
                Ok(()) => tracing::info!("HDR: staged BT.2020/HDR color state for {conn:?}"),
                Err(e) => tracing::warn!(
                    "HDR: failed to stage color state for {conn:?}: {e}; leaving output SDR"
                ),
            }
        }
    });
}

/// Revert a connector to the SDR default color state (`Colorspace::Default`
/// i.e. sRGB/BT.709, no HDR metadata, no max_bpc request). Mirrors
/// `set_hdr_output_properties`'s `with_compositor` shape. `use_color_state`
/// stages for the next atomic commit and is safe to call repeatedly at
/// runtime -- no re-init needed.
fn set_sdr_output_properties(drm_output: &RubixDrmOutput) {
    drm_output.with_compositor(|comp| {
        let desired = ConnectorColorState::default();
        if comp.pending_color_state() != desired {
            match comp.use_color_state(desired) {
                Ok(()) => tracing::info!("HDR: reverted connector to SDR default color state"),
                Err(e) => tracing::warn!("HDR: failed to revert to SDR color state: {e}"),
            }
        }
    });
}

/// Toggle live HDR on/off on every HDR-capable output. Flips `SurfaceData::hdr`
/// and stages the corresponding connector color state (HDR or SDR default),
/// then forces a repaint so the staged state actually reaches the next atomic
/// commit. Outputs that were never HDR-capable (`hdr_capable == false`, e.g. a
/// non-HDR HDMI strip) are left untouched.
///
/// Borrow discipline matches `nudge_all_renders`: mutate surfaces and collect
/// `(DrmNode, crtc::Handle)` targets inside a short `udev.borrow_mut()`, then
/// drop the borrow before calling `schedule_render` (which re-borrows `udev`
/// for the loop handle).
///
/// Returns the toggled outputs so the caller (`RubixState::toggle_hdr`) can
/// notify their bound `wp_color_management_output_v1` objects
/// (`ColorManagementState::output_description_changed`) to re-query
/// `description_for_output` -- without needing a second borrow of `udev`
/// alongside `self.color_management_state`.
pub(crate) fn toggle_hdr(udev: &Rc<RefCell<UdevData>>) -> Vec<Output> {
    let (targets, outputs): (Vec<(DrmNode, crtc::Handle)>, Vec<Output>) = {
        let mut guard = udev.borrow_mut();
        let mut targets = Vec::new();
        let mut outputs = Vec::new();
        for (node, backend) in guard.backends.iter_mut() {
            for (crtc, surface) in backend.surfaces.iter_mut() {
                if !surface.hdr_capable {
                    continue;
                }
                // `render_surface` is the sole owner of connector color state
                // (see `SurfaceData::applied_connector_hdr`); this only flips
                // the logical flag and schedules a render below, which picks
                // the transition up on the next frame.
                surface.hdr = !surface.hdr;
                tracing::info!(
                    "HDR toggle: {} is now {}",
                    surface.output.name(),
                    if surface.hdr { "HDR" } else { "SDR" }
                );
                targets.push((*node, *crtc));
                outputs.push(surface.output.clone());
            }
        }
        (targets, outputs)
    };

    if targets.is_empty() {
        tracing::info!("HDR toggle: no HDR-capable output");
        return outputs;
    }

    for (node, crtc) in targets {
        schedule_render(udev, node, crtc, Duration::ZERO);
    }

    outputs
}

/// A connector on a CRTC went away: unmap and drop its output.
fn connector_disconnected(
    udev: &Rc<RefCell<UdevData>>,
    data: &mut RubixState,
    node: DrmNode,
    crtc: crtc::Handle,
) {
    let mut guard = udev.borrow_mut();
    let Some(backend) = guard.backends.get_mut(&node) else {
        return;
    };
    let Some(surface) = backend.surfaces.remove(&crtc) else {
        return;
    };

    data.space.unmap_output(&surface.output);
    if let Some(global) = surface.global {
        data.display_handle.remove_global::<RubixState>(global);
    }
    tracing::info!("connector on {crtc:?} disconnected from {node}");
}

/// Render one surface's frame: build render elements from the space, scan them
/// out through the `DrmOutput`, and either queue the flip (on damage) or arm a
/// retry timer (idle). Frame callbacks fire to keep clients animating.
fn render(udev: &Rc<RefCell<UdevData>>, data: &mut RubixState, node: DrmNode, crtc: crtc::Handle) {
    let mut guard = udev.borrow_mut();
    let udev_data = &mut *guard;

    // Paused (VT switched away): the DRM device rejects page-flips, so rendering
    // just errors. Bail without rescheduling -- ActivateSession restarts the loop.
    if !udev_data.active {
        return;
    }

    let primary = udev_data.primary_gpu;

    let Some(backend) = udev_data.backends.get_mut(&node) else {
        return;
    };
    let render_node = backend.render_node;

    // Acquire the renderer (zero-copy on single GPU; copy across GPUs otherwise).
    let mut renderer = if primary == render_node {
        udev_data.gpus.single_renderer(&render_node)
    } else {
        let format = match backend.surfaces.get(&crtc) {
            Some(s) => s.drm_output.format(),
            None => return,
        };
        udev_data.gpus.renderer(&primary, &render_node, format)
    }
    .expect("failed to acquire renderer");

    let Some(surface) = backend.surfaces.get_mut(&crtc) else {
        return;
    };

    let animating = data.step_animations();

    let result = render_surface(surface, &mut renderer, data);

    // Service any screencopy captures against the frame we just rendered, using
    // this surface's live renderer (re-renders into its own offscreen buffer).
    if !data.pending_screencopy.is_empty() {
        let sc_output = surface.output.clone();
        crate::screencopy::fulfill_pending(data, &mut renderer, &sc_output);
    }

    let reschedule = match result {
        Ok(true) => {
            // Damage submitted; the flip's VBlank will schedule the next frame.
            false
        }
        Ok(false) => {
            // Nothing to draw -- no flip, so nothing wakes us. Arm a heartbeat.
            true
        }
      Err(ref err) => {
            tracing::warn!("render error on {crtc:?}: {err}");
            !matches!(err, SwapBuffersError::ContextLost(_))
        }
    };

    let reschedule = reschedule || (animating && !matches!(&result, Ok(true)));

    let frame_duration = surface.frame_duration;
    // Send frame callbacks so clients keep producing content.
    let start = data.start_time;
    let output = surface.output.clone();
    data.space.elements().for_each(|window| {
        window.send_frame(&output, start.elapsed(), Some(Duration::ZERO), |_, _| {
            Some(output.clone())
        })
    });
    // Layer-shell surfaces (waybar, etc.) need frame callbacks too, or they
    // paint their first buffer and freeze -- they're rendered above but were
    // never told to produce their next frame.
    {
        let map = layer_map_for_output(&output);
        for layer in map.layers() {
            layer.send_frame(&output, start.elapsed(), Some(Duration::ZERO), |_, _| {
                Some(output.clone())
            });
        }
    }

    drop(guard);

    if reschedule {
        schedule_render(udev, node, crtc, frame_duration);
    }
}

/// The pure scan-out step, factored out so `render` can hold disjoint borrows of
/// the GPU manager (via `renderer`) and the surface at once. Returns whether a
/// flip was queued (i.e. there was damage to present).
fn render_surface(
    surface: &mut SurfaceData,
    renderer: &mut RubixRenderer<'_>,
    state: &mut RubixState,
) -> Result<bool, SwapBuffersError> {
    // Z-order, top-to-bottom: overlay -> top -> ghosts -> tiled windows (space)
    // -> bottom -> background. `space_render_elements` was dropped in favor of
    // building this by hand: as of smithay 0.7 (wayland_frontend feature) it
    // already folds an output's LayerMap into its result (desktop/space/mod.rs
    // ~L599-656), which would double-render every layer surface if combined
    // with our own pass below. `Space::render_elements_for_region` gives the
    // space's contribution alone.
    let scale = 1.0_f64;

    // Cheap (no renderer work), computed once and reused for cursor
    // suppression, chrome clearing, connector color state, and the HDR
    // composite bypass below.
    let fullscreen_kind = fullscreen_scanout_target(state, &surface.output, surface.hdr_capable);

    let mut background: Vec<WaylandSurfaceRenderElement<RubixRenderer<'_>>> = Vec::new();
    let mut bottom: Vec<WaylandSurfaceRenderElement<RubixRenderer<'_>>> = Vec::new();
    let mut top: Vec<WaylandSurfaceRenderElement<RubixRenderer<'_>>> = Vec::new();
    let mut overlay: Vec<WaylandSurfaceRenderElement<RubixRenderer<'_>>> = Vec::new();
    {
        let map = layer_map_for_output(&surface.output);
        for layer in map.layers() {
            let Some(geo) = map.layer_geometry(layer) else { continue };
            let loc = geo.loc.to_physical_precise_round(scale);
            let elems = layer.render_elements::<WaylandSurfaceRenderElement<RubixRenderer<'_>>>(
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

    let space_elements: Vec<WaylandSurfaceRenderElement<RubixRenderer<'_>>> = state
        .space
        .output_geometry(&surface.output)
        .map(|geo| state.space.render_elements_for_region(renderer, &geo, scale, 1.0))
        .unwrap_or_default();

    // Ghost elements for any in-flight rotation wrap, built from
    // `active_ghosts` (populated by `step_animations` right before this call,
    // same frame). Collect the (window, pos) pairs first so the immutable
    // borrow of `state` is released before calling `render_elements`, which
    // needs the separate `&mut renderer` already in scope. Output scale is 1.0
    // here, so a logical Pos maps numerically to physical directly -- if that
    // ever changes, this needs `.to_physical_precise_round(scale)` from a
    // logical point instead. Rendered between top/overlay and the space so
    // ghosts stay above tiled windows but below chrome-style layer surfaces.
    let ghost_windows: Vec<(Window, Pos)> = state
        .active_ghosts
        .iter()
        .filter_map(|(id, pos)| state.windows.get(id).map(|w| (w.clone(), *pos)))
        .collect();
    let mut ghosts: Vec<WaylandSurfaceRenderElement<RubixRenderer<'_>>> = Vec::new();
    for (window, pos) in ghost_windows {
        ghosts.extend(window.render_elements::<WaylandSurfaceRenderElement<RubixRenderer<'_>>>(
            renderer,
            Point::<i32, Physical>::from((pos.x, pos.y)),
            Scale::from(1.0),
            1.0,
        ));
    }

    // Windows mid-Reveal, drawn scaled about their own centre. They are
    // unmapped from the Space for the tween's duration, so this list is their
    // only draw -- dropping it makes them vanish for the animation rather than
    // merely render unscaled. Same z-slot as the ghosts, for the same reason.
    let mut scaled = crate::state::reveal_scale_elements(state, renderer);

    // Exclusive fullscreen: chrome above the game (layer-shell top/overlay,
    // animation ghosts) must not render, both because it would be incorrect
    // (waybar etc. shouldn't paint over a fullscreen game) and because
    // anything above the candidate element in the final list is fatal to
    // primary-plane promotion. `bottom`/`background` are left as built --
    // they're culled by `DrmCompositor`'s opaque short-circuit and are the
    // fallback if promotion fails for some other reason.
    if fullscreen_kind.is_some() {
        top.clear();
        overlay.clear();
        ghosts.clear();
        scaled.clear();
    }

    // Cursor built last (it also needs `renderer`), same "collect before the
    // combined render call" discipline as the ghost/layer lists above so the
    // mutable borrow is released before `drm_output.render_frame` below.
    // Only the output the pointer is actually over draws it, and the location is
    // translated into that output's local space (subtract its geometry origin) --
    // otherwise every output redraws the cursor at the raw global coordinate,
    // producing a phantom cursor per extra monitor.
    //
    // The cursor is NOT suppressed under exclusive fullscreen. `DrmCompositor`
    // assigns it to the hardware cursor plane from this element list
    // (`FrameFlags::ALLOW_CURSOR_PLANE_SCANOUT`), so a cursor element does not
    // land in `primary_plane_elements` and does not block primary-plane
    // promotion. There is no separate hw-cursor path to fall back on: dropping
    // these elements drops the cursor outright, everywhere, for as long as any
    // fullscreen window covers the output.
    let output_geo = state.space.output_geometry(&surface.output);
    let cursor_elements = match output_geo {
        Some(geo) if geo.to_f64().contains(state.pointer_location) => {
            let local = state.pointer_location - geo.loc.to_f64();
            pointer_render_elements(renderer, &state.cursor_status, local, scale)
        }
        _ => Vec::new(),
    };

    // Cursor prepended -- front of the Vec is topmost, and it must draw above
    // everything else, including overlay layers.
    let mut elements: Vec<RubixRenderElement<RubixRenderer<'_>>> = Vec::new();
    elements.extend(cursor_elements);
    elements.extend(overlay.into_iter().map(RubixRenderElement::Surface));
    elements.extend(top.into_iter().map(RubixRenderElement::Surface));
    elements.extend(ghosts.into_iter().map(RubixRenderElement::Surface));
    elements.extend(scaled.into_iter().map(RubixRenderElement::Rescaled));
    elements.extend(space_elements.into_iter().map(RubixRenderElement::Surface));
    elements.extend(bottom.into_iter().map(RubixRenderElement::Surface));
    elements.extend(background.into_iter().map(RubixRenderElement::Surface));

    // Connector color state follows the exclusive-fullscreen window's own
    // declared transfer function; on the desktop (no exclusive fullscreen) it
    // follows `surface.hdr` as before. `render_surface` is the sole owner of
    // connector color state -- see `SurfaceData::applied_connector_hdr` --
    // so the transition is only staged to DRM when it actually changes.
    let desired_connector_hdr = match fullscreen_kind {
        // Exclusive fullscreen: the connector follows the content. Matched on
        // Sdr explicitly and everything else via the catch-all, rather than a
        // guard on is_hdr() -- guard arms do not count toward exhaustiveness,
        // so a future DecodeKind would compile silently into the wrong branch.
        Some(DecodeKind::Sdr) => false,
        Some(_) => surface.hdr_capable && surface.hdr,
        // Desktop: today's behaviour.
        None => surface.hdr,
    };
    if surface.applied_connector_hdr != Some(desired_connector_hdr) {
        let reason = match fullscreen_kind {
            Some(DecodeKind::Sdr) => "fullscreen-sdr",
            Some(_) => "fullscreen-hdr",
            None => "desktop",
        };
        if desired_connector_hdr {
            set_hdr_output_properties(&surface.drm_output);
        } else {
            set_sdr_output_properties(&surface.drm_output);
        }
        surface.applied_connector_hdr = Some(desired_connector_hdr);
        tracing::info!(
            "connector color state on {}: hdr = {desired_connector_hdr} ({reason})",
            surface.output.name(),
        );
    }

    // HDR Phase 2/1b: `hdr == true` outputs composite through a linear-light
    // 16F offscreen instead of drawing `elements` straight to scanout
    // (below). On ANY failure (shader compile, offscreen bind, either render
    // call) the HDR path returns `Err` and touches nothing further -- fall
    // through to the exact same `render_frame` call every non-HDR output
    // takes, degrading this one frame to SDR rather than panicking the
    // session. The non-HDR path below is intentionally untouched byte-for-
    // byte by this branch: most outputs (e.g. HDMI-A-1) never enter it.
    //
    // Phase 1b: when at least one HDR-declared surface contributes to this
    // output, the fast single-pass decode (below, unchanged since Phase 2)
    // can no longer be used -- it applies one shader override to every
    // surface. `output_has_hdr_window` is a cheap per-window check (no
    // renderer work) run first so the common case (no HDR content anywhere,
    // e.g. every normal desktop frame) takes the exact proven fast path with
    // zero extra cost. Only when it's actually needed is the more expensive
    // per-window gather (`gather_tagged_elements`) done and the z-run decode
    // (`render_surface_hdr_zrun`) used instead.
    // The HDR composite path renders into a 16F offscreen and hands
    // `render_frame` a single texture element, which makes direct scanout
    // structurally impossible. Under exclusive fullscreen the connector
    // color state has already been set above to follow the content directly
    // (`desired_connector_hdr`), so bypassing this path is correct, not just
    // an optimization:
    //   - HDR game on an HDR output: connector stays PQ/BT.2020, the client
    //     buffer is already PQ/BT.2020, no shader touches it -- an identity
    //     pass, strictly better than the composite round trip.
    //   - SDR game on an HDR output: connector drops to SDR default
    //     (sRGB/BT.709) above, so the 8-bit client buffer is interpreted
    //     correctly. HDR returns automatically on leaving fullscreen.
    //   - Non-HDR output: unchanged either way.
    // Note: `surface_decode_kind` does not recognise HLG, so HLG content maps to
    // `DecodeKind::Sdr` -- an HLG fullscreen client drives the connector to SDR.
    // Acceptable: it degrades to today's behaviour rather than misrendering.
    //
    // Which fullscreen kinds may SKIP the HDR composite pass is a correctness
    // question, not an optimisation: bypassing means the client's buffer is
    // scanned out as-is, which is only right when it is ALREADY in the
    // connector's encoding.
    //   - Sdr        -> connector was just driven to SDR; sRGB buffer matches. Skip.
    //   - HdrPq      -> connector is BT.2020/PQ; the buffer already is BT.2020/PQ. Skip.
    //   - WindowsScrgb -> connector is BT.2020/PQ but the buffer is LINEAR BT.709
    //                   extended-range. Handing that to a PQ connector unconverted
    //                   would misrender badly. It MUST go through decode+encode.
    let fullscreen_needs_conversion = matches!(fullscreen_kind, Some(DecodeKind::WindowsScrgb));
    if surface.hdr && (fullscreen_kind.is_none() || fullscreen_needs_conversion) {
        let hdr_result = if output_has_hdr_window(state, &surface.output) {
            let tagged = gather_tagged_elements(state, renderer, &surface.output, scale);
            render_surface_hdr_zrun(surface, renderer, &tagged, state.sdr_white_nits)
        } else {
            render_surface_hdr(surface, renderer, &elements, state.sdr_white_nits)
        };
        match hdr_result {
            Ok(presented) => return Ok(presented),
            Err(err) => {
                tracing::warn!(
                    "HDR pipeline failed on {:?}, falling back to SDR for this frame: {err}",
                    surface.output.name()
                );
            }
        }
    }

    // Let the primary plane take a fullscreen buffer whose format differs from
    // the swapchain's.
    //
    // `FrameFlags::DEFAULT` permits primary-plane scanout only for an element
    // matching the swapchain slot's format exactly (compositor/mod.rs:3019). On
    // an HDR-capable output we negotiate a 10-bit slot (AB30), so every ordinary
    // 8-bit client buffer -- XR24, which is what SDR clients overwhelmingly
    // allocate -- fails that equality and is composited without scanout ever
    // being attempted. It surfaces as `Rendering { reason: None }`, which reads
    // like "not a candidate" rather than "rejected", and is how the HDR work
    // silently made direct scanout unreachable for SDR fullscreen clients.
    //
    // Scoped to frames with a fullscreen target on purpose. A fullscreen client
    // owns the output, and the connector has already been driven to match its
    // decode kind by this point, so adopting its buffer format on the plane is
    // the intended outcome. Granting it on ordinary desktop frames would let any
    // client's format reconfigure the plane -- churn for no gain, since a
    // multi-window desktop composites regardless.
    let frame_flags = if fullscreen_kind.is_some() {
        FrameFlags::DEFAULT | FrameFlags::ALLOW_PRIMARY_PLANE_SCANOUT_ANY
    } else {
        FrameFlags::DEFAULT
    };

    let frame = surface
        .drm_output
        .render_frame(renderer, &elements, [0.1, 0.1, 0.1, 1.0], frame_flags)
        .map_err(|err| match err {
            smithay::backend::drm::compositor::RenderFrameError::PrepareFrame(e) => {
                SwapBuffersError::from(e)
            }
            smithay::backend::drm::compositor::RenderFrameError::RenderFrame(
                OutputDamageTrackerError::Rendering(e),
            ) => SwapBuffersError::from(e),
            other => {
                SwapBuffersError::TemporaryFailure(Box::new(std::io::Error::other(format!("{other:?}"))))
            }
        })?;

    // Scanout diagnostic: log whether primary-plane promotion actually
    // happened, only on change so it's silent in steady state. Runs before
    // the `is_empty` early return below, or a promotion change on an empty
    // frame would be missed.
    let promoted = matches!(frame.primary_element, PrimaryPlaneElement::Element(_));
    let candidate = fullscreen_candidate_id(state, &surface.output);
    if surface.last_scanout_promoted != Some(promoted) || surface.last_scanout_candidate != candidate
    {
        surface.last_scanout_promoted = Some(promoted);
        surface.last_scanout_candidate = candidate;
        let output_name = surface.output.name();
        let element_count = elements.len();
        // `tagged` distinguishes "client says it is SDR" from "client said
        // nothing", which `fullscreen_kind` collapses. The one-shot warning in
        // color_management only ever reports the FIRST frame, and image
        // descriptions are created asynchronously -- so a first-frame miss
        // proves nothing, and silence afterwards was reading as failure twice
        // over. This line fires on every candidate change, so a client that
        // tags late shows up here.
        let tagged = fullscreen_candidate_surface(state, &surface.output)
            .map(|s| crate::color_management::surface_description_present(&s));
        tracing::info!(
            "direct-scanout on {output_name}: promoted = {promoted} ({element_count} elements,              fullscreen = {fullscreen_kind:?}, candidate tagged = {tagged:?})",
        );
        if !promoted && fullscreen_kind.is_some() {
            for (id, st) in frame.states.states.iter() {
                tracing::info!("  element {id:?}: {:?}", st.presentation_state);
            }
            // Spec B: distinguish "client never got a scanout tranche to
            // allocate against" from "client got one and ignored it" --
            // otherwise a missing/empty tranche and a stubborn client look
            // identical in the log above.
            match surface.dmabuf_feedback.as_ref() {
                Some(fb) => tracing::info!(
                    "  dmabuf scanout tranche: {} format(s)",
                    fb.scanout_format_count,
                ),
                None => tracing::info!("  dmabuf scanout feedback: none built for this output"),
            }
            log_scanout_format_mismatch(state, surface, &frame.states);
        }
    }

    // Spec B: keep each surface's primary-scanout-output tracking current
    // with *this* frame's `RenderElementStates`, then tell it which dmabuf
    // feedback (render-optimal vs scanout-capable) to allocate against next.
    // Mirrors anvil's `update_primary_scanout_output` + `post_repaint`
    // (`state.rs:1079-1110`, `906-962`) in the same two-step order: the
    // compare fn needs this frame's states, and `select_dmabuf_feedback`
    // needs the primary-scanout-output write to have already landed.
    //
    // Placement: inline here, not threaded out to the `send_frame` loop in
    // `render()` (udev.rs ~1040-1073) -- `render_surface` already owns `&mut
    // RubixState` (`state`) and `&mut SurfaceData` (`surface`) as disjoint
    // parameters, so this needs no new borrow and no change to
    // `render_surface`'s `Result<bool, _>` return type, which the HDR
    // early-returns above depend on. Feedback is sent only from this, the
    // plain `render_frame` path: the HDR composite path
    // (`render_surface_hdr[_zrun]`) already returned above, and its output is
    // a single opaque texture element that can never be scanned out, so
    // there's nothing meaningful to select between there.
    state.space.elements().for_each(|window| {
        window.with_surfaces(|wl_surface, states| {
            update_surface_primary_scanout_output(
                wl_surface,
                &surface.output,
                states,
                None,
                &frame.states,
                default_primary_scanout_output_compare,
            );
        });
        if let Some(fb) = surface.dmabuf_feedback.as_ref() {
            window.send_dmabuf_feedback(&surface.output, surface_primary_scanout_output, |wl_surface, _| {
                select_dmabuf_feedback(wl_surface, &frame.states, &fb.render_feedback, &fb.scanout_feedback)
            });
        }
    });
    {
        let map = layer_map_for_output(&surface.output);
        for layer in map.layers() {
            layer.with_surfaces(|wl_surface, states| {
                update_surface_primary_scanout_output(
                    wl_surface,
                    &surface.output,
                    states,
                    None,
                    &frame.states,
                    default_primary_scanout_output_compare,
                );
            });
            if let Some(fb) = surface.dmabuf_feedback.as_ref() {
                layer.send_dmabuf_feedback(&surface.output, surface_primary_scanout_output, |wl_surface, _| {
                    select_dmabuf_feedback(wl_surface, &frame.states, &fb.render_feedback, &fb.scanout_feedback)
                });
            }
        }
    }

    if frame.is_empty {
        return Ok(false);
    }

    surface
        .drm_output
        .queue_frame(None)
        .map_err(Into::<SwapBuffersError>::into)?;
    Ok(true)
}

/// The fullscreen window that exclusively covers `output`, plus the decode
/// kind it declares, if any. Stricter than the old origin-point test: the
/// window's bbox must actually contain the whole output geometry, which is
/// the same condition `DrmCompositor` requires for primary-plane promotion.
/// Cheap -- no renderer work, just `Space`/surface-state lookups.
/// The `WlSurface` of the fullscreen scanout candidate on `output`.
///
/// Companion to [`fullscreen_candidate_id`]; the diagnostic needs the surface
/// itself to ask whether it carries a color-management description.
fn fullscreen_candidate_surface(
    state: &RubixState,
    output: &Output,
) -> Option<smithay::reexports::wayland_server::protocol::wl_surface::WlSurface> {
    let id = fullscreen_candidate_id(state, output)?;
    state
        .windows
        .get(&id)
        .and_then(|w| w.wl_surface())
        .map(|s| s.into_owned())
}

/// The window id `fullscreen_scanout_target` would pick for `output`.
///
/// Same containment test, returning the identity rather than the decode kind,
/// so the diagnostic can tell "a different client is the candidate now" from
/// "the same one is still failing".
fn fullscreen_candidate_id(state: &RubixState, output: &Output) -> Option<u32> {
    let output_geo = state.space.output_geometry(output)?;
    state.fullscreen_windows.iter().copied().find(|id| {
        state
            .windows
            .get(id)
            .and_then(|window| state.space.element_bbox(window))
            .is_some_and(|bbox| bbox.contains_rect(output_geo))
    })
}

fn fullscreen_scanout_target(state: &RubixState, output: &Output, hdr_capable: bool) -> Option<DecodeKind> {
    let output_geo = state.space.output_geometry(output)?;
    state.fullscreen_windows.iter().find_map(|id| {
        let window = state.windows.get(id)?;
        let bbox = state.space.element_bbox(window)?;
        if !bbox.contains_rect(output_geo) {
            return None;
        }
        let kind = window
            .wl_surface()
            .map(|s| surface_decode_kind(&s))
            .unwrap_or(DecodeKind::Sdr);
        // An HDR-capable output landing on the SDR path is the case worth
        // explaining: it is either genuinely SDR content, or an HDR client that
        // was never given a way to tag itself. Only the latter is a bug, and
        // they are identical from here without asking.
        if hdr_capable && !kind.is_hdr() {
            if let Some(surface) = window.wl_surface() {
                crate::color_management::note_untagged_fullscreen(&surface, &output.name());
            }
        }
        Some(kind)
    })
}

/// Cheap pre-check: does any window contributing to `output` currently
/// declare an HDR (ST 2084 PQ) color-management description? No renderer
/// work -- just `Space`/surface-state lookups -- so it's safe to run on
/// every frame of every `hdr = true` output, including the common case where
/// the answer is "no" (plain desktop use). Layer-shell surfaces, ghosts, and
/// the cursor never carry HDR content (compositor/shell chrome), so only
/// space windows need checking. Filters by output-region bbox overlap, same
/// as `Space::render_elements_for_region`, so a window tiled on a DIFFERENT
/// output doesn't wrongly force this one onto the slow path.
fn output_has_hdr_window(state: &RubixState, output: &Output) -> bool {
    let Some(region) = state.space.output_geometry(output) else {
        return false;
    };
    state.space.elements().any(|window| {
        state
            .space
            .element_bbox(window)
            .is_some_and(|bbox| region.overlaps(bbox))
            && window
                .wl_surface()
                .is_some_and(|s| surface_decode_kind(&s).is_hdr())
    })
}

/// Slow-path element gather for HDR Phase 1b: same front-to-back z-order as
/// `render_surface`'s plain `elements` (cursor, overlay, top, ghosts, space,
/// bottom, background), but paired with each element's [`DecodeKind`] so
/// `render_surface_hdr_zrun` can decode each surface through its own
/// transfer function. Cursor/layer-shell/ghost elements are always `Sdr`
/// (compositor/shell chrome, never client HDR content) and are rebuilt here
/// via fresh, independent `render_elements` calls -- cheap (element structs
/// referencing existing textures, no new GPU work) and avoids needing the
/// non-`Clone` `WaylandSurfaceRenderElement`/`RubixRenderElement` values
/// built by `render_surface`'s own (untouched) element collection to somehow
/// be shared between the fast and slow paths.
///
/// Space windows are gathered **per window** (`window.render_elements`, the
/// same call the ghost path already uses) rather than via the single
/// `Space::render_elements_for_region` batch, so each window's elements can
/// be tagged with that window's own root-surface decode kind
/// (`surface_decode_kind(window.wl_surface())`). The per-window geometry here
/// reproduces `render_elements_for_region`'s algorithm exactly (smithay's
/// `desktop/space/mod.rs`): iterate `Space::elements()` (back-to-front) in
/// reverse for front-to-back order, skip windows whose bbox doesn't overlap
/// the output region, and use `element_location - window.geometry().loc -
/// region.loc` (physical-rounded) as the render location -- `InnerElement::
/// render_location() - region.loc`, spelled out because `Space`'s internal
/// `InnerElement` type isn't public.
fn gather_tagged_elements<'a>(
    state: &RubixState,
    renderer: &mut RubixRenderer<'a>,
    output: &Output,
    scale: f64,
) -> Vec<(DecodeKind, RubixRenderElement<RubixRenderer<'a>>)> {
    let mut background: Vec<WaylandSurfaceRenderElement<RubixRenderer<'a>>> = Vec::new();
    let mut bottom: Vec<WaylandSurfaceRenderElement<RubixRenderer<'a>>> = Vec::new();
    let mut top: Vec<WaylandSurfaceRenderElement<RubixRenderer<'a>>> = Vec::new();
    let mut overlay: Vec<WaylandSurfaceRenderElement<RubixRenderer<'a>>> = Vec::new();
    {
        let map = layer_map_for_output(output);
        for layer in map.layers() {
            let Some(geo) = map.layer_geometry(layer) else { continue };
            let loc = geo.loc.to_physical_precise_round(scale);
            let elems = layer.render_elements::<WaylandSurfaceRenderElement<RubixRenderer<'a>>>(
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

    let output_geo = state.space.output_geometry(output);

    let mut space_tagged: Vec<(DecodeKind, WaylandSurfaceRenderElement<RubixRenderer<'a>>)> = Vec::new();
    if let Some(region) = output_geo {
        for window in state.space.elements().rev() {
            let Some(bbox) = state.space.element_bbox(window) else { continue };
            if !region.overlaps(bbox) {
                continue;
            }
            let Some(location) = state.space.element_location(window) else { continue };
            let render_location: Point<i32, Logical> = location - window.geometry().loc - region.loc;
            let kind = window
                .wl_surface()
                .map(|s| surface_decode_kind(&s))
                .unwrap_or(DecodeKind::Sdr);
            let elems = window.render_elements::<WaylandSurfaceRenderElement<RubixRenderer<'a>>>(
                renderer,
                render_location.to_physical_precise_round(scale),
                Scale::from(scale),
                1.0,
            );
            space_tagged.extend(elems.into_iter().map(|e| (kind, e)));
        }
    }

    let ghost_windows: Vec<(Window, Pos)> = state
        .active_ghosts
        .iter()
        .filter_map(|(id, pos)| state.windows.get(id).map(|w| (w.clone(), *pos)))
        .collect();
    let mut ghosts: Vec<WaylandSurfaceRenderElement<RubixRenderer<'a>>> = Vec::new();
    for (window, pos) in ghost_windows {
        ghosts.extend(window.render_elements::<WaylandSurfaceRenderElement<RubixRenderer<'a>>>(
            renderer,
            Point::<i32, Physical>::from((pos.x, pos.y)),
            Scale::from(1.0),
            1.0,
        ));
    }

    let cursor_elements: Vec<RubixRenderElement<RubixRenderer<'a>>> = match output_geo {
        Some(geo) if geo.to_f64().contains(state.pointer_location) => {
            let local = state.pointer_location - geo.loc.to_f64();
            pointer_render_elements(renderer, &state.cursor_status, local, scale)
        }
        _ => Vec::new(),
    };

    // Windows mid-Reveal, drawn scaled about their own centre. They are
    // unmapped from the Space for the tween's duration, so this list is their
    // only draw -- dropping it makes them vanish for the animation rather than
    // merely render unscaled. Same z-slot as the ghosts, for the same reason.
    let scaled = crate::state::reveal_scale_elements(state, renderer);

    // Same front-to-back order as `render_surface`'s `elements`: cursor,
    // overlay, top, ghosts, space, bottom, background.
    let mut tagged: Vec<(DecodeKind, RubixRenderElement<RubixRenderer<'a>>)> = Vec::new();
    tagged.extend(cursor_elements.into_iter().map(|e| (DecodeKind::Sdr, e)));
    tagged.extend(
        overlay
            .into_iter()
            .map(|e| (DecodeKind::Sdr, RubixRenderElement::Surface(e))),
    );
    tagged.extend(
        top.into_iter()
            .map(|e| (DecodeKind::Sdr, RubixRenderElement::Surface(e))),
    );
    tagged.extend(
        ghosts
            .into_iter()
            .map(|e| (DecodeKind::Sdr, RubixRenderElement::Surface(e))),
    );
    tagged.extend(
        scaled
            .into_iter()
            .map(|e| (DecodeKind::Sdr, RubixRenderElement::Rescaled(e))),
    );
    tagged.extend(space_tagged.into_iter().map(|(k, e)| (k, RubixRenderElement::Surface(e))));
    tagged.extend(
        bottom
            .into_iter()
            .map(|e| (DecodeKind::Sdr, RubixRenderElement::Surface(e))),
    );
    tagged.extend(
        background
            .into_iter()
            .map(|e| (DecodeKind::Sdr, RubixRenderElement::Surface(e))),
    );
    tagged
}

/// Shared HDR resource prep for both the fast (`render_surface_hdr`) and
/// slow (`render_surface_hdr_zrun`) decode paths: compile the three HDR
/// shaders once (cached on `SurfaceData::hdr_shaders` forever after) and
/// (re)allocate the linear 16F offscreen when the output's mode size has
/// changed (or on first use) -- never per frame either way. Any failure
/// (compile or bind) returns `Err` and leaves `surface.hdr_offscreen`/
/// `hdr_shaders` exactly as they were (pre-first-success: stays `None` so
/// the next frame retries from scratch; failure after caching once: caches
/// stay warm, this frame's HDR pass is just skipped).
fn ensure_hdr_resources(
    surface: &mut SurfaceData,
    renderer: &mut RubixRenderer<'_>,
) -> Result<HdrShaders, String> {
    if surface.hdr_shaders.is_none() {
        let gles: &mut GlesRenderer = renderer.as_mut();
        let shaders =
            compile_hdr_shaders(gles).map_err(|e| format!("HDR shader compile failed: {e:?}"))?;
        surface.hdr_shaders = Some(shaders);
    }
    let shaders = surface.hdr_shaders.clone().expect("just set above");

    let mode = surface
        .output
        .current_mode()
        .ok_or_else(|| "output has no mode".to_string())?;
    let phys_size = Size::<i32, BufferCoord>::from((mode.size.w, mode.size.h));
    let needs_resize = surface
        .hdr_offscreen
        .as_ref()
        .map(|o| o.size != phys_size)
        .unwrap_or(true);
    if needs_resize {
        // Drop the stale target first so its GL resources are released
        // before asking the driver for a new one of a different size.
        surface.hdr_offscreen = None;
        let mut bind_errors = Vec::new();
        for &fourcc in HDR_OFFSCREEN_FORMATS {
            match Offscreen::<GlesTexture>::create_buffer(renderer, fourcc, phys_size) {
                Ok(texture) => {
                    surface.hdr_offscreen = Some(HdrOffscreen {
                        texture,
                        size: phys_size,
                        // `Transform::Normal`, not `surface.output
                        // .current_transform()`: `elements` are already built
                        // in the same plain physical space `drm_output
                        // .render_frame` (the encode call below) expects and
                        // re-transforms internally on its own -- baking the
                        // output's transform in here too would rotate/flip
                        // twice. Mirrors `screencopy.rs::copy_output`'s
                        // proven "re-render into an offscreen of output
                        // size" pattern (screencopy.rs:343-354).
                        damage_tracker: OutputDamageTracker::new(
                            Size::<i32, Physical>::from((mode.size.w, mode.size.h)),
                            1.0,
                            Transform::Normal,
                        ),
                    });
                    break;
                }
                Err(e) => bind_errors.push(format!("{fourcc:?}: {e:?}")),
            }
        }
        if surface.hdr_offscreen.is_none() {
            return Err(format!("HDR offscreen bind failed: {}", bind_errors.join("; ")));
        }
    }
    Ok(shaders)
}

/// Shared HDR encode pass for both decode paths: the linear 16F offscreen,
/// wrapped as a single fullscreen `RubixRenderElement::Texture`, goes through
/// the SAME `drm_output.render_frame` call the non-HDR path uses, with the
/// renderer-global override set to the (Phase 1b: now bare-PQ, no uniforms)
/// encode shader. Fresh `Id` every frame on purpose: the offscreen is fully
/// re-rendered each frame, so the encode element must report full damage
/// each frame or `DrmCompositor` leaves stale/black backbuffers (see
/// `HdrOffscreen`'s doc). `FrameFlags::empty()` forces GL composition so the
/// encode shader can't be bypassed by direct-scanout promotion (see
/// `compositor/mod.rs:2880-2911`'s scanout-eligibility check).
fn encode_pass(surface: &mut SurfaceData, renderer: &mut RubixRenderer<'_>, shaders: &HdrShaders) -> Result<bool, String> {
    let texture = {
        let offscreen = surface.hdr_offscreen.as_ref().expect("just ensured above");
        offscreen.texture.clone()
    };
    let gles: &mut GlesRenderer = renderer.as_mut();
    let context_id = gles.context_id();
    let texture_element = TextureRenderElement::from_static_texture(
        Id::new(),
        context_id,
        Point::<f64, Physical>::from((0.0, 0.0)),
        texture,
        1,
        Transform::Normal,
        None,
        None,
        None,
        None,
        Kind::Unspecified,
    );
    // Phase 1b: the encode shader dropped `sdr_white_nits` (moved into the
    // SDR decode) -- no uniforms now.
    gles.set_default_tex_program_override(Some((shaders.encode.clone(), Vec::new())));
    let encode_elements = [RubixRenderElement::Texture(texture_element)];
    let render_result =
        surface
            .drm_output
            .render_frame(gles, &encode_elements, [0.0, 0.0, 0.0, 1.0], FrameFlags::empty());
    {
        let gles: &mut GlesRenderer = renderer.as_mut();
        gles.set_default_tex_program_override(None);
    }
    let frame = render_result.map_err(|e| format!("HDR encode render_frame: {e:?}"))?;
    if frame.is_empty {
        return Ok(false);
    }
    surface
        .drm_output
        .queue_frame(None)
        .map_err(|e| format!("HDR encode queue_frame: {e:?}"))?;
    Ok(true)
}

/// HDR Phase 2/1b's fast-path linear-light pipeline for one `hdr = true`
/// output with no HDR-declared surfaces on it -- the common case (plain
/// desktop use). Single decode pass over the SAME `elements` `render_surface`
/// already built, via `OutputDamageTracker::render_output`, with the
/// renderer-global texture-program override set to `DECODE_SDR` (now
/// carrying the `sdr_white_nits` uniform -- Phase 1b moved the BT.709-
/// >BT.2020 + nits scaling out of the encode shader and into this one) and
/// the solid-color transform set to `sdr_solid_transform(sdr_white_nits)`
/// (ditto). Both overrides are cleared immediately after the render call,
/// win or lose. Then the shared [`encode_pass`].
fn render_surface_hdr<'a>(
    surface: &mut SurfaceData,
    renderer: &mut RubixRenderer<'a>,
    elements: &[RubixRenderElement<RubixRenderer<'a>>],
    sdr_white_nits: f32,
) -> Result<bool, String> {
    let shaders = ensure_hdr_resources(surface, renderer)?;

    // --- decode pass: elements -> linear 16F offscreen ----------------------
    {
        let gles: &mut GlesRenderer = renderer.as_mut();
        gles.set_default_tex_program_override(Some((
            shaders.decode_sdr.clone(),
            vec![Uniform::new("sdr_white_nits", sdr_white_nits)],
        )));
        gles.set_solid_color_transform(Some(Box::new(sdr_solid_transform(sdr_white_nits))));
    }
    let decode_result: Result<(), String> = (|| {
        let offscreen = surface.hdr_offscreen.as_mut().expect("just ensured above");
        let mut fb = renderer
            .bind(&mut offscreen.texture)
            .map_err(|e| format!("HDR offscreen bind: {e:?}"))?;
        offscreen
            .damage_tracker
            .render_output(renderer, &mut fb, 0, elements, [0.0, 0.0, 0.0, 1.0])
            .map(|_| ())
            .map_err(|e| format!("HDR decode render_output: {e:?}"))
    })();
    // Always clear both overrides, win or lose -- they must never leak into
    // the next frame or into other (non-HDR) outputs sharing this renderer
    // (there is exactly one `GlesRenderer` behind `RubixRenderer<'_>` on
    // Rubix's single-GPU zero-copy configuration).
    {
        let gles: &mut GlesRenderer = renderer.as_mut();
        gles.set_default_tex_program_override(None);
        gles.set_solid_color_transform(None);
    }
    decode_result?;

    encode_pass(surface, renderer, &shaders)
}

/// HDR Phase 1b's slow-path pipeline: used only when [`output_has_hdr_window`]
/// found at least one HDR-declared surface on this output. Because the
/// shader override is renderer-global-only (`MultiFrame` doesn't expose the
/// underlying `GlesFrame` for a per-element override), each surface is
/// decoded through its own transfer function via **multiple render passes
/// grouped into maximal contiguous z-runs of equal `DecodeKind`**,
/// accumulating into the same offscreen:
///
/// 1. Partition `tagged_elements` (front-to-back) into runs, then reverse the
///    run order (and each run's own element order) for back-to-front
///    painter's-algorithm drawing -- equivalent to reversing the whole
///    sequence once, just done in two nested steps so each run's `DecodeKind`
///    stays intact.
/// 2. Bind the offscreen once; set the SDR solid-color transform (live nits)
///    once for the whole pass -- solids are never HDR.
/// 3. For each run, back-to-front: set the renderer-global override to that
///    run's decode shader (`decode_sdr` + `sdr_white_nits` uniform, or
///    `decode_hdr_pq` with no uniforms), open a fresh `MultiFrame` via
///    `renderer.render`, clear to black on the FIRST run only (subsequent
///    runs must NOT clear, so content accumulates), draw each element with
///    full per-element damage (the offscreen is redrawn fully every frame --
///    matches the fresh-`Id` full-damage behavior the encode pass relies on),
///    then drop the frame to release the renderer borrow before the next
///    run's override.
/// 4. Clear the tex override and solid transform after the pass. Then the
///    shared [`encode_pass`] (structurally unchanged, no uniforms now).
fn render_surface_hdr_zrun<'a>(
    surface: &mut SurfaceData,
    renderer: &mut RubixRenderer<'a>,
    tagged_elements: &[(DecodeKind, RubixRenderElement<RubixRenderer<'a>>)],
    sdr_white_nits: f32,
) -> Result<bool, String> {
    let shaders = ensure_hdr_resources(surface, renderer)?;

    // Maximal contiguous runs of equal `DecodeKind`, preserving `tagged_elements`'s
    // front-to-back order; reversed below (run order AND per-run element
    // order) for back-to-front drawing.
    let mut runs: Vec<(DecodeKind, Vec<&RubixRenderElement<RubixRenderer<'a>>>)> = Vec::new();
    for (kind, elem) in tagged_elements {
        match runs.last_mut() {
            Some((last_kind, elems)) if *last_kind == *kind => elems.push(elem),
            _ => runs.push((*kind, vec![elem])),
        }
    }
    runs.reverse();

    {
        let gles: &mut GlesRenderer = renderer.as_mut();
        gles.set_solid_color_transform(Some(Box::new(sdr_solid_transform(sdr_white_nits))));
    }
    let decode_result: Result<(), String> = (|| {
        let offscreen = surface.hdr_offscreen.as_mut().expect("just ensured above");
        let mut fb = renderer
            .bind(&mut offscreen.texture)
            .map_err(|e| format!("HDR z-run offscreen bind: {e:?}"))?;
        let size = Size::<i32, Physical>::from((offscreen.size.w, offscreen.size.h));
        let full_rect = Rectangle::new(Point::from((0, 0)), size);

        for (i, (kind, elems)) in runs.iter().enumerate() {
            {
                let gles: &mut GlesRenderer = renderer.as_mut();
                let prog = match kind {
                    DecodeKind::Sdr => (
                        shaders.decode_sdr.clone(),
                        vec![Uniform::new("sdr_white_nits", sdr_white_nits)],
                    ),
                    DecodeKind::HdrPq => (shaders.decode_hdr_pq.clone(), Vec::new()),
                    // No sdr_white_nits uniform: Windows-scRGB pins 1.0 to
                    // 80 cd/m² by protocol, so the SDR brightness slider must
                    // not move HDR content.
                    DecodeKind::WindowsScrgb => (shaders.decode_windows_scrgb.clone(), Vec::new()),
                };
                gles.set_default_tex_program_override(Some(prog));
            }
            let mut frame = renderer
                .render(&mut fb, size, Transform::Normal)
                .map_err(|e| format!("HDR z-run render: {e:?}"))?;
            if i == 0 {
                frame
                    .clear(Color32F::new(0.0, 0.0, 0.0, 1.0), &[full_rect])
                    .map_err(|e| format!("HDR z-run clear: {e:?}"))?;
            }
            for elem in elems.iter().rev() {
                let dst = elem.geometry(Scale::from(1.0));
                let src = elem.src();
                let damage = [Rectangle::new(Point::from((0, 0)), dst.size)];
                let opaque_regions = elem.opaque_regions(Scale::from(1.0));
                elem.draw(&mut frame, src, dst, &damage, &opaque_regions, None)
                    .map_err(|e| format!("HDR z-run draw: {e:?}"))?;
            }
            drop(frame);
        }
        Ok(())
    })();
    {
        let gles: &mut GlesRenderer = renderer.as_mut();
        gles.set_default_tex_program_override(None);
        gles.set_solid_color_transform(None);
    }
    decode_result?;

    encode_pass(surface, renderer, &shaders)
}

/// VBlank handler: a queued flip completed. Ack it to the `DrmOutput`, then
/// schedule the next frame (throttled to just under the refresh interval).
fn frame_finish(
    udev: &Rc<RefCell<UdevData>>,
    node: DrmNode,
    crtc: crtc::Handle,
    _metadata: &mut Option<DrmEventMetadata>,
) {
    let frame_duration = {
        let mut guard = udev.borrow_mut();
        let Some(backend) = guard.backends.get_mut(&node) else {
            return;
        };
        let Some(surface) = backend.surfaces.get_mut(&crtc) else {
            return;
        };
        match surface.drm_output.frame_submitted() {
            Ok(_feedback) => {}
            Err(e) => tracing::warn!("frame_submitted error on {crtc:?}: {e}"),
        }
        surface.frame_duration
    };

    // Repaint a touch before the next scanout so the buffer is ready in time.
    let delay = frame_duration.mul_f64(0.6);
    schedule_render(udev, node, crtc, delay);
}

/// Force an immediate repaint of every live CRTC. Used by screencopy: a capture
/// must produce the *next* frame even when the screen is idle, or the client
/// blocks forever. Collect the (node, crtc) targets under a short borrow, then
/// schedule outside it -- `schedule_render` re-borrows `udev` for the loop handle.
pub(crate) fn nudge_all_renders(udev: &Rc<RefCell<UdevData>>) {
    let targets: Vec<(DrmNode, crtc::Handle)> = {
        let guard = udev.borrow();
        guard
            .backends
            .iter()
            .flat_map(|(node, backend)| backend.surfaces.keys().map(move |crtc| (*node, *crtc)))
            .collect()
    };
    for (node, crtc) in targets {
        schedule_render(udev, node, crtc, Duration::ZERO);
    }
}

/// Arm a one-shot timer that renders `(node, crtc)` after `delay`. All render
/// entry points funnel through here so the borrow discipline stays in one place.
/// The timer closure captures its own `udev` clone and reaches `&mut RubixState`
/// directly through the loop's shared calloop data, exactly like every other source.
fn schedule_render(udev: &Rc<RefCell<UdevData>>, node: DrmNode, crtc: crtc::Handle, delay: Duration) {
    let loop_handle = udev.borrow().loop_handle.clone();
    let udev = udev.clone();
    let timer = if delay.is_zero() {
        Timer::immediate()
    } else {
        Timer::from_duration(delay)
    };
    if let Err(e) = loop_handle.insert_source(timer, move |_, _, data| {
        render(&udev, data, node, crtc);
        TimeoutAction::Drop
    }) {
        tracing::warn!("failed to schedule render for {crtc:?}: {e}");
    }
}

/// Why the primary plane refused the fullscreen buffer, as far as its format
/// can explain it.
///
/// The `presentation_state` lines above say promotion *failed*; they don't say
/// whether KMS could ever have accepted the buffer. That splits into two very
/// different bugs and this is what tells them apart:
///
/// - **format/modifier not in the plane's set** -- the client allocated
///   something this plane cannot scan out. The fix is upstream of KMS, in what
///   the dmabuf tranche advertised or what the client chose to ignore.
/// - **format/modifier IS in the set** -- KMS refused the atomic test for some
///   other reason (buffer size vs mode, position, scaling, an overlapping
///   element forcing composite, a plane property mismatch). Nothing about the
///   format needs changing and the search should move elsewhere.
///
/// A linear-modifier buffer is called out specially: `Linear` on a GPU that
/// wants a tiled/compressed modifier is the single most common way a client
/// lands a format that is technically listed but practically never promoted.
///
/// Diagnostic only, and only on a promotion *change* under a fullscreen target,
/// so it cannot spam a steady-state frame loop.
fn log_scanout_format_mismatch(state: &RubixState, surface: &SurfaceData, states: &RenderElementStates) {
    let Some(output_geo) = state.space.output_geometry(&surface.output) else {
        return;
    };
    for id in state.fullscreen_windows.iter() {
        let Some(window) = state.windows.get(id) else { continue };
        // Same containment test `fullscreen_scanout_target` uses, so this
        // reports on exactly the surface that was the promotion candidate.
        if !state.space.element_bbox(window).is_some_and(|bbox| bbox.contains_rect(output_geo)) {
            continue;
        }
        let Some(wl_surface) = window.wl_surface() else { continue };

        // Which dmabuf feedback this surface is being handed, and why.
        //
        // This is the bootstrap question. `select_dmabuf_feedback` only serves
        // the *scanout* tranche when the element is already `ZeroCopy`, or is
        // `Rendering` with reason `ScanoutFailed`/`FormatUnsupported`. Any other
        // state -- notably `Skipped`, and `Rendering { reason: None }` -- gets
        // the render-optimal feedback naming every format the GPU can draw into,
        // compressed modifiers included.
        //
        // If the fullscreen candidate is in one of those states, it is never
        // told the scanout formats, so it keeps allocating a buffer KMS cannot
        // take, so it is never promoted, so it is never told. That deadlock is
        // invisible in the format lines below -- they show the wrong buffer
        // without showing why the client had no way to know better.
        match states.element_render_state(Id::from(&*wl_surface)) {
            Some(st) => {
                let (tranche, why) = match st.presentation_state {
                    RenderElementPresentationState::ZeroCopy => ("scanout", "already promoted"),
                    RenderElementPresentationState::Rendering {
                        reason: Some(RenderingReason::ScanoutFailed),
                    } => ("scanout", "scanout was attempted and refused"),
                    RenderElementPresentationState::Rendering {
                        reason: Some(RenderingReason::FormatUnsupported),
                    } => ("scanout", "format rejected up front"),
                    RenderElementPresentationState::Rendering { reason: None } => {
                        ("render (DEADLOCK)", "composited without ever attempting scanout")
                    }
                    RenderElementPresentationState::Skipped => {
                        ("render (DEADLOCK)", "element skipped this frame")
                    }
                };
                tracing::info!(
                    "  window {id} element state: {:?} -> served the {tranche} tranche ({why})",
                    st.presentation_state,
                );
            }
            None => tracing::info!(
                "  window {id}: NO element render state -- surface contributed no element this \
                 frame, so it receives the default (render) feedback",
            ),
        }

        let dmabuf = with_renderer_surface_state(&wl_surface, |st| {
            st.buffer()
                .and_then(|buffer| get_dmabuf(buffer).ok().cloned())
        })
        .flatten();

        let Some(dmabuf) = dmabuf else {
            tracing::info!("  window {id}: no dmabuf attached (shm buffer -- never scannable)");
            continue;
        };

        let format = dmabuf.format();
        let supported = surface.primary_plane_formats.contains(&format);
        tracing::info!(
            "  window {id} buffer: {:?} / {:?} ({}x{}) -- primary plane supports this pair: {supported}",
            format.code,
            format.modifier,
            dmabuf.width(),
            dmabuf.height(),
        );

        if !supported {
            // Narrow it further: a listed fourcc with the wrong modifier is a
            // different (and much more likely) failure than an unlistable fourcc.
            let code_listed = surface
                .primary_plane_formats
                .iter()
                .any(|f| f.code == format.code);
            if code_listed {
                let mods: Vec<_> = surface
                    .primary_plane_formats
                    .iter()
                    .filter(|f| f.code == format.code)
                    .map(|f| f.modifier)
                    .collect();
                tracing::info!(
                    "    fourcc IS supported; the modifier is not. Plane accepts {:?} for {:?}",
                    mods,
                    format.code,
                );
            } else {
                tracing::info!("    fourcc {:?} is not on the primary plane at all", format.code);
            }
        } else if format.modifier == Modifier::Linear {
            tracing::info!(
                "    NOTE: linear modifier. Listed, but rarely promotable in practice -- \
                 if the atomic test is refusing this, the client wanting a tiled modifier \
                 is the likelier fix than anything in KMS.",
            );
        }
    }
}
