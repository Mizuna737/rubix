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
//! loop's lifetime. Every callback still receives `&mut CalloopData` for
//! `data.state` (space/seat) *plus* its own `udev.borrow_mut()` -- disjoint
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
    DrmDevice, DrmDeviceFd, DrmEvent, DrmEventMetadata, DrmNode, NodeType,
};
use smithay::backend::egl::{EGLDevice, EGLDisplay};
use smithay::backend::egl::context::ContextPriority;
use smithay::backend::input::InputEvent;
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::renderer::damage::Error as OutputDamageTrackerError;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::AsRenderElements;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::multigpu::gbm::GbmGlesBackend;
use smithay::backend::renderer::multigpu::{GpuManager, MultiRenderer};
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionEvent, Session};
use smithay::backend::udev::{all_gpus, primary_gpu, UdevBackend, UdevEvent};
use smithay::backend::SwapBuffersError;
use smithay::desktop::space::SpaceRenderElements;
use smithay::desktop::utils::OutputPresentationFeedback;
use smithay::desktop::{layer_map_for_output, Window};
use smithay::output::{Mode as WlMode, Output, PhysicalProperties, Subpixel};
use smithay::wayland::shell::wlr_layer::Layer;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{LoopHandle, RegistrationToken};
use smithay::reexports::drm::control::{connector, crtc, ModeTypeFlags};
use smithay::reexports::drm::Device as BaseDrmDevice;
use smithay::reexports::input::Libinput;
use smithay::reexports::rustix::fs::OFlags;
use smithay::reexports::wayland_server::backend::GlobalId;
use smithay::utils::{DeviceFd, Physical, Point, Scale, Transform};

use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};

use crate::state::Pos;
use crate::{CalloopData, RubixState};

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

/// Backend-wide state, shared across calloop sources via `Rc<RefCell<_>>`.
struct UdevData {
    /// Seat session -- owns the VT, brokers DRM/input device fds.
    session: LibSeatSession,
    /// The primary render GPU. For a single-GPU box this is the only node.
    primary_gpu: DrmNode,
    /// Multi-GPU renderer registry (one node registered per DRM device).
    gpus: GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>>,
    /// One entry per DRM device (GPU), keyed by its primary node.
    backends: HashMap<DrmNode, BackendData>,
    /// Loop handle for inserting per-device VBlank sources and frame timers from
    /// inside the udev/device callbacks.
    loop_handle: LoopHandle<'static, CalloopData>,
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
}

/// Entry point for the TTY/DRM backend. Mirrors `winit::init_winit`'s signature
/// so `main` can dispatch to either behind [`crate::Backend`].
pub fn init_udev(
    event_loop: &mut smithay::reexports::calloop::EventLoop<'static, CalloopData>,
    data: &mut CalloopData,
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
            data.state.process_input_event(event);
            // A VT-switch chord sets `pending_vt` inside `process_input_event`
            // (it has the xkb state); only the backend owns the session, so we
            // perform the actual switch here. Without this the compositor holds
            // DRM master forever and Ctrl+Alt+Fn is swallowed -- trapping the TTY.
            if let Some(vt) = data.state.pending_vt.take() {
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
                            if let Err(e) = backend.drm_output_manager.activate(false) {
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
    // SAFETY: set once at startup before any client threads exist (see winit).
    unsafe {
        std::env::set_var("WAYLAND_DISPLAY", &data.state.socket_name);
    }

    tracing::info!("udev backend up ({} device(s))", udev.borrow().backends.len());
    Ok(())
}

/// A DRM device appeared (or was present at startup): open it through the
/// session, build its GBM allocator + DRM output manager, register a VBlank
/// source, then scan its connectors.
fn device_added(
    udev: &Rc<RefCell<UdevData>>,
    data: &mut CalloopData,
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
fn device_changed(udev: &Rc<RefCell<UdevData>>, data: &mut CalloopData, node: DrmNode) {
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
fn device_removed(udev: &Rc<RefCell<UdevData>>, data: &mut CalloopData, node: DrmNode) {
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
    data: &mut CalloopData,
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

    // Preferred mode, else the first advertised.
    let drm_mode = *connector
        .modes()
        .iter()
        .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
        .unwrap_or_else(|| &connector.modes()[0]);
    let wl_mode = WlMode::from(drm_mode);

    let (phys_w, phys_h) = connector.size().unwrap_or((0, 0));
    let output = Output::new(
        output_name,
        PhysicalProperties {
            size: (phys_w as i32, phys_h as i32).into(),
            subpixel: Subpixel::Unknown,
            make: "Rubix".into(),
            model: "DRM".into(),
        },
    );
    let global = output.create_global::<RubixState>(&data.display_handle);
    output.set_preferred(wl_mode);
    output.change_current_state(Some(wl_mode), Some(Transform::Normal), None, Some((0, 0).into()));
    data.state.space.map_output(&output, (0, 0));

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

    let drm_output = match backend.drm_output_manager.initialize_output::<
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

    backend.surfaces.insert(
        crtc,
        SurfaceData {
            output,
            global: Some(global),
            drm_output,
            frame_duration,
        },
    );

    drop(guard);
    // First frame on the next loop turn (also does the initial tiling pass).
    data.state.apply_layout();
    schedule_render(udev, node, crtc, Duration::ZERO);
}

/// A connector on a CRTC went away: unmap and drop its output.
fn connector_disconnected(
    udev: &Rc<RefCell<UdevData>>,
    data: &mut CalloopData,
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

    data.state.space.unmap_output(&surface.output);
    if let Some(global) = surface.global {
        data.display_handle.remove_global::<RubixState>(global);
    }
    tracing::info!("connector on {crtc:?} disconnected from {node}");
}

/// Render one surface's frame: build render elements from the space, scan them
/// out through the `DrmOutput`, and either queue the flip (on damage) or arm a
/// retry timer (idle). Frame callbacks fire to keep clients animating.
fn render(udev: &Rc<RefCell<UdevData>>, data: &mut CalloopData, node: DrmNode, crtc: crtc::Handle) {
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

    let animating = data.state.step_animations();

    let result = render_surface(surface, &mut renderer, &mut data.state);

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
    let start = data.state.start_time;
    let output = surface.output.clone();
    data.state.space.elements().for_each(|window| {
        window.send_frame(&output, start.elapsed(), Some(Duration::ZERO), |_, _| {
            Some(output.clone())
        })
    });

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

    let mut elements: Vec<SpaceRenderElements<RubixRenderer<'_>, WaylandSurfaceRenderElement<RubixRenderer<'_>>>> =
        Vec::new();
    elements.extend(overlay.into_iter().map(SpaceRenderElements::Surface));
    elements.extend(top.into_iter().map(SpaceRenderElements::Surface));
    elements.extend(ghosts.into_iter().map(SpaceRenderElements::Surface));
    elements.extend(space_elements.into_iter().map(SpaceRenderElements::Surface));
    elements.extend(bottom.into_iter().map(SpaceRenderElements::Surface));
    elements.extend(background.into_iter().map(SpaceRenderElements::Surface));

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

/// Arm a one-shot timer that renders `(node, crtc)` after `delay`. All render
/// entry points funnel through here so the borrow discipline stays in one place.
/// The timer closure captures its own `udev` clone and reaches `data.state`
/// through the loop's shared `CalloopData`, exactly like every other source.
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

