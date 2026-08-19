use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    sync::Arc,
    time::Instant,
};

use smithay::{
    desktop::{layer_map_for_output, LayerSurface, PopupManager, Space, Window, WindowSurface, WindowSurfaceType},
    input::{
        pointer::CursorImageStatus,
        Seat, SeatState,
    },
    output::Output,
    reexports::{
        calloop::{generic::Generic, EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction},
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
            Display, DisplayHandle,
        },
    },
    utils::{Logical, Point, Rectangle},
    wayland::{
        color::management::ColorManagementState,
        compositor::{CompositorClientState, CompositorState},
        dmabuf::{DmabufGlobal, DmabufState},
        output::OutputManagerState,
        pointer_constraints::{PointerConstraintsState, with_pointer_constraint},
        seat::WaylandFocus,
        relative_pointer::RelativePointerManagerState,
        selection::{data_device::DataDeviceState, wlr_data_control::DataControlState},
        shell::wlr_layer::{Layer as WlrLayer, WlrLayerShellState},
        shell::xdg::XdgShellState,
        shm::ShmState,
        socket::ListeningSocketSource,
        viewporter::ViewporterState,
        xwayland_shell::XWaylandShellState,
    },
    xwayland::X11Wm,
};

use crate::{
    config::Config,
    model::{
        geometry::Rect,
        grid::Workspace,
        tiling::SplitDirection,
    },
};

// Stashed in an Output's user-data map at bind time (`bind_output_monitor`) so
// any code holding an `Output` can recover which model `Monitor` it drives,
// without a name-based lookup back through config.
#[derive(Clone, Copy)]
pub(crate) struct MonitorId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Pos {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum TweenKind { Enter, Leave, Move }

// The exiting "ghost" trajectory for a wrapping rotate Move: a second copy of
// the same surface, drawn only for the duration of the tween, sliding off the
// near edge while the Space-mapped copy slides in from the far edge.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct GhostTrack {
    from: Pos,
    to: Pos,
}

// The scale channel of a tween, used only by Reveal. `None` means "draw at
// native size", which is every tween the slide transitions produce -- keeping
// it optional is what lets Scroll/Rotate stay on the cheap Space-mapped path
// while Reveal alone takes the render-time rescale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ScaleTrack {
    from: f32,
    to: f32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Tween {
    kind: TweenKind,
    from: Pos,
    to: Pos,
    start: Instant,
    ghost: Option<GhostTrack>,
    scale: Option<ScaleTrack>,
}

// The kind of spatial-nav transition that just happened. Carries the slide
// axis/sign, which cannot be recovered from a set diff.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Transition {
    Scroll { down: bool },
    Rotate,
    // A reveal swap: the group traded into the active slot grows from nothing
    // while the displaced group shrinks away, both in place. Nothing slides,
    // because the two groups are not adjacent -- the displaced one is going to
    // a column that is off-screen by definition, so there is no edge to travel
    // toward that would read as motion rather than a glitch.
    Reveal,
}

pub struct RubixState {
    pub start_time: std::time::Instant,
    pub socket_name: OsString,
    pub display_handle: DisplayHandle,

    pub space: Space<Window>,
    pub loop_signal: LoopSignal,

    // Backend-neutral VT-switch request. The keyboard filter sets this when it
    // sees an XF86Switch_VT chord; the udev input source consumes it and calls
    // `session.change_vt` (the session lives in the backend, not here). winit
    // never reads it. `Option` so a single chord fires exactly one switch.
    pub pending_vt: Option<i32>,

    // User configuration (keybinds + layout), resolved at startup.
    pub config: Config,

    // Live SDR-white-nits value the HDR encode pass reads each frame
    // (render_surface_hdr, threaded through render_surface). Seeded from
    // `config.sdr_white_nits` in `new` and re-seeded in `reload_config`, but
    // also adjustable independently at runtime via the IncreaseSdrWhite /
    // DecreaseSdrWhite keybinds (input.rs dispatch_nav) without touching the
    // config struct. Always in [80, 300].
    pub sdr_white_nits: f32,

    // Rubix model + translation registry.
    // `workspace` is the pure tiling model, one Monitor per bound output;
    // `windows` maps its synthetic u32 ids to live Smithay handles; `next_id`
    // mints those ids (starts at 1 -- 0 is TilingNode::remove_window's
    // transient placeholder, never real).
    pub workspace: Workspace,
    pub windows: HashMap<u32, Window>,
    pub next_id: u32,

    // Wayland toplevels that have been created (initial configure sent) but
    // never committed a buffer yet -- e.g. a headless clipboard reader that
    // creates a toplevel just to read the selection and exits before mapping.
    // Kept OUT of `windows` so the model/focus/IPC never see them; promoted to
    // `windows` on first buffer commit (see xdg_shell::handle_commit).
    pub(crate) unmapped: HashMap<u32, Window>,

    // Set by any mutation that changes cube state (nav dispatch, window
    // map/unmap). The run-loop callback in main.rs checks-and-clears this once
    // per dispatch cycle to coalesce a burst of mutations into a single IPC
    // subscriber push (see ipc.rs).
    pub ipc_dirty: bool,

    // wlr-foreign-toplevel-management: bound managers plus what each window's
    // handles were last told, so `foreign_toplevel::refresh` can send deltas.
    pub(crate) foreign_toplevel: crate::foreign_toplevel::ForeignToplevelState,

    // Last tiling area we laid out into. When a layer surface (bar) changes its
    // exclusive zone, the reserved area shifts; comparing against this lets the
    // layer-commit path reflow existing windows exactly once per change instead
    // of every frame the bar repaints.
    pub reserved_bounds: Option<Rect>,

    animations: HashMap<u32, Tween>,
    pub(crate) pending_transition: Option<Transition>,
    // The exiting-ghost render positions for the in-flight frame, rebuilt fresh
    // by `step_animations` each call. Consumed by the backends right after, to
    // inject a second draw of the wrapping surface. Not Space state.
    pub(crate) active_ghosts: Vec<(u32, Pos)>,
    // Windows mid-Reveal: (id, top-left position, current scale). Rendered
    // outside the Space, wrapped in RescaleRenderElement -- the Space renders
    // in one region-wide call that cannot scale individual elements, so a
    // scaling window has to be unmapped and drawn by hand, the same way
    // rotation ghosts are.
    pub(crate) active_scales: Vec<(u32, Pos, f32)>,

    // Windows currently in fullscreen state (bypass normal tiling).
    pub(crate) fullscreen_windows: HashSet<u32>,

    // X11 windows last reported to their client as iconified. Only what the
    // client was told -- `sync_x11_iconic` diffs against it so an unchanged
    // layout pass sends no property writes.
    pub(crate) iconified: HashSet<u32>,

    // The window currently maximized, if any. Compositor-only state: unlike
    // fullscreen it involves no client protocol state, keeps its grid slot, and
    // releases itself as soon as focus moves elsewhere.
    pub(crate) maximized: Option<u32>,

    // Track the last configured geometry and fullscreen state for each window
    // to avoid redundant configure resends when layout is recomputed but geometry
    // hasn't changed (prevents flicker during exclusive fullscreen scanout).
    last_configured: HashMap<u32, (Rect, bool)>,

    // Smithay State
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub layer_shell_state: WlrLayerShellState,
    pub xwayland_shell_state: XWaylandShellState,
    // None until XWaylandEvent::Ready fires and X11Wm::start_wm succeeds.
    pub xwm: Option<X11Wm>,
    // Display number (e.g. `1` for `:1`), stored for logging/env once XWayland is ready.
    pub xdisplay: Option<u32>,
    pub shm_state: ShmState,
    pub viewporter_state: ViewporterState,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<RubixState>,
    pub data_device_state: DataDeviceState,
    pub data_control_state: DataControlState,
    pub popups: PopupManager,
    pub dmabuf_state: DmabufState,
    // Created lazily once the udev backend knows the primary GPU's render
    // formats (in `device_added`); stays None on winit.
    pub dmabuf_global: Option<DmabufGlobal>,
    // Handle back into the udev backend's renderer, so `DmabufHandler::dmabuf_imported`
    // (which fires on `RubixState`) can reach `GpuManager::single_renderer` to
    // validate an imported dmabuf. None on winit.
    pub(crate) udev_handle: Option<std::rc::Rc<std::cell::RefCell<crate::udev::UdevData>>>,

    // Pointer constraints: allows clients to lock or confine the pointer.
    // Used by games for mouselook/aim functionality.
    pub pointer_constraints_state: PointerConstraintsState,
    // Relative pointer: allows clients to receive raw pointer deltas when locked.
    pub relative_pointer_manager_state: RelativePointerManagerState,

    pub seat: Seat<Self>,

    // Single source of truth for the software cursor's logical position, kept
    // in sync by BOTH input paths (relative + absolute) in `input.rs`. The
    // cursor render element (src/cursor.rs) reads this each frame.
    pub pointer_location: Point<f64, Logical>,
    // The client-requested cursor image (named/surface/hidden), set by the
    // `SeatHandler::cursor_image` callback in handlers/mod.rs.
    pub cursor_status: CursorImageStatus,
    // Surface-local position a pointer-locking client says it is drawing its own
    // cursor at. Recorded while the lock is active so the real pointer can be
    // warped there when the lock ends (handlers/pointer_constraints.rs).
    pub(crate) cursor_position_hint: Option<(WlSurface, Point<f64, Logical>)>,
    // Live focus-follows-mouse flag. Seeded from `config.focus_follows_mouse`
    // and re-seeded by `reload_config`, but flippable on its own via the
    // ToggleFocusFollowsMouse keybind -- same seed/live split as sdr_white_nits.
    pub(crate) focus_follows_mouse: bool,

    // wlr-screencopy captures awaiting the next presented frame. Pushed by the
    // frame `copy` handler (screencopy.rs), drained by each backend's render
    // path via `screencopy::fulfill_pending` right after it presents.
    pub(crate) pending_screencopy: Vec<crate::screencopy::PendingScreencopy>,

    // HDR Phase 1b: wp_color_management_v1 state (advertised TFs/primaries,
    // known image-description identities). See `color_management::init`.
    pub(crate) color_management_state: ColorManagementState,

    // Loop handle stashed so `ColorManagementHandler::schedule_image_description_info`
    // can defer `wp_image_description_info_v1`'s events to an idle callback
    // (required -- see that impl's doc comment). Cloned from `event_loop.handle()`
    // at construction; calloop's `LoopHandle` is itself a cheap `Rc`-backed clone.
    pub(crate) loop_handle: LoopHandle<'static, RubixState>,
}

impl RubixState {
    pub fn new(event_loop: &mut EventLoop<'static, Self>, display: Display<Self>, config: Config) -> Self {
        let start_time = std::time::Instant::now();

        let dh = display.handle();
        let loop_handle = event_loop.handle();

        let compositor_state = CompositorState::new::<Self>(&dh);
        let viewporter_state = ViewporterState::new::<Self>(&dh);
        let color_management_state = crate::color_management::init(&dh);
        let pointer_constraints_state = PointerConstraintsState::new::<Self>(&dh);
        let relative_pointer_manager_state = RelativePointerManagerState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let layer_shell_state = WlrLayerShellState::new::<Self>(&dh);
        let xwayland_shell_state = XWaylandShellState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let mut seat_state = SeatState::new();
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        // wlr-data-control: lets headless clients (wl-paste, clipse, etc.) read/write
        // the clipboard without creating a real toplevel. No primary-selection support.
        let data_control_state = DataControlState::new::<Self, _>(&dh, None, |_| true);
        let popups = PopupManager::default();

        // A seat is a group of keyboards, pointer and touch devices.
        // A seat typically has a pointer and maintains a keyboard focus and a pointer focus.
        let mut seat: Seat<Self> = seat_state.new_wl_seat(&dh, "winit");

        // Notify clients that we have a keyboard, for the sake of the example we assume that keyboard is always present.
        // You may want to track keyboard hot-plug in real compositor.
        seat.add_keyboard(Default::default(), 200, 25).unwrap();

        // Notify clients that we have a pointer (mouse)
        // Here we assume that there is always pointer plugged in
        seat.add_pointer();

        // A space represents a two-dimensional plane. Windows and Outputs can be mapped onto it.
        //
        // Windows get a position and stacking order through mapping.
        // Outputs become views of a part of the Space and can be rendered via Space::render_output.
        let space = Space::default();

        let socket_name = Self::init_wayland_listener(display, event_loop);

        // Get the loop signal, used to stop the event loop
        let loop_signal = event_loop.get_signal();

        // No output is mapped into `space` yet at construction time (the
        // winit/udev backends map their output right after `RubixState::new`
        // returns), so this always takes the `(0.0, 0.0)` branch today. The
        // `space.outputs()` lookup is kept anyway so this stays correct if
        // that ordering ever changes.
        let pointer_location = space
            .outputs()
            .next()
            .and_then(|o| space.output_geometry(o))
            .map(|geo| {
                Point::<f64, Logical>::from((
                    geo.loc.x as f64 + geo.size.w as f64 / 2.0,
                    geo.loc.y as f64 + geo.size.h as f64 / 2.0,
                ))
            })
            .unwrap_or_else(|| (0.0, 0.0).into());

        // Monitors are created lazily, one per bound output, in
        // `bind_output_monitor` once each backend's output-connect path maps
        // an Output into `space` (see udev.rs/winit.rs). Empty at construction.
        let workspace = Workspace::new();

        let sdr_white_nits = config.sdr_white_nits.clamp(80.0, 300.0);
        let focus_follows_mouse = config.focus_follows_mouse;

        Self {
            start_time,
            display_handle: dh,

            space,
            loop_signal,
            pending_vt: None,
            socket_name,

            config,
            sdr_white_nits,

            workspace,
            windows: HashMap::new(),
            next_id: 1,
            unmapped: HashMap::new(),
            ipc_dirty: false,
            foreign_toplevel: Default::default(),
            reserved_bounds: None,
            animations: HashMap::new(),
            pending_transition: None,
            active_ghosts: Vec::new(),
            active_scales: Vec::new(),
            fullscreen_windows: HashSet::new(),
            iconified: HashSet::new(),
            maximized: None,
            last_configured: HashMap::new(),

            compositor_state,
            viewporter_state,
            xdg_shell_state,
            layer_shell_state,
            xwayland_shell_state,
            xwm: None,
            xdisplay: None,
            shm_state,
            output_manager_state,
            seat_state,
            data_device_state,
            data_control_state,
            popups,
            dmabuf_state: DmabufState::new(),
            dmabuf_global: None,
            udev_handle: None,
            pointer_constraints_state,
            relative_pointer_manager_state,
            seat,

            pointer_location,
            cursor_status: CursorImageStatus::default_named(),
            cursor_position_hint: None,
            focus_follows_mouse,
            pending_screencopy: Vec::new(),

            color_management_state,
            loop_handle,
        }
    }

    /// Force a repaint so a queued screencopy capture is serviced. The udev
    /// backend renders on demand (VBlank/timer) and would otherwise stay idle
    /// until something else dirties the screen, leaving the client (e.g. grim)
    /// blocked forever; winit repaints continuously and needs no nudge.
    pub(crate) fn nudge_render(&self) {
        if let Some(udev) = &self.udev_handle {
            crate::udev::nudge_all_renders(udev);
        }
    }

    /// Live A/B toggle of HDR on every HDR-capable output; see
    /// `crate::udev::toggle_hdr`. Notifies bound `wp_color_management_output_v1`
    /// objects for the toggled outputs afterward, so HDR-aware clients
    /// (browsers) re-query `description_for_output` and flip HDR detection
    /// without a page reload. `udev::toggle_hdr` returns the toggled outputs
    /// rather than us re-borrowing `udev` here -- avoids a second borrow
    /// alongside `self.color_management_state`.
    pub(crate) fn toggle_hdr(&mut self) {
        // Clone the `Rc` (not a borrow of `self`) first so the subsequent
        // `&mut self.color_management_state` below doesn't conflict with a
        // live `&self.udev_handle` borrow.
        let Some(udev) = self.udev_handle.clone() else {
            return;
        };
        let outputs = crate::udev::toggle_hdr(&udev);
        for output in &outputs {
            self.color_management_state.output_description_changed(output);
        }
    }

    fn init_wayland_listener(
        display: Display<RubixState>,
        event_loop: &mut EventLoop<RubixState>,
    ) -> OsString {
        // Creates a new listening socket, automatically choosing the next available `wayland` socket name.
        let listening_socket = ListeningSocketSource::new_auto().unwrap();

        // Get the name of the listening socket.
        // Clients will connect to this socket.
        let socket_name = listening_socket.socket_name().to_os_string();

        let loop_handle = event_loop.handle();

        loop_handle
            .insert_source(listening_socket, move |client_stream, _, state| {
                // Inside the callback, you should insert the client into the display.
                //
                // You may also associate some data with the client when inserting the client.
                state
                    .display_handle
                    .insert_client(client_stream, Arc::new(ClientState::default()))
                    .unwrap();
            })
            .expect("Failed to init the wayland event source.");

        // You also need to add the display itself to the event loop, so that client events will be processed by wayland-server.
        loop_handle
            .insert_source(
                Generic::new(display, Interest::READ, Mode::Level),
                |_, display, state| {
                    // Safety: we don't drop the display
                    unsafe {
                        display.get_mut().dispatch_clients(state).unwrap();
                    }
                    Ok(PostAction::Continue)
                },
            )
            .unwrap();

        socket_name
    }

    /// The output whose geometry contains this global point, if any. `None`
    /// when there are no outputs, or the point falls in a dead zone between
    /// heads (a gap left by non-adjacent placement in config).
    pub(crate) fn output_at(&self, point: Point<f64, Logical>) -> Option<Output> {
        self.space
            .outputs()
            .find(|o| {
                self.space
                    .output_geometry(o)
                    .map(|geo| geo.to_f64().contains(point))
                    .unwrap_or(false)
            })
            .cloned()
    }

    /// Resolve the surface (and its global position) under the pointer, honouring
    /// the layer-shell stacking order: overlay/top layer surfaces sit *above* the
    /// tiled windows, bottom/background *below* -- the same z-order the render
    /// paths build. Without the layer hit-test the space's toplevels were the only
    /// thing the pointer could ever land on, so layer clients (mako notifications,
    /// the bar) never received pointer enter/button events and couldn't be clicked.
    /// `(app_id, title)` for a window, whichever shell it came from.
    ///
    /// Wayland reads xdg-toplevel surface state; X11 maps `class` -> app_id and
    /// `title` -> title, which is the convention every taskbar expects. Empty
    /// X11 strings become `None` rather than `Some("")` so consumers can treat a
    /// missing identity uniformly. Shared by the IPC snapshot and the
    /// foreign-toplevel list so a window is named the same way everywhere.
    pub(crate) fn window_identity(&self, id: u32) -> (Option<String>, Option<String>) {
        let Some(window) = self.windows.get(&id) else {
            return (None, None);
        };
        match window.underlying_surface() {
            WindowSurface::Wayland(toplevel) => {
                smithay::wayland::compositor::with_states(toplevel.wl_surface(), |states| {
                    let attrs = states
                        .data_map
                        .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                        .map(|d| d.lock().unwrap());
                    match attrs {
                        Some(attrs) => (attrs.app_id.clone(), attrs.title.clone()),
                        None => (None, None),
                    }
                })
            }
            WindowSurface::X11(x11) => {
                let non_empty = |s: String| (!s.is_empty()).then_some(s);
                (non_empty(x11.class()), non_empty(x11.title()))
            }
        }
    }

    /// Global top-left of the mapped window backing `surface`, for turning
    /// surface-local client coordinates back into compositor space.
    pub(crate) fn window_location(&self, surface: &WlSurface) -> Option<Point<f64, Logical>> {
        let window = self
            .space
            .elements()
            .find(|window| window.wl_surface().is_some_and(|s| s.as_ref() == surface))?;
        self.space.element_location(window).map(|loc| loc.to_f64())
    }

    /// Hold a proposed pointer position inside the output layout.
    ///
    /// Any point landing on some output is accepted as-is, so the cursor crosses
    /// freely between adjacent heads; otherwise it is clamped per-axis to the
    /// output it is currently on, stopping it at that head's edge where there is
    /// no neighbour.
    pub(crate) fn clamp_to_outputs(&self, proposed: Point<f64, Logical>) -> Point<f64, Logical> {
        if self.output_at(proposed).is_some() {
            return proposed;
        }
        let current = self
            .output_at(self.pointer_location)
            .or_else(|| self.space.outputs().next().cloned());
        let Some(current) = current else { return self.pointer_location };
        let Some(geo) = self.space.output_geometry(&current) else { return self.pointer_location };

        let mut clamped = proposed;
        clamped.x = clamped.x.clamp(geo.loc.x as f64, (geo.loc.x + geo.size.w) as f64);
        clamped.y = clamped.y.clamp(geo.loc.y as f64, (geo.loc.y + geo.size.h) as f64);
        clamped
    }

    pub fn surface_under(&self, pos: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
        // Prefer the output whose geometry actually contains the pointer; fall
        // back to the first known output for a point in a dead zone between
        // heads (gaps from non-adjacent placement), so behaviour never
        // regresses versus the single-output case.
        let output = self.output_at(pos).unwrap_or(self.space.outputs().next()?.clone());
        let output = &output;
        let output_loc = self.space.output_geometry(output).map(|g| g.loc).unwrap_or_default();
        let layers = layer_map_for_output(output);
        // layer_geometry / layer_under work in output-local coords; shift the global
        // pointer position into that space, and shift results back out.
        let local = pos - output_loc.to_f64();

        let hit_layer = |layer: &LayerSurface| -> Option<(WlSurface, Point<f64, Logical>)> {
            let base = layers.layer_geometry(layer)?.loc.to_f64() + output_loc.to_f64();
            layer
                .surface_under(pos - base, WindowSurfaceType::ALL)
                .map(|(s, p)| (s, p.to_f64() + base))
        };

        // Above the tiled windows.
        if let Some(layer) = layers
            .layer_under(WlrLayer::Overlay, local)
            .or_else(|| layers.layer_under(WlrLayer::Top, local))
        {
            if let Some(hit) = hit_layer(layer) {
                return Some(hit);
            }
        }

        // The tiled windows themselves.
        if let Some((window, location)) = self.space.element_under(pos) {
            if let Some(hit) = window
                .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                .map(|(s, p)| (s, (p + location).to_f64()))
            {
                return Some(hit);
            }
        }

        // Below the tiled windows.
        if let Some(layer) = layers
            .layer_under(WlrLayer::Bottom, local)
            .or_else(|| layers.layer_under(WlrLayer::Background, local))
        {
            if let Some(hit) = hit_layer(layer) {
                return Some(hit);
            }
        }

        None
    }

    /// Bind a freshly-mapped Output to a model Monitor: idempotently create/get
    /// the Monitor whose id matches the output's `[[output]]` config entry (by
    /// name), stash that id in the Output's user-data so later lookups
    /// (`output_bounds_for`, remove-on-disconnect) can go the other way, and
    /// make it the active monitor if none is active yet (first head connected
    /// wins). Unconfigured outputs (e.g. winit's nested "winit" name, or a
    /// hotplugged head with no matching entry) get an id appended above the
    /// configured range rather than colliding with a configured id.
    pub(crate) fn bind_output_monitor(&mut self, output: &Output) {
        let name = output.name();
        let id = self
            .config
            .outputs
            .iter()
            .position(|o| o.name == name)
            .map(|i| i as u32)
            .unwrap_or_else(|| {
                let base = self.config.outputs.len() as u32;
                base + self
                    .workspace
                    .monitors
                    .iter()
                    .filter(|m| m.id >= base)
                    .count() as u32
            });
        self.workspace.ensure_monitor(id, self.config.visible_columns);
        output.user_data().insert_if_missing(|| MonitorId(id));
        // The configured primary output claims active focus even if another head
        // bound first (connectors can enumerate in any order); otherwise the first
        // output to bind seeds it.
        let is_primary = self
            .config
            .outputs
            .iter()
            .find(|o| o.name == name)
            .map(|o| o.primary)
            .unwrap_or(false);
        if is_primary || self.workspace.active_monitor().is_none() {
            self.workspace.set_active_monitor(id);
        }
    }

    /// Hot-reload keybinds from the user config file. Only the keybind set is
    /// swapped; `visible_columns` is structural (it seeds the monitor's fixed
    /// column slots at startup), so a live change is logged and ignored until a
    /// restart. On a parse failure `Config::reload` returns `None` and the
    /// current binds stay in place (keep-last-good). The swap is live on the
    /// next keypress -- `process_input_event` reads `config.keybinds` fresh each
    /// time, so no re-registration is needed.
    pub fn reload_config(&mut self) {
        let Some(new) = Config::reload() else {
            return;
        };
        if new.visible_columns != self.config.visible_columns {
            tracing::info!(
                "config visible_columns {} -> {} needs a restart to take effect; keeping {}",
                self.config.visible_columns,
                new.visible_columns,
                self.config.visible_columns,
            );
        }
        let count = new.keybinds.len();
        self.config.keybinds = new.keybinds;
        self.config.animation_duration = new.animation_duration;
        // Gaps are per-frame layout inputs, not structural like visible_columns --
        // safe to swap live; the next apply_layout re-tiles with the new values.
        self.config.outer_gap = new.outer_gap;
        self.config.inner_gap = new.inner_gap;
        self.config.outputs = new.outputs;
        self.config.sdr_white_nits = new.sdr_white_nits;
        // Re-seed the live runtime value too (already clamped by resolve()),
        // so a plain config-file edit takes effect immediately without
        // needing a keybind nudge -- matches the gaps' live-swap behavior.
        self.sdr_white_nits = self.config.sdr_white_nits;
        self.config.focus_follows_mouse = new.focus_follows_mouse;
        // Re-seeded like sdr_white_nits: a config edit wins over a runtime
        // toggle, so saving the file is always the way back to a known state.
        self.focus_follows_mouse = self.config.focus_follows_mouse;
        tracing::info!("reloaded config: {count} keybinds active");
        // Force a repaint so an sdr_white_nits edit is visible immediately,
        // same reasoning as the keybind path in dispatch_nav below.
        self.nudge_render();
    }

    /// Mint the next synthetic window id. Monotonic, never reused within a run.
    pub fn next_window_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Reconcile the model onto the Space. Runs the pure geometry pass over the
    /// active group's tree, unmaps any owned window that fell out of the visible
    /// set (hiding scrolled-away or rotated-out groups), then for each computed
    /// rectangle pushes position (via `map_element`) and size (via an xdg
    /// configure) onto the live window. Idempotent -- call it after every model
    /// mutation; re-mapping a window at an unchanged rect is a no-op and
    /// `send_pending_configure` only emits when the pending size actually differs.
    pub(crate) fn output_bounds_for(&self, monitor_id: u32) -> Option<Rect> {
        let Some(output) = self
            .space
            .outputs()
            .find(|o| o.user_data().get::<MonitorId>().is_some_and(|m| m.0 == monitor_id))
            .cloned()
        else {
            return None;
        };
        let Some(output_geo) = self.space.output_geometry(&output) else {
            return None;
        };
        // Reserve space for exclusive layer surfaces (e.g. waybar). The layer map
        // computes this during `arrange()` (run on every layer commit); its
        // non-exclusive zone is the output-local rect left over after subtracting
        // each anchored bar's exclusive_zone. Tiling into it keeps windows from
        // overlapping the bar. `zone.loc` carries the top/left inset, so offset
        // it by the output's global position.
        let zone = smithay::desktop::layer_map_for_output(&output).non_exclusive_zone();
        let bounds = Rect {
            x: (output_geo.loc.x + zone.loc.x).max(0) as u32,
            y: (output_geo.loc.y + zone.loc.y).max(0) as u32,
            width: zone.size.w.max(0) as u32,
            height: zone.size.h.max(0) as u32,
        };
        Some(bounds)
    }
    
    pub fn window_rect(&self, id: u32) -> Option<Rect> {
        let monitor = self.workspace.active_monitor()?;
        let bounds = self.output_bounds_for(monitor.id)?;
        monitor
            .compute_layout(bounds, self.config.outer_gap, self.config.inner_gap)
            .into_iter()
            .find(|(wid, _)| *wid == id)
            .map(|(_, rect)| rect)
    }

    fn ease(t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        1.0 - (1.0 - t).powi(3)
    }

    // Position-only interpolation, signed -- no clamp, no size (tweens carry
    // only a top-left position; size is configured once, up front).
    fn lerp_scale(track: ScaleTrack, t: f32) -> f32 {
        track.from + (track.to - track.from) * t
    }

    fn lerp_pos(from: Pos, to: Pos, t: f32) -> Pos {
        let lerp = |a: i32, b: i32| (a as f32 + (b - a) as f32 * t).round() as i32;
        Pos { x: lerp(from.x, to.x), y: lerp(from.y, to.y) }
    }

    /// Plan the tween set for a transition. PURE -- caller supplies current
    /// on-screen positions (read from Space) and the target layout. `bounds`
    /// gives the off-screen slide distance (height for Scroll, width for
    /// Rotate). Endpoints are signed `Pos` -- an off-screen coordinate above
    /// or left of the origin is a plain negative number, not clamped.
    fn plan_transition(
        current: &HashMap<u32, Pos>,
        targets: &[(u32, Rect)],
        transition: Transition,
        bounds: Rect,
        now: Instant,
    ) -> HashMap<u32, Tween> {
        let targets_map: HashMap<u32, Pos> = targets
            .iter()
            .map(|(id, r)| (*id, Pos { x: r.x as i32, y: r.y as i32 }))
            .collect();
        let mut tweens: HashMap<u32, Tween> = HashMap::new();

        // Enter: in targets only
        for &(id, rect) in targets {
            if !current.contains_key(&id) {
                let target = Pos { x: rect.x as i32, y: rect.y as i32 };
                let from = Self::enter_from(transition, target, bounds);
                let scale = matches!(transition, Transition::Reveal)
                    .then_some(ScaleTrack { from: 0.0, to: 1.0 });
                tweens.insert(id, Tween { kind: TweenKind::Enter, from, to: target, start: now, ghost: None, scale });
            }
        }

        // Leave: in current only
        for (&id, &cur) in current {
            if !targets_map.contains_key(&id) {
                let to = Self::leave_to(transition, cur, bounds);
                let scale = matches!(transition, Transition::Reveal)
                    .then_some(ScaleTrack { from: 1.0, to: 0.0 });
                tweens.insert(id, Tween { kind: TweenKind::Leave, from: cur, to, start: now, ghost: None, scale });
            }
        }

        // Move: in both. Rotate wraps a window whose straight-across delta is
        // longer than the shorter cross-edge path; Scroll never wraps.
        for (&id, &cur) in current {
            if let Some(&target) = targets_map.get(&id) {
                let tween = match transition {
                    Transition::Rotate => Self::plan_rotate_move(cur, target, bounds, now),
                    // Reveal only swaps two groups; every other visible column
                    // holds still, so its windows get a degenerate Move rather
                    // than a scale.
                    Transition::Scroll { .. } | Transition::Reveal => {
                        Tween { kind: TweenKind::Move, from: cur, to: target, start: now, ghost: None, scale: None }
                    }
                };
                tweens.insert(id, tween);
            }
        }

        tweens
    }

    /// Plan a single Rotate Move, detecting a band-wrap. `orig_from`/`orig_to`
    /// are the window's current and target `Pos`. STRICT `>` on the threshold:
    /// an exact `width/2` delta (e.g. a 2-column full swap) is NOT a wrap, and
    /// both windows cross through the middle -- intentional for now (see spec
    /// known-limitations; a possible future follow-up).
    fn plan_rotate_move(orig_from: Pos, orig_to: Pos, bounds: Rect, now: Instant) -> Tween {
        let long_delta = orig_to.x - orig_from.x;
        if long_delta.abs() > (bounds.width as i32) / 2 {
            let wrap_delta = if long_delta > 0 {
                long_delta - bounds.width as i32
            } else {
                long_delta + bounds.width as i32
            };
            // Space-mapped copy enters from the far edge and lands at the real
            // destination -- LOAD-BEARING: this copy must end at `orig_to`
            // because the next transition reads `current` from
            // `space.element_location`, and focus/input hit-testing use the
            // Space location.
            let from = Pos { x: orig_to.x - wrap_delta, y: orig_to.y };
            let to = orig_to;
            // Ghost copy exits off the near edge, starting where the window is now.
            let ghost = Some(GhostTrack {
                from: orig_from,
                to: Pos { x: orig_from.x + wrap_delta, y: orig_from.y },
            });
            Tween { kind: TweenKind::Move, from, to, start: now, ghost, scale: None }
        } else {
            Tween { kind: TweenKind::Move, from: orig_from, to: orig_to, start: now, ghost: None, scale: None }
        }
    }

    /// Off-screen starting position for an Enter tween, by transition kind.
    fn enter_from(transition: Transition, target: Pos, bounds: Rect) -> Pos {
        match transition {
            // down == true (content moves up): enter from BELOW.
            // down == false (content moves down): enter from ABOVE.
            Transition::Scroll { down } => {
                let dy = bounds.height as i32;
                Pos { x: target.x, y: if down { target.y + dy } else { target.y - dy } }
            }
            // Nearest-edge: a target on the left half came from off-screen
            // LEFT; right half came from off-screen RIGHT.
            Transition::Rotate => {
                let dx = bounds.width as i32;
                let midpoint = bounds.x as i32 + dx / 2;
                if target.x < midpoint {
                    Pos { x: target.x - dx, y: target.y }
                } else {
                    Pos { x: target.x + dx, y: target.y }
                }
            }
            // Grows in place: start and end are the same point.
            Transition::Reveal => target,
        }
    }

    /// Off-screen ending position for a Leave tween, by transition kind.
    fn leave_to(transition: Transition, cur: Pos, bounds: Rect) -> Pos {
        match transition {
            // down == true (content moves up): leave to TOP.
            // down == false (content moves down): leave to BOTTOM.
            Transition::Scroll { down } => {
                let dy = bounds.height as i32;
                Pos { x: cur.x, y: if down { cur.y - dy } else { cur.y + dy } }
            }
            // Nearest-edge: a window currently on the left half exits LEFT;
            // right half exits RIGHT.
            Transition::Rotate => {
                let dx = bounds.width as i32;
                let midpoint = bounds.x as i32 + dx / 2;
                if cur.x < midpoint {
                    Pos { x: cur.x - dx, y: cur.y }
                } else {
                    Pos { x: cur.x + dx, y: cur.y }
                }
            }
            // Shrinks in place: start and end are the same point.
            Transition::Reveal => cur,
        }
    }

    /// Settle in-flight tweens so Space is a clean baseline. Leave any windows
    /// still owned in `self.windows` mapped at their final rect; unmapping only
    /// for Leave tweens (windows that fell out of the visible set).
    fn settle_tweens(&mut self) {
        let done: Vec<u32> = self.animations.keys().copied().collect();
        for id in done {
            if let Some(tween) = self.animations.remove(&id) {
                if let Some(window) = self.windows.get(&id) {
                    match tween.kind {
                        TweenKind::Leave => {
                            self.space.unmap_elem(window);
                        }
                        _ => {
                            self.space.map_element(window.clone(), (tween.to.x, tween.to.y), false);
                        }
                    }
                }
            }
        }
    }

    /// Advance every active tween one frame. Returns true while any tween is live.
    /// Touches nothing when `self.animations` is empty (otherwise the udev backend
    /// never idles).
    pub fn step_animations(&mut self) -> bool {
        // Cleared FIRST, before the empty-animations guard below: otherwise the
        // last wrap's ghost would leak forever, since the guard returns early
        // on every subsequent idle frame and the list never gets rebuilt.
        self.active_ghosts.clear();
        self.active_scales.clear();
        if self.animations.is_empty() { return false; }
        let duration_secs = self.config.animation_duration.as_secs_f32();
        let now = Instant::now();
        let mut done: Vec<u32> = Vec::new();
        for (id, tween) in self.animations.iter() {
            let t = (now - tween.start).as_secs_f32() / duration_secs;
            let e = Self::ease(t);
            let pos = Self::lerp_pos(tween.from, tween.to, e);
            match tween.scale {
                // Scaling window: kept OUT of the Space for the duration, or
                // render_elements_for_region would draw a second copy at full
                // size underneath the scaled one. Re-mapped on completion below
                // (Enter) or left unmapped for good (Leave).
                Some(track) => {
                    if let Some(window) = self.windows.get(id) {
                        self.space.unmap_elem(window);
                    }
                    self.active_scales.push((*id, pos, Self::lerp_scale(track, e)));
                }
                None => {
                    if let Some(window) = self.windows.get(id) {
                        self.space.map_element(window.clone(), (pos.x, pos.y), false);
                    }
                }
            }
            if let Some(g) = tween.ghost {
                let gpos = Self::lerp_pos(g.from, g.to, e);
                self.active_ghosts.push((*id, gpos));
            }
            if t >= 1.0 { done.push(*id); }
        }
        for id in done {
            if let Some(tween) = self.animations.remove(&id) {
                if let Some(window) = self.windows.get(&id) {
                    match tween.kind {
                        TweenKind::Leave => { self.space.unmap_elem(window); }
                        _ => { self.space.map_element(window.clone(), (tween.to.x, tween.to.y), false); }
                    }
                }
            }
        }
        !self.animations.is_empty()
    }

    /// Insert `id` into the active monitor's grid by splitting the focused
    /// window -- the same rule new windows follow in `xdg_shell::new_toplevel`
    /// and `xwayland::map_window_request`.
    fn insert_into_grid(&mut self, id: u32) {
        let focused_id = self.focused_window_id();
        let direction = focused_id
            .and_then(|fid| self.window_rect(fid))
            .map(Rect::longer_axis)
            .unwrap_or(SplitDirection::Horizontal);
        if let Some(monitor) = self.workspace.active_monitor_mut() {
            monitor.add_window(direction, id, focused_id.unwrap_or(0));
        }
    }

    /// Enter or leave exclusive fullscreen for `id`.
    ///
    /// A fullscreen window LEAVES the tiling grid rather than keeping a slot
    /// that `compute_layout` keeps filling. Holding a slot meant the window only
    /// stopped being drawn once its entire column scrolled off screen, which
    /// never happens when every column already fits -- navigating away from a
    /// game did nothing. Out of the grid the remaining windows reflow into the
    /// whole layout, and `apply_layout` becomes the only thing deciding whether
    /// the fullscreen window is on screen.
    ///
    /// Re-insertion splits the focused window, so a window does not necessarily
    /// return to the slot it left; restoring exactly needs an insert-at-slot API
    /// the model doesn't have yet.
    /// Ask a window to close, the polite way for each shell.
    ///
    /// This is a request, not a teardown: the client decides (it may put up a
    /// "save changes?" dialog and never close). Rubix's own bookkeeping is
    /// driven by the resulting unmap, not from here.
    pub(crate) fn close_window(&mut self, id: u32) {
        let Some(window) = self.windows.get(&id) else { return };
        match window.underlying_surface() {
            WindowSurface::Wayland(toplevel) => toplevel.send_close(),
            WindowSurface::X11(x11) => {
                let _ = x11.close();
            }
        }
    }

    pub fn set_window_fullscreen(&mut self, id: u32, fullscreen: bool) {
        if fullscreen {
            if !self.fullscreen_windows.insert(id) {
                return;
            }
            // A destroyed window may have been on any monitor, and
            // `remove_window` is id-based and a no-op when absent.
            for monitor in &mut self.workspace.monitors {
                let _ = monitor.remove_window(id);
            }
        } else {
            if !self.fullscreen_windows.remove(&id) {
                return;
            }
            self.insert_into_grid(id);
        }
    }

    /// Toggle maximize for the focused window, releasing any previous one.
    ///
    /// Maximize is deliberately cheap next to fullscreen: no `_NET_WM_STATE`, no
    /// client negotiation, no connector or scanout involvement. The window keeps
    /// its grid slot and simply gets the monitor's work area as its rect.
    pub fn toggle_maximize(&mut self) {
        let Some(id) = self.focused_window_id() else {
            return;
        };
        // Fullscreen already owns the whole output; maximizing under it would be
        // a no-op the user can't see, and un-maximizing later would look random.
        if self.fullscreen_windows.contains(&id) {
            return;
        }

        // MODEL SEAM: before maximizing, a window that is not already first in
        // its group's tree should be promoted there -- restructured so it is the
        // root's first child and takes half the space, as if it were the first
        // window spawned and split once. That is tree surgery in `model::grid`,
        // so it wants a `Monitor::promote_to_first(id) -> bool` (true when it
        // moved, false when it was already first). Wire it here as:
        //
        //     if self.workspace.active_monitor_mut()
        //         .is_some_and(|m| m.promote_to_first(id)) { .. return early .. }
        //
        // Until that exists, every press maximizes directly.
        self.maximized = if self.maximized == Some(id) { None } else { Some(id) };
        self.apply_layout();
        self.ipc_dirty = true;
    }

    /// Focus a fullscreen window, cycling if there is more than one.
    ///
    /// Fullscreen windows are outside the grid, so `focus_active_window` -- which
    /// walks active_column -> active_row -> first leaf -- can never land on one.
    /// Without this there is no way back to a game once you navigate off it. The
    /// real answer is a focus-by-id path the launcher can drive; this is the
    /// keyboard escape hatch until that exists.
    pub fn focus_next_fullscreen(&mut self) {
        let mut ids: Vec<u32> = self
            .fullscreen_windows
            .iter()
            .copied()
            .filter(|id| self.windows.contains_key(id))
            .collect();
        if ids.is_empty() {
            return;
        }
        // Sorted so "next" is stable across calls -- HashSet iteration order is
        // not, and cycling that would revisit windows at random.
        ids.sort_unstable();

        let current = self.focused_window_id();
        let next = current
            .and_then(|cur| ids.iter().position(|id| *id == cur))
            .map(|pos| ids[(pos + 1) % ids.len()])
            .unwrap_or(ids[0]);
        self.focus_by_id(next);
    }

    /// Give keyboard focus to the window now under the pointer, if
    /// focus-follows-mouse is on and nothing vetoes it.
    ///
    /// Driven from pointer *motion* only, never from a window arriving under a
    /// stationary cursor -- matching sway, and avoiding focus lurching around
    /// on its own during a rotate animation.
    pub(crate) fn focus_follows_pointer(&mut self, pos: Point<f64, Logical>) {
        if !self.focus_follows_mouse {
            return;
        }
        // A grab owns the pointer for the length of a drag or resize; moving
        // focus mid-drag pulls the grab out from under itself.
        if self.seat.get_pointer().is_some_and(|p| p.is_grabbed()) {
            return;
        }
        // Suspended entirely while anything is fullscreen. `reconcile_focus_state`
        // drops a non-focused window's fullscreen, so a stray hover would kick a
        // game out of fullscreen -- and a fullscreen window is outside the grid,
        // making that hard to undo.
        if !self.fullscreen_windows.is_empty() {
            return;
        }
        // `element_under` only sees space elements, i.e. real toplevels, so
        // layer-shell surfaces are structurally excluded: hovering the bar or
        // rofi must never take the keyboard away from them.
        let Some(id) = self.window_id_at(pos) else { return };
        if self.focused_window_id() == Some(id) {
            return;
        }
        // Focus without raising: hover is not a gesture at the window, so it
        // must not reorder the stack. See focus_by_id_without_raising.
        self.focus_by_id_without_raising(id);
        self.ipc_dirty = true;
    }

    /// The window under a point, as a Rubix id. Resolved through the Space so
    /// subsurfaces and popups map to their owning toplevel.
    pub(crate) fn window_id_at(&self, pos: Point<f64, Logical>) -> Option<u32> {
        let (window, _) = self.space.element_under(pos)?;
        self.windows
            .iter()
            .find(|(_, candidate)| *candidate == window)
            .map(|(id, _)| *id)
    }

    /// Deactivate the pointer constraint on every window except `keep`.
    ///
    /// The protocol releases a constraint when its surface loses *pointer* focus,
    /// but a locked pointer cannot move, so pointer focus never changes on its
    /// own: navigate away from a game that holds a lock and the cursor stays
    /// frozen on it forever. Keyboard focus moving off the window is the signal
    /// we actually get, so the release is driven from there.
    pub(crate) fn release_pointer_constraints(&mut self, keep: Option<u32>) {
        let Some(pointer) = self.seat.get_pointer() else { return };
        let surfaces: Vec<WlSurface> = self
            .windows
            .iter()
            .filter(|(id, _)| Some(**id) != keep)
            .filter_map(|(_, window)| window.wl_surface().map(|s| s.into_owned()))
            .collect();

        for surface in surfaces {
            with_pointer_constraint(&surface, &pointer, |constraint| {
                if let Some(constraint) = constraint {
                    if constraint.is_active() {
                        constraint.deactivate();
                    }
                }
            });
        }
    }

    /// Reconcile focus-dependent window state after keyboard focus moves.
    ///
    /// Maximize releases on any focus change, mirroring the `unfocus` handler in
    /// Max's AwesomeWM config. Fullscreen is split by client type: a Wayland
    /// toplevel accepts the compositor's word and is genuinely un-fullscreened
    /// back into the grid, while an X11 client keeps its fullscreen state and is
    /// merely hidden -- telling one it is windowed just invites it to set
    /// `_NET_WM_STATE_FULLSCREEN` straight back.
    pub(crate) fn reconcile_focus_state(&mut self) {
        let focused = self.focused_window_id();
        let mut dirty = false;

        self.release_pointer_constraints(focused);

        if self.maximized.is_some() && self.maximized != focused {
            self.maximized = None;
            dirty = true;
        }

        let unfullscreen: Vec<u32> = self
            .fullscreen_windows
            .iter()
            .copied()
            .filter(|id| Some(*id) != focused)
            .filter(|id| {
                self.windows.get(id).is_some_and(|w| {
                    matches!(w.underlying_surface(), WindowSurface::Wayland(_))
                })
            })
            .collect();

        for id in unfullscreen {
            if let Some(WindowSurface::Wayland(toplevel)) =
                self.windows.get(&id).map(|w| w.underlying_surface())
            {
                toplevel.with_pending_state(|state| {
                    state.states.unset(xdg_toplevel::State::Fullscreen);
                });
            }
            self.set_window_fullscreen(id, false);
            tracing::info!("window {id} left fullscreen (lost focus)");
            dirty = true;
        }

        if dirty {
            self.apply_layout();
            self.ipc_dirty = true;
        }
    }

    /// Tell X11 clients whether the grid is currently showing them.
    ///
    /// An X11 client will not accept being un-fullscreened: drop
    /// `_NET_WM_STATE_FULLSCREEN` and a game sets it straight back (Against the
    /// Storm re-asserted ~3s later, taking the output with it). `WM_STATE =
    /// IconicState` is a state it will respect, because the correct response to
    /// being minimized is to stop rather than to argue. This leaves the client's
    /// own idea of being fullscreen intact, so navigating back restores it.
    ///
    /// Driven by visibility rather than by the nav paths, since the grid hides
    /// windows several ways -- scrolling past `visible_columns`, a non-active
    /// row, a new window displacing one -- and all of them owe the client the
    /// same notification.
    fn sync_x11_iconic(&mut self, visible: &HashSet<u32>) {
        // `self.iconified` tracks what each client was last told, so a layout
        // pass that changes nothing doesn't spray redundant property writes.
        let mut changed: Vec<(u32, bool)> = Vec::new();
        for (id, window) in &self.windows {
            if let WindowSurface::X11(x11) = window.underlying_surface() {
                let hidden = !visible.contains(id);
                if self.iconified.contains(id) != hidden {
                    let _ = x11.set_hidden(hidden);
                    changed.push((*id, hidden));
                }
            }
        }

        for (id, hidden) in changed {
            if hidden {
                tracing::info!("window {id} iconified (hidden by layout)");
                self.iconified.insert(id);
            } else {
                self.iconified.remove(&id);
            }
        }
    }

    pub fn apply_layout(&mut self) {
        let mut targets: Vec<(u32, Rect)> = Vec::new();
        for monitor in &self.workspace.monitors {
            if let Some(bounds) = self.output_bounds_for(monitor.id) {
                targets.extend(monitor.compute_layout(bounds, self.config.outer_gap, self.config.inner_gap));
            }
        }

        // Maximize: keep the grid slot, take the monitor work area. Only applies
        // while the window is actually on screen -- a maximized window scrolled
        // out of view has no entry to override and stays out of view.
        if let Some(id) = self.maximized {
            if let Some(bounds) = self
                .workspace
                .active_monitor()
                .and_then(|m| self.output_bounds_for(m.id))
            {
                let gap = self.config.outer_gap;
                let rect = Rect {
                    x: bounds.x + gap,
                    y: bounds.y + gap,
                    width: bounds.width.saturating_sub(2 * gap),
                    height: bounds.height.saturating_sub(2 * gap),
                };
                if let Some(entry) = targets.iter_mut().find(|(tid, _)| *tid == id) {
                    entry.1 = rect;
                }
            }
        }

        // Fullscreen windows are OUT of the grid (`set_window_fullscreen`), so
        // `compute_layout` never emits them and their rect comes from here --
        // but only for the focused one. That is what makes navigating away
        // actually leave a fullscreen game, rather than depending on its column
        // happening to scroll off screen (which never happens when every column
        // already fits). Anything left out is reported iconic below.
        let focused = self.focused_window_id();
        let fullscreen_ids: Vec<u32> = self.fullscreen_windows.iter().cloned().collect();
        for id in fullscreen_ids {
            if Some(id) != focused {
                continue;
            }
            if let Some(window) = self.windows.get(&id) {
                // Find which monitor this window is on by checking its current location
                let output = self
                    .space
                    .element_location(window)
                    .and_then(|loc| self.output_at(loc.to_f64()))
                    .or_else(|| self.space.outputs().next().cloned());

                if let Some(output) = output {
                    if let Some(bounds) = self.space.output_geometry(&output) {
                        let rect = Rect {
                            x: bounds.loc.x as u32,
                            y: bounds.loc.y as u32,
                            width: bounds.size.w as u32,
                            height: bounds.size.h as u32,
                        };
                        // Normally there is no entry to override, since the window
                        // left the grid on going fullscreen. The override still
                        // covers the transient case where a client requests
                        // fullscreen in the same pass its tiled entry was built.
                        if let Some(entry) = targets.iter_mut().find(|(tid, _)| *tid == id) {
                            entry.1 = rect;
                        } else {
                            targets.push((id, rect));
                        }
                    }
                }
            }
        }

        // Whatever `targets` holds now is exactly what the grid intends to show,
        // on both the snap and the animate path, so this is the one place that
        // knows which X11 clients just became (in)visible.
        let visible: HashSet<u32> = targets.iter().map(|(id, _)| *id).collect();
        self.sync_x11_iconic(&visible);

        match self.pending_transition.take() {
            None => {
                // SNAP PATH — byte-for-byte today's behavior.
                // Settle in-flight animations first (safety): for each tween,
                // if Leave -> unmap; else map_element at tween.to. Clear the map.
                self.settle_tweens();

                let stale: Vec<Window> = self
                    .windows
                    .iter()
                    .filter(|(id, _)| !visible.contains(id))
                    .map(|(_, window)| window.clone())
                    .collect();
                for window in stale {
                    self.space.unmap_elem(&window);
                }

                for (id, rect) in targets {
                    let Some(window) = self.windows.get(&id).cloned() else {
                        continue;
                    };

                    let is_fullscreen = self.fullscreen_windows.contains(&id);

                    // Check if geometry or fullscreen state has actually changed since last configure.
                    // This gates redundant configure resends during reflows of unrelated windows,
                    // preventing spurious swapchain recreations that cause flicker during exclusive
                    // fullscreen scanout.
                    let state_changed = self.last_configured.get(&id)
                        .map(|(last_rect, last_fullscreen)| {
                            last_rect != &rect || *last_fullscreen != is_fullscreen
                        })
                        .unwrap_or(true);

                    if state_changed {
                        match window.underlying_surface() {
                            WindowSurface::Wayland(toplevel) => {
                                toplevel.with_pending_state(|state| {
                                    state.size = Some((rect.width as i32, rect.height as i32).into());
                                    if is_fullscreen {
                                        state.states.set(xdg_toplevel::State::Fullscreen);
                                        state.states.unset(xdg_toplevel::State::TiledLeft);
                                        state.states.unset(xdg_toplevel::State::TiledRight);
                                        state.states.unset(xdg_toplevel::State::TiledTop);
                                        state.states.unset(xdg_toplevel::State::TiledBottom);
                                    } else {
                                        state.states.set(xdg_toplevel::State::TiledLeft);
                                        state.states.set(xdg_toplevel::State::TiledRight);
                                        state.states.set(xdg_toplevel::State::TiledTop);
                                        state.states.set(xdg_toplevel::State::TiledBottom);
                                        state.states.unset(xdg_toplevel::State::Fullscreen);
                                    }
                                });
                                toplevel.send_pending_configure();
                                self.last_configured.insert(id, (rect.clone(), is_fullscreen));
                            }
                            WindowSurface::X11(x11) => {
                                // X11 configure carries position AND size in one rect.
                                // map_element below still sets the compositor-side
                                // location; keep both.
                                let _ = x11.configure(Some(Rectangle::new(
                                    (rect.x as i32, rect.y as i32).into(),
                                    (rect.width as i32, rect.height as i32).into(),
                                )));
                                self.last_configured.insert(id, (rect.clone(), is_fullscreen));
                            }
                        }
                    }

                    self.space.map_element(window, (rect.x as i32, rect.y as i32), false);
                }
            }
            Some(transition) => {
                // ANIMATE PATH
                // Nav dispatch (the only source of a pending_transition) always
                // mutates the ACTIVE monitor, so the active monitor's bounds are
                // the correct off-screen reference frame for this tween -- not an
                // approximation. If the active monitor has no bound output (or
                // there's no active monitor), settle in-flight tweens and bail,
                // matching the prior no-output early-return.
                let Some(bounds) = self
                    .workspace
                    .active_monitor()
                    .and_then(|m| self.output_bounds_for(m.id))
                else {
                    self.settle_tweens();
                    return;
                };

                // 1. Settle in-flight tweens first so Space is a clean baseline.
                self.settle_tweens();

                // 2. Build current positions from windows still mapped in Space.
                let mut current: HashMap<u32, Pos> = HashMap::new();
                for (id, window) in &self.windows {
                    if let Some(loc) = self.space.element_location(window) {
                        current.insert(*id, Pos { x: loc.x, y: loc.y });
                    }
                }

                // 3. For every target id: send the size configure.
                for (id, rect) in &targets {
                    let Some(window) = self.windows.get(id).cloned() else {
                        continue;
                    };

                    let is_fullscreen = self.fullscreen_windows.contains(id);

                    match window.underlying_surface() {
                        WindowSurface::Wayland(toplevel) => {
                            toplevel.with_pending_state(|state| {
                                state.size = Some((rect.width as i32, rect.height as i32).into());
                                if is_fullscreen {
                                    state.states.set(xdg_toplevel::State::Fullscreen);
                                    state.states.unset(xdg_toplevel::State::TiledLeft);
                                    state.states.unset(xdg_toplevel::State::TiledRight);
                                    state.states.unset(xdg_toplevel::State::TiledTop);
                                    state.states.unset(xdg_toplevel::State::TiledBottom);
                                } else {
                                    state.states.set(xdg_toplevel::State::TiledLeft);
                                    state.states.set(xdg_toplevel::State::TiledRight);
                                    state.states.set(xdg_toplevel::State::TiledTop);
                                    state.states.set(xdg_toplevel::State::TiledBottom);
                                    state.states.unset(xdg_toplevel::State::Fullscreen);
                                }
                            });
                            toplevel.send_pending_configure();
                        }
                        WindowSurface::X11(x11) => {
                            // Same rect-carries-position-and-size as the snap
                            // path; map_element for the tween is driven by the
                            // plan below (unchanged) -- X11 windows animate
                            // identically since space placement is
                            // surface-agnostic.
                            let _ = x11.configure(Some(Rectangle::new(
                                (rect.x as i32, rect.y as i32).into(),
                                (rect.width as i32, rect.height as i32).into(),
                            )));
                        }
                    }
                }

                // 4-6. Plan tweens, map enter/move at from-position, store.
                let plan = Self::plan_transition(&current, &targets, transition, bounds, Instant::now());
                for (id, tween) in &plan {
                    if let Some(window) = self.windows.get(id).cloned() {
                        // Scaling tweens are drawn outside the Space for their
                        // whole run, so they must not be mapped here either --
                        // otherwise the window paints once at full size in the
                        // frame between planning and the first step_animations.
                        if tween.scale.is_some() {
                            self.space.unmap_elem(&window);
                            continue;
                        }
                        match tween.kind {
                            TweenKind::Enter | TweenKind::Move => {
                                self.space.map_element(window, (tween.from.x, tween.from.y), false);
                            }
                            TweenKind::Leave => {
                                // Leave it mapped where it is; step_animations unmaps at completion.
                            }
                        }
                    }
                }
                self.animations = plan;
            }
        }

        // A maximized window overlaps its neighbours' tiles, so stack order is
        // what decides whether it reads as maximized or as half-buried. Raising
        // it within `Space` puts it above every other toplevel and no higher:
        // layer-shell Top/Overlay (the bar, notifications) are composited from
        // separate lists that always sit in front of space elements, so this
        // cannot paint over them. Raised before fullscreen so that if both
        // somehow apply at once, fullscreen still wins the top slot.
        if let Some(window) = self.maximized.and_then(|id| self.windows.get(&id).cloned()) {
            self.space.raise_element(&window, false);
        }

        // A fullscreen window must be topmost in the Space stack or a tiled
        // window rendered above it kills primary-plane promotion. Raise after
        // both the SNAP and ANIMATE paths have finished mapping.
        for id in self.fullscreen_windows.iter().copied().collect::<Vec<_>>() {
            if let Some(window) = self.windows.get(&id).cloned() {
                self.space.raise_element(&window, false);
            }
        }
    }
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

/// Surface elements for every window mid-Reveal, each wrapped in a
/// `RescaleRenderElement` about the window's own centre so it grows or shrinks
/// in place rather than toward a corner.
///
/// These windows are unmapped from the `Space` for the whole tween (see
/// `step_animations`), because `render_elements_for_region` draws the space in
/// one call and cannot scale individual elements. So this is their ONLY draw --
/// omitting this list from a render path makes revealing windows invisible for
/// the length of the animation rather than merely unscaled. Mirrors the ghost
/// path, including its 1.0 output scale assumption.
use smithay::backend::renderer::{
    element::{surface::WaylandSurfaceRenderElement, utils::RescaleRenderElement, AsRenderElements},
    ImportAll, Renderer,
};
use smithay::utils::{Physical, Scale};

pub(crate) fn reveal_scale_elements<R>(
    state: &RubixState,
    renderer: &mut R,
) -> Vec<RescaleRenderElement<WaylandSurfaceRenderElement<R>>>
where
    R: Renderer + ImportAll,
    R::TextureId: Clone + 'static,
{
    let scaled: Vec<(Window, Pos, f32)> = state
        .active_scales
        .iter()
        .filter_map(|(id, pos, factor)| state.windows.get(id).map(|w| (w.clone(), *pos, *factor)))
        .collect();

    let mut out = Vec::new();
    for (window, pos, factor) in scaled {
        // Fully collapsed: nothing worth drawing, and a zero scale degenerates
        // the element geometry.
        if factor <= 0.01 {
            continue;
        }
        let size = window.geometry().size;
        let origin = Point::<i32, Physical>::from((pos.x + size.w / 2, pos.y + size.h / 2));
        for element in window.render_elements::<WaylandSurfaceRenderElement<R>>(
            renderer,
            Point::<i32, Physical>::from((pos.x, pos.y)),
            Scale::from(1.0),
            1.0,
        ) {
            out.push(RescaleRenderElement::from_element(element, origin, factor as f64));
        }
    }
    out
}
