use smithay::{
    delegate_xdg_shell,
    desktop::{find_popup_root_surface, get_popup_toplevel_coords, PopupKind, PopupManager, Space, Window},
    input::{
        pointer::{Focus, GrabStartData as PointerGrabStartData},
        Seat,
    },
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            protocol::{wl_seat, wl_surface::WlSurface},
            Resource,
        },
    },
    utils::{Rectangle, Serial},
    wayland::{
        compositor::with_states,
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
            XdgToplevelSurfaceData,
        },
    },
};

use crate::{
    grabs::{MoveSurfaceGrab, ResizeSurfaceGrab},
    model::{
        tiling::SplitDirection,
        geometry::Rect,
        grid::Direction,
    },
    RubixState,
};

impl XdgShellHandler for RubixState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }



    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // Register the window, then insert it into the model. It is not mapped
        // here -- apply_layout maps it once the model knows where it goes.
        let window = Window::new_wayland_window(surface);
        let id = self.next_window_id();
        self.windows.insert(id, window);

        // Split the currently-focused window (via seat keyboard focus, reverse
        // -looked-up to its id); an unfocused/empty case falls through to 0,
        // which add_window treats as "no target" (seeds the root if empty).
        let focused_id = self.focused_window_id();
        let direction = focused_id
            .and_then(|fid| self.window_rect(fid))
            .map(Rect::longer_axis)
            .unwrap_or(SplitDirection::Horizontal);
        self.monitor.add_window(direction, id, focused_id.unwrap_or(0));
        self.apply_layout();
        self.ipc_dirty = true;
        tracing::info!(
            "new toplevel -> window {id} ({} tracked, {} mapped in space)",
            self.windows.len(),
            self.space.elements().count(),
        );
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        // Reverse-lookup the destroyed surface's id, then evict it from both the
        // model and the registry and re-tile. Skips cleanly if the window was
        // never tracked (nothing to remove).
        let destroyed_id = self
            .windows
            .iter()
            .find(|(_, w)| w.toplevel().is_some_and(|t| t.wl_surface() == surface.wl_surface()))
            .map(|(id, _)| *id);

        if let Some(id) = destroyed_id {
            self.monitor.remove_window(id);
            self.windows.remove(&id);
            self.apply_layout();
            self.ipc_dirty = true;
        }
    }



    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.unconstrain_popup(&surface);
        let _ = self.popups.track_popup(PopupKind::Xdg(surface));
    }

    fn reposition_request(&mut self, surface: PopupSurface, positioner: PositionerState, token: u32) {
        surface.with_pending_state(|state| {
            let geometry = positioner.get_geometry();
            state.geometry = geometry;
            state.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }

    fn move_request(&mut self, surface: ToplevelSurface, seat: wl_seat::WlSeat, serial: Serial) {
        let seat = Seat::from_resource(&seat).unwrap();

        let wl_surface = surface.wl_surface();

        if let Some(start_data) = check_grab(&seat, wl_surface, serial) {
            let pointer = seat.get_pointer().unwrap();

            let window = self
                .space
                .elements()
                .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface.wl_surface()))
                .unwrap()
                .clone();
            let initial_window_location = self.space.element_location(&window).unwrap();

            let grab = MoveSurfaceGrab {
                start_data,
                window,
                initial_window_location,
            };

            pointer.set_grab(self, grab, serial, Focus::Clear);
        }
    }

    fn resize_request(
        &mut self,
        surface: ToplevelSurface,
        seat: wl_seat::WlSeat,
        serial: Serial,
        edges: xdg_toplevel::ResizeEdge,
    ) {
        let seat = Seat::from_resource(&seat).unwrap();

        let wl_surface = surface.wl_surface();

        if let Some(start_data) = check_grab(&seat, wl_surface, serial) {
            let pointer = seat.get_pointer().unwrap();

            let window = self
                .space
                .elements()
                .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface.wl_surface()))
                .unwrap()
                .clone();
            let initial_window_location = self.space.element_location(&window).unwrap();
            let initial_window_size = window.geometry().size;

            surface.with_pending_state(|state| {
                state.states.set(xdg_toplevel::State::Resizing);
            });

            surface.send_pending_configure();

            let grab = ResizeSurfaceGrab::start(
                start_data,
                window,
                edges.into(),
                Rectangle::new(initial_window_location, initial_window_size),
            );

            pointer.set_grab(self, grab, serial, Focus::Clear);
        }
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {
        // TODO popup grabs
    }
}

// Xdg Shell
delegate_xdg_shell!(RubixState);

fn check_grab(
    seat: &Seat<RubixState>,
    surface: &WlSurface,
    serial: Serial,
) -> Option<PointerGrabStartData<RubixState>> {
    let pointer = seat.get_pointer()?;

    // Check that this surface has a click grab.
    if !pointer.has_grab(serial) {
        return None;
    }

    let start_data = pointer.grab_start_data()?;

    let (focus, _) = start_data.focus.as_ref()?;
    // If the focus was for a different surface, ignore the request.
    if !focus.id().same_client_as(&surface.id()) {
        return None;
    }

    Some(start_data)
}

/// Should be called on `WlSurface::commit`
pub fn handle_commit(popups: &mut PopupManager, space: &Space<Window>, surface: &WlSurface) {
    // Handle toplevel commits.
    if let Some(window) = space
        .elements()
        .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface))
        .cloned()
    {
        let initial_configure_sent = with_states(surface, |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .unwrap()
                .lock()
                .unwrap()
                .initial_configure_sent
        });

        if !initial_configure_sent {
            window.toplevel().unwrap().send_configure();
        }
    }

    // Handle popup commits.
    popups.commit(surface);
    if let Some(popup) = popups.find_popup(surface) {
        match popup {
            PopupKind::Xdg(ref xdg) => {
                if !xdg.is_initial_configure_sent() {
                    // NOTE: This should never fail as the initial configure is always
                    // allowed.
                    xdg.send_configure().expect("initial configure failed");
                }
            }
            PopupKind::InputMethod(ref _input_method) => {}
        }
    }
}

impl RubixState {
    fn unconstrain_popup(&self, popup: &PopupSurface) {
        let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(popup.clone())) else {
            return;
        };
        let Some(window) = self
            .space
            .elements()
            .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == &root))
        else {
            return;
        };

        let output = self.space.outputs().next().unwrap();
        let output_geo = self.space.output_geometry(output).unwrap();
        let window_geo = self.space.element_geometry(window).unwrap();

        // The target geometry for the positioner should be relative to its parent's geometry, so
        // we will compute that here.
        let mut target = output_geo;
        target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));
        target.loc -= window_geo.loc;

        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }
    pub(crate) fn focused_window_id(& self) -> Option<u32> {
        let keyboard = self.seat.get_keyboard().unwrap();
        let focus = keyboard.current_focus();
        let focused_id = focus.and_then(|surface| {
            self.windows
                .iter()
                .find(|(_, w)| w.toplevel().is_some_and(|t| t.wl_surface() == &surface))
                .map(|(id, _)| *id)
        });
        focused_id
    }

    pub fn move_focused_window_to_new_column(&mut self) {
        let Some(id) = self.focused_window_id() else { return };
        self.monitor.move_window_to_new_column(id);
        self.apply_layout();
    }

    pub fn flip_focused_parent_split_direction(&mut self) {
        let Some(id) = self.focused_window_id() else { return };
        self.monitor.flip_split_direction(id);
        self.apply_layout();
    }

    pub fn move_focused_window_by_direction(&mut self,direction: Direction) {
        let Some(focused_id) = self.focused_window_id() else { return };
        let Some((c,g)) = self.monitor.find_group_by_direction(focused_id, direction) else { return };
        let split_direction = self.monitor.find_first_leaf_id(c,g)
            .and_then(|target_id| self.window_rect(target_id))
            .map(Rect::longer_axis)
            .unwrap_or(SplitDirection::Horizontal);
        self.monitor.move_window_to_group(focused_id, c, g, split_direction);
        self.apply_layout();
    }
}
