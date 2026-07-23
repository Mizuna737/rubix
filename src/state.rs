use std::{collections::HashMap, ffi::OsString, sync::Arc};

use smithay::{
    desktop::{PopupManager, Space, Window, WindowSurfaceType},
    input::{Seat, SeatState},
    reexports::{
        calloop::{generic::Generic, EventLoop, Interest, LoopSignal, Mode, PostAction},
        wayland_server::{
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
            Display, DisplayHandle,
        },
    },
    utils::{Logical, Point},
    wayland::{
        compositor::{CompositorClientState, CompositorState},
        output::OutputManagerState,
        selection::data_device::DataDeviceState,
        shell::xdg::XdgShellState,
        shm::ShmState,
        socket::ListeningSocketSource,
    },
};

use crate::{
    config::Config,
    model::{
        geometry::Rect,
        grid::{Column, Group, Monitor},
    },
    CalloopData,
};

pub struct RubixState {
    pub start_time: std::time::Instant,
    pub socket_name: OsString,
    pub display_handle: DisplayHandle,

    pub space: Space<Window>,
    pub loop_signal: LoopSignal,

    // User configuration (keybinds + layout), resolved at startup.
    pub config: Config,

    // Rubix model + translation registry.
    // `monitor` is the pure tiling model (fixed column slots); `windows` maps its
    // synthetic u32 ids to live Smithay handles; `next_id` mints those ids (starts
    // at 1 -- 0 is TilingNode::remove_window's transient placeholder, never real).
    pub monitor: Monitor,
    pub windows: HashMap<u32, Window>,
    pub next_id: u32,

    // Smithay State
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<RubixState>,
    pub data_device_state: DataDeviceState,
    pub popups: PopupManager,

    pub seat: Seat<Self>,
}

impl RubixState {
    pub fn new(event_loop: &mut EventLoop<CalloopData>, display: Display<Self>, config: Config) -> Self {
        let start_time = std::time::Instant::now();

        let dh = display.handle();

        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let mut seat_state = SeatState::new();
        let data_device_state = DataDeviceState::new::<Self>(&dh);
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

        // Seed the monitor with exactly visible_columns column slots so the
        // active-column cursor (rem_euclid(visible_columns)) always indexes a real
        // column. Columns are fixed slots -- never removed below this floor. Each
        // slot carries one empty-layout placeholder group so `groups[active_row]`
        // is always valid (rotate_columns swaps it; the layout walk renders a
        // `layout: None` group as a blank band). The 0 width is a placeholder; the
        // walk derives band width from output geometry.
        let mut monitor = Monitor::new(0, config.visible_columns);
        for _ in 0..config.visible_columns {
            let mut column = Column::new(0);
            column.add_group(Group { layout: None });
            monitor.add_column(column);
        }

        Self {
            start_time,
            display_handle: dh,

            space,
            loop_signal,
            socket_name,

            config,

            monitor,
            windows: HashMap::new(),
            next_id: 1,

            compositor_state,
            xdg_shell_state,
            shm_state,
            output_manager_state,
            seat_state,
            data_device_state,
            popups,
            seat,
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

    pub fn surface_under(&self, pos: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
        self.space.element_under(pos).and_then(|(window, location)| {
            window
                .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                .map(|(s, p)| (s, (p + location).to_f64()))
        })
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
        tracing::info!("reloaded config: {count} keybinds active");
    }

    /// Mint the next synthetic window id. Monotonic, never reused within a run.
    pub fn next_window_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Reconcile the model onto the Space. Runs the pure geometry pass over the
    /// active group's tree, then for each computed rectangle pushes position
    /// (via `map_element`) and size (via an xdg configure) onto the live window.
    /// Idempotent -- call it after every model mutation; re-mapping a window at
    /// an unchanged rect is a no-op and `send_pending_configure` only emits when
    /// the pending size actually differs.
    pub fn apply_layout(&mut self) {
        let Some(output) = self.space.outputs().next().cloned() else {
            return;
        };
        let Some(output_geo) = self.space.output_geometry(&output) else {
            return;
        };
        let bounds = Rect {
            x: output_geo.loc.x.max(0) as u32,
            y: output_geo.loc.y.max(0) as u32,
            width: output_geo.size.w.max(0) as u32,
            height: output_geo.size.h.max(0) as u32,
        };

        // Borrow the model only long enough to compute; `placements` is owned, so
        // the immutable borrow of `self.monitor` is released before we touch
        // `self.space` / `self.windows` mutably below.
        let placements = self.monitor.compute_layout(bounds);

        for (id, rect) in placements {
            let Some(window) = self.windows.get(&id).cloned() else {
                continue;
            };

            // Force the tiled size via a configure (same handshake as resize_grab).
            let toplevel = window.toplevel().unwrap();
            toplevel.with_pending_state(|state| {
                state.size = Some((rect.width as i32, rect.height as i32).into());
            });
            toplevel.send_pending_configure();

            self.space.map_element(window, (rect.x as i32, rect.y as i32), false);
        }
    }
}

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}
