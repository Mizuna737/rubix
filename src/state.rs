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
        calloop::{generic::Generic, EventLoop, Interest, LoopSignal, Mode, PostAction},
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
            Display, DisplayHandle,
        },
    },
    utils::{Logical, Point, Rectangle},
    wayland::{
        compositor::{CompositorClientState, CompositorState},
        dmabuf::{DmabufGlobal, DmabufState},
        output::OutputManagerState,
        selection::{data_device::DataDeviceState, wlr_data_control::DataControlState},
        shell::wlr_layer::{Layer as WlrLayer, WlrLayerShellState},
        shell::xdg::XdgShellState,
        shm::ShmState,
        socket::ListeningSocketSource,
        xwayland_shell::XWaylandShellState,
    },
    xwayland::X11Wm,
};

use crate::{
    config::Config,
    model::{
        geometry::Rect,
        grid::Workspace,
    },
    CalloopData,
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

#[derive(Clone, Copy, Debug)]
pub(crate) struct Tween {
    kind: TweenKind,
    from: Pos,
    to: Pos,
    start: Instant,
    ghost: Option<GhostTrack>,
}

// The kind of spatial-nav transition that just happened. Carries the slide
// axis/sign, which cannot be recovered from a set diff.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Transition {
    Scroll { down: bool },
    Rotate,
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

    // Windows currently in fullscreen state (bypass normal tiling).
    pub(crate) fullscreen_windows: HashSet<u32>,

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

    pub seat: Seat<Self>,

    // Single source of truth for the software cursor's logical position, kept
    // in sync by BOTH input paths (relative + absolute) in `input.rs`. The
    // cursor render element (src/cursor.rs) reads this each frame.
    pub pointer_location: Point<f64, Logical>,
    // The client-requested cursor image (named/surface/hidden), set by the
    // `SeatHandler::cursor_image` callback in handlers/mod.rs.
    pub cursor_status: CursorImageStatus,

    // wlr-screencopy captures awaiting the next presented frame. Pushed by the
    // frame `copy` handler (screencopy.rs), drained by each backend's render
    // path via `screencopy::fulfill_pending` right after it presents.
    pub(crate) pending_screencopy: Vec<crate::screencopy::PendingScreencopy>,
}

impl RubixState {
    pub fn new(event_loop: &mut EventLoop<CalloopData>, display: Display<Self>, config: Config) -> Self {
        let start_time = std::time::Instant::now();

        let dh = display.handle();

        let compositor_state = CompositorState::new::<Self>(&dh);
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

        Self {
            start_time,
            display_handle: dh,

            space,
            loop_signal,
            pending_vt: None,
            socket_name,

            config,

            workspace,
            windows: HashMap::new(),
            next_id: 1,
            unmapped: HashMap::new(),
            ipc_dirty: false,
            reserved_bounds: None,
            animations: HashMap::new(),
            pending_transition: None,
            active_ghosts: Vec::new(),
            fullscreen_windows: HashSet::new(),

            compositor_state,
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
            seat,

            pointer_location,
            cursor_status: CursorImageStatus::default_named(),
            pending_screencopy: Vec::new(),
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

    fn init_wayland_listener(
        display: Display<RubixState>,
        event_loop: &mut EventLoop<CalloopData>,
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
                        display.get_mut().dispatch_clients(&mut state.state).unwrap();
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
        if self.workspace.active_monitor().is_none() {
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
        tracing::info!("reloaded config: {count} keybinds active");
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
                tweens.insert(id, Tween { kind: TweenKind::Enter, from, to: target, start: now, ghost: None });
            }
        }

        // Leave: in current only
        for (&id, &cur) in current {
            if !targets_map.contains_key(&id) {
                let to = Self::leave_to(transition, cur, bounds);
                tweens.insert(id, Tween { kind: TweenKind::Leave, from: cur, to, start: now, ghost: None });
            }
        }

        // Move: in both. Rotate wraps a window whose straight-across delta is
        // longer than the shorter cross-edge path; Scroll never wraps.
        for (&id, &cur) in current {
            if let Some(&target) = targets_map.get(&id) {
                let tween = match transition {
                    Transition::Rotate => Self::plan_rotate_move(cur, target, bounds, now),
                    Transition::Scroll { .. } => {
                        Tween { kind: TweenKind::Move, from: cur, to: target, start: now, ghost: None }
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
            Tween { kind: TweenKind::Move, from, to, start: now, ghost }
        } else {
            Tween { kind: TweenKind::Move, from: orig_from, to: orig_to, start: now, ghost: None }
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
        if self.animations.is_empty() { return false; }
        let duration_secs = self.config.animation_duration.as_secs_f32();
        let now = Instant::now();
        let mut done: Vec<u32> = Vec::new();
        for (id, tween) in self.animations.iter() {
            let t = (now - tween.start).as_secs_f32() / duration_secs;
            let e = Self::ease(t);
            let pos = Self::lerp_pos(tween.from, tween.to, e);
            if let Some(window) = self.windows.get(id) {
                self.space.map_element(window.clone(), (pos.x, pos.y), false);
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

    pub fn apply_layout(&mut self) {
        let mut targets: Vec<(u32, Rect)> = Vec::new();
        for monitor in &self.workspace.monitors {
            if let Some(bounds) = self.output_bounds_for(monitor.id) {
                targets.extend(monitor.compute_layout(bounds, self.config.outer_gap, self.config.inner_gap));
            }
        }

        // Add fullscreen windows to targets with their full output bounds.
        let fullscreen_ids: Vec<u32> = self.fullscreen_windows.iter().cloned().collect();
        for id in fullscreen_ids {
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
                        // Only add if not already in targets (shouldn't happen, but be safe)
                        if !targets.iter().any(|(tid, _)| *tid == id) {
                            targets.push((id, rect));
                        }
                    }
                }
            }
        }

        match self.pending_transition.take() {
            None => {
                // SNAP PATH — byte-for-byte today's behavior.
                // Settle in-flight animations first (safety): for each tween,
                // if Leave -> unmap; else map_element at tween.to. Clear the map.
                self.settle_tweens();

                let visible: HashSet<u32> = targets.iter().map(|(id, _)| *id).collect();

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
                            // X11 configure carries position AND size in one rect.
                            // map_element below still sets the compositor-side
                            // location; keep both.
                            let _ = x11.configure(Some(Rectangle::new(
                                (rect.x as i32, rect.y as i32).into(),
                                (rect.width as i32, rect.height as i32).into(),
                            )));
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
