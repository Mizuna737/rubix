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

use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::allocator::Fourcc;
use smithay::backend::drm::exporter::gbm::GbmFramebufferExporter;
use smithay::backend::drm::output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements};
use smithay::backend::drm::compositor::FrameFlags;
use smithay::backend::drm::{
    Colorspace, DrmDevice, DrmDeviceFd, DrmEvent, DrmEventMetadata, DrmNode, NodeType,
};
use smithay::backend::egl::{EGLDevice, EGLDisplay};
use smithay::backend::egl::context::ContextPriority;
use smithay::backend::input::InputEvent;
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::renderer::damage::{Error as OutputDamageTrackerError, OutputDamageTracker};
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::texture::TextureRenderElement;
use smithay::backend::renderer::element::{AsRenderElements, Id, Kind};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture, Uniform};
use smithay::backend::renderer::multigpu::gbm::GbmGlesBackend;
use smithay::backend::renderer::multigpu::{GpuManager, MultiRenderer};
use smithay::backend::renderer::{Bind, Offscreen, Renderer};
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionEvent, Session};
use smithay::backend::udev::{all_gpus, primary_gpu, UdevBackend, UdevEvent};
use smithay::backend::SwapBuffersError;
use smithay::desktop::space::SpaceRenderElements;
use smithay::desktop::utils::OutputPresentationFeedback;
use smithay::desktop::{layer_map_for_output, Window};
use smithay::output::{Mode as WlMode, Output, PhysicalProperties, Subpixel};
use smithay::wayland::shell::wlr_layer::Layer;
use smithay::wayland::dmabuf::DmabufFeedbackBuilder;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{LoopHandle, RegistrationToken};
use smithay::reexports::drm::control::{connector, crtc, ModeTypeFlags};
use smithay::reexports::drm::Device as BaseDrmDevice;
use smithay::reexports::input::Libinput;
use smithay::reexports::rustix::fs::OFlags;
use smithay::reexports::wayland_server::backend::GlobalId;
use smithay::utils::{Buffer as BufferCoord, DeviceFd, Physical, Point, Scale, Size, Transform};

use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};

use crate::cursor::{pointer_render_elements, RubixRenderElement};
use crate::hdr_shaders::{compile_hdr_shaders, srgb_to_linear_solid, HdrShaders, SDR_WHITE_NITS};
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
    backends: HashMap<DrmNode, BackendData>,
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
struct BackendData {
    /// Manages DRM outputs (scanout, damage, plane assignment) for this device.
    drm_output_manager: RubixDrmOutputManager,
    /// Scans connectors→CRTCs on hotplug/probe.
    drm_scanner: DrmScanner,
    /// Live outputs on this device, keyed by CRTC.
    surfaces: HashMap<crtc::Handle, SurfaceData>,
    /// The EGL render node backing this device (== primary_gpu on single-GPU).
    render_node: DrmNode,
    /// calloop token for this device's DRM VBlank source, removed on unplug.
    registration_token: RegistrationToken,
}

/// Per-CRTC (per-monitor) state: the smithay `Output`, its global, and the
/// `DrmOutput` we scan out through.
struct SurfaceData {
    /// The logical output mapped into the space.
    output: Output,
    /// Wayland global for the output, destroyed on disconnect.
    global: Option<GlobalId>,
    /// The DRM scanout target (wraps a DrmCompositor internally).
    drm_output: RubixDrmOutput,
    /// Frame interval derived from the mode refresh, for scheduling repaint.
    frame_duration: Duration,
    /// HDR Phase 2: whether this output composites through the linear 16F
    /// pipeline (`render_surface`'s HDR branch). Set once at
    /// `connector_connected` from `OutputConfig::hdr`; never changes live
    /// (matches `set_hdr_output_properties`'s own connector-property gating).
    hdr: bool,
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
            hdr_shaders: None,
            hdr_offscreen: None,
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

    // Cursor built last (it also needs `renderer`), same "collect before the
    // combined render call" discipline as the ghost/layer lists above so the
    // mutable borrow is released before `drm_output.render_frame` below.
    // Only the output the pointer is actually over draws it, and the location is
    // translated into that output's local space (subtract its geometry origin) --
    // otherwise every output redraws the cursor at the raw global coordinate,
    // producing a phantom cursor per extra monitor.
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
    elements.extend(space_elements.into_iter().map(RubixRenderElement::Surface));
    elements.extend(bottom.into_iter().map(RubixRenderElement::Surface));
    elements.extend(background.into_iter().map(RubixRenderElement::Surface));

    // HDR Phase 2: `hdr == true` outputs composite through a linear-light 16F
    // offscreen instead of drawing `elements` straight to scanout (below).
    // On ANY failure (shader compile, offscreen bind, either render call)
    // `render_surface_hdr` returns `Err` and touches nothing further -- fall
    // through to the exact same `render_frame` call every non-HDR output
    // takes, degrading this one frame to SDR rather than panicking the
    // session. The non-HDR path below is intentionally untouched byte-for-
    // byte by this branch: most outputs (e.g. HDMI-A-1) never enter it.
    if surface.hdr {
        match render_surface_hdr(surface, renderer, &elements) {
            Ok(presented) => return Ok(presented),
            Err(err) => {
                tracing::warn!(
                    "HDR pipeline failed on {:?}, falling back to SDR for this frame: {err}",
                    surface.output.name()
                );
            }
        }
    }

    let frame = surface
        .drm_output
        .render_frame(renderer, &elements, [0.1, 0.1, 0.1, 1.0], FrameFlags::DEFAULT)
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

    if frame.is_empty {
        return Ok(false);
    }

    surface
        .drm_output
        .queue_frame(None)
        .map_err(Into::<SwapBuffersError>::into)?;
    Ok(true)
}

/// HDR Phase 2's linear-light pipeline for one `hdr = true` output. Two
/// passes over the SAME `elements` `render_surface` already built (the
/// element-collection code above is shared, unchanged, by both branches):
///
/// 1. **Decode** -- `elements` render into a cached 16F offscreen via
///    `OutputDamageTracker::render_output`, with the renderer-global
///    texture-program override set to the decode shader and the solid-color
///    transform set to `srgb_to_linear_solid`, so every surface texture AND
///    solid-color draw linearizes on the way in. Both overrides are cleared
///    immediately after the render call, win or lose (`decode_result` is
///    computed first and `?`-propagated only after the clear).
/// 2. **Encode** -- the offscreen, wrapped as a single fullscreen
///    `RubixRenderElement::Texture`, goes through the SAME
///    `drm_output.render_frame` call the non-HDR path uses, with the
///    override set to the encode shader, then cleared the same way.
///
/// Both passes reach `&mut GlesRenderer` via `renderer.as_mut()`
/// (`MultiRenderer`'s `AsMut<GlesRenderer>` impl -- confirmed in this spec's
/// Deliverable 0) because the shader-compile/override APIs live on
/// `GlesRenderer`, not `MultiRenderer`. The decode pass's offscreen bind and
/// `render_output` call go through the OUTER `renderer: &mut
/// RubixRenderer<'_>` instead, because `elements` is typed
/// `RubixRenderElement<RubixRenderer<'_>>` by the caller -- `MultiRenderer`
/// implements `Offscreen`/`Bind` generically for exactly this reason
/// (multigpu/mod.rs:1027-1124), so no type mismatch. The encode pass's
/// `drm_output.render_frame` call, by contrast, is handed the raw `&mut
/// GlesRenderer` directly: `render_frame`'s `R`/`E` are generic per call
/// (compositor/mod.rs:1688-1699, `E: RenderElement<R>, R: Renderer +
/// Bind<Dmabuf>`), and the single texture element wrapping the 16F offscreen
/// is a native `GlesTexture` (from the decode pass's `Offscreen::<GlesTexture>
/// ::create_buffer` call) -- routing it back through `MultiRenderer` would
/// mean converting to `MultiTexture` first (`MultiTexture::from_native_texture`,
/// needs a `ContextId` round-trip) for no benefit on Rubix's single-GPU,
/// always-zero-copy configuration (`RubixRenderer<'_>`'s own doc comment,
/// udev.rs:103-105).
///
/// `FrameFlags::empty()` on the encode call is the direct-scanout-promotion
/// fix flagged in the spec: `DrmCompositor::render_frame` only skips
/// assigning elements to scanout planes when `!frame_flags.intersects
/// (FrameFlags::ALLOW_SCANOUT)` (compositor/mod.rs:2887) -- i.e. it composes
/// via GL only when NONE of the `ALLOW_*` bits are set. A single fullscreen
/// opaque element with the default `FrameFlags::DEFAULT` (which sets every
/// `ALLOW_*` bit) is exactly the shape `DrmCompositor` promotes straight to
/// a scanout plane, which would skip the encode shader and scan the RAW
/// LINEAR buffer out. Passing `FrameFlags::empty()` clears every `ALLOW_*`
/// bit, so that condition holds unconditionally and the element is forced
/// through GL composition every time -- the encode shader is guaranteed to
/// run. (Confirmed by reading `render_frame`'s scanout-eligibility check
/// directly, not inferred -- see compositor/mod.rs:2880-2911.)
///
/// Any failure (compile, bind, or either render call) returns `Err`
/// immediately and leaves `surface.hdr_offscreen`/`hdr_shaders` exactly as
/// they were (a pre-first-success failure leaves them `None`, so the next
/// frame retries the compile/bind from scratch; a failure after both have
/// been cached once just skips this frame's HDR pass, the caches stay warm
/// for the next). The caller falls back to the plain
/// `drm_output.render_frame(renderer, &elements, ..)` path for that frame.
fn render_surface_hdr<'a>(
    surface: &mut SurfaceData,
    renderer: &mut RubixRenderer<'a>,
    elements: &[RubixRenderElement<RubixRenderer<'a>>],
) -> Result<bool, String> {
    // Compile once, cache forever (SurfaceData::hdr_shaders -- see its doc
    // comment for why the cache lives here and not on UdevData).
    if surface.hdr_shaders.is_none() {
        let gles: &mut GlesRenderer = renderer.as_mut();
        let shaders =
            compile_hdr_shaders(gles).map_err(|e| format!("HDR shader compile failed: {e:?}"))?;
        surface.hdr_shaders = Some(shaders);
    }
    let shaders = surface.hdr_shaders.clone().expect("just set above");

    // Allocate/resize the offscreen only when the output's mode size has
    // changed (or on first use) -- never per frame.
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

    // --- decode pass: elements -> linear 16F offscreen ----------------------
    {
        let gles: &mut GlesRenderer = renderer.as_mut();
        gles.set_default_tex_program_override(Some((shaders.decode.clone(), Vec::new())));
        gles.set_solid_color_transform(Some(Box::new(srgb_to_linear_solid)));
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

    // --- encode pass: linear 16F offscreen -> scanout ------------------------
    // Fresh `Id` every frame on purpose: the offscreen is fully re-rendered
    // each frame, so the encode element must report full damage each frame or
    // `DrmCompositor` leaves stale/black backbuffers (see `HdrOffscreen`'s doc).
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
    gles.set_default_tex_program_override(Some((
        shaders.encode.clone(),
        vec![Uniform::new("sdr_white_nits", SDR_WHITE_NITS)],
    )));
    let encode_elements = [RubixRenderElement::Texture(texture_element)];
    // See the doc comment above for why `FrameFlags::empty()` (not
    // `FrameFlags::DEFAULT`) is required here: it forces GL composition so
    // the encode shader cannot be bypassed by direct-scanout promotion.
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

