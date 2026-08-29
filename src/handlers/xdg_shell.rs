use smithay::{
    desktop::{find_popup_root_surface, get_popup_toplevel_coords, PopupKind, PopupManager, Space, Window},
    input::{
        pointer::{Focus, GrabStartData as PointerGrabStartData},
        Seat,
    },
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            protocol::{wl_seat, wl_output, wl_surface::WlSurface},
            Resource,
        },
    },
    utils::{Rectangle, Serial},
    wayland::{
        compositor::with_states,
        seat::WaylandFocus,
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
        grid::{Direction, RevealKind},
        cube::find_target_by_direction,
    },
    state::Transition,
    RubixState,
};

impl XdgShellHandler for RubixState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }



    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // Client pid (best-effort): names which process owns a toplevel, so a
        // rapid create/destroy loop points straight at the offending app. Read
        // before `surface` is moved into the Window below.
        let client_pid = surface
            .wl_surface()
            .client()
            .and_then(|c| c.get_credentials(&self.display_handle).ok())
            .map(|cred| cred.pid);
        // Register the window and send the initial configure, but stage it as
        // unmapped -- it does not enter the model/space/focus/IPC until it
        // commits a first buffer (see handle_commit below). A client that
        // creates a toplevel and never maps it (e.g. a headless clipboard
        // reader grabbing the selection) then leaves no trace.
        let window = Window::new_wayland_window(surface);
        window.toplevel().unwrap().send_configure();
        let id = self.next_window_id();
        self.unmapped.insert(id, window);
        tracing::info!(
            "new toplevel -> window {id} pid={client_pid:?} (unmapped, {} pending)",
            self.unmapped.len(),
        );
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        // A toplevel that never mapped just drops out of `unmapped` -- it was
        // never in the model, so there's nothing to remove/re-tile.
        let unmapped_id = self
            .unmapped
            .iter()
            .find(|(_, w)| w.toplevel().is_some_and(|t| t.wl_surface() == surface.wl_surface()))
            .map(|(id, _)| *id);
        if let Some(id) = unmapped_id {
            self.unmapped.remove(&id);
            // A toplevel can request fullscreen (Deliverable 2) before its
            // first commit; if it dies before mapping, the id must not linger
            // in fullscreen_windows -- apply_layout's raise loop and the
            // scanout target iterate it regardless of whether the id is
            // actually a live, tracked window.
            self.fullscreen_windows.remove(&id);
            tracing::info!(
                "unmapped toplevel destroyed -> window {id} ({} still pending)",
                self.unmapped.len(),
            );
            return;
        }

        // Reverse-lookup the destroyed surface's id, then evict it from both the
        // model and the registry and re-tile. Skips cleanly if the window was
        // never tracked (nothing to remove).
        let destroyed_id = self
            .windows
            .iter()
            .find(|(_, w)| w.toplevel().is_some_and(|t| t.wl_surface() == surface.wl_surface()))
            .map(|(id, _)| *id);

        if let Some(id) = destroyed_id {
            self.remove_window_by_id(id);
            self.apply_layout();
            self.ipc_dirty = true;
            tracing::info!(
                "toplevel destroyed -> window {id} ({} tracked, {} mapped in space)",
                self.windows.len(),
                self.space.elements().count(),
            );
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

    fn fullscreen_request(&mut self, surface: ToplevelSurface, _wl_output: Option<wl_output::WlOutput>) {
        let wl_surface = surface.wl_surface();
        if let Some((id, _)) = self.windows.iter().find(|(_, w)| w.toplevel().is_some_and(|t| t.wl_surface() == wl_surface)) {
            let id = *id;
            self.set_window_fullscreen(id, true);
            surface.with_pending_state(|state| {
                state.states.set(xdg_toplevel::State::Fullscreen);
            });
            surface.send_pending_configure();
            self.apply_layout();
            self.ipc_dirty = true;
            return;
        }

        // Not mapped yet: the client called set_fullscreen before its first
        // commit -- the normal way a game starts fullscreen. Stage the
        // fullscreen state now so it is live by the time `handle_commit`
        // promotes the id into `self.windows`; do NOT call `apply_layout()`
        // here, the window isn't in the model yet and the promotion path
        // already calls it.
        if let Some((id, _)) = self
            .unmapped
            .iter()
            .find(|(_, w)| w.toplevel().is_some_and(|t| t.wl_surface() == wl_surface))
        {
            let id = *id;
            self.fullscreen_windows.insert(id);
            let bounds = self
                .workspace
                .active_monitor()
                .and_then(|m| self.output_bounds_for(m.id));
            surface.with_pending_state(|state| {
                state.states.set(xdg_toplevel::State::Fullscreen);
                if let Some(bounds) = bounds {
                    state.size = Some((bounds.width as i32, bounds.height as i32).into());
                }
            });
            surface.send_pending_configure();
            tracing::info!("pre-map fullscreen request captured -> window {id}");
        }
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        let wl_surface = surface.wl_surface();
        if let Some((id, _)) = self.windows.iter().find(|(_, w)| w.toplevel().is_some_and(|t| t.wl_surface() == wl_surface)) {
            let id = *id;
            self.set_window_fullscreen(id, false);
            surface.with_pending_state(|state| {
                state.states.unset(xdg_toplevel::State::Fullscreen);
            });
            surface.send_pending_configure();
            self.apply_layout();
            self.ipc_dirty = true;
            return;
        }

        if let Some((id, _)) = self
            .unmapped
            .iter()
            .find(|(_, w)| w.toplevel().is_some_and(|t| t.wl_surface() == wl_surface))
        {
            let id = *id;
            self.fullscreen_windows.remove(&id);
            surface.with_pending_state(|state| {
                state.states.unset(xdg_toplevel::State::Fullscreen);
            });
            surface.send_pending_configure();
        }
    }
}

// Xdg Shell: see handlers/mod.rs for the single `delegate_dispatch2!(RubixState)`
// call that now covers this (and every other) protocol.

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

        // Constrain against the output the popup's parent toplevel actually
        // sits on (multi-monitor), falling back to the first known output --
        // gracefully, no unwrap -- if for some reason the parent's location
        // can't be resolved (e.g. no outputs at all).
        let Some(window_geo) = self.space.element_geometry(window) else { return; };
        let output = self
            .output_at(window_geo.loc.to_f64())
            .or_else(|| self.space.outputs().next().cloned());
        let Some(output) = output else { return; };
        let Some(output_geo) = self.space.output_geometry(&output) else { return; };

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
        // Match by wl_surface (not toplevel()) so X11 focus resolves too --
        // toplevel() is None for X11 windows.
        let focus = keyboard.current_focus().and_then(|t| t.surface());
        let focused_id = focus.and_then(|surface| {
            self.windows
                .iter()
                .find(|(_, w)| w.wl_surface().is_some_and(|s| s.as_ref() == &surface))
                .map(|(id, _)| *id)
        });
        focused_id
    }

    // NOTE: focused-window ops route through the ACTIVE monitor -- this
    // assumes the keyboard-focused window is on the active monitor, which
    // holds true after nav (nav mutates + refocuses the active monitor).
    // Click-to-focus on another head not yet syncing active_monitor is a
    // known follow-up, not addressed here.
    /// Promote the focused window into a fresh column on one side of the active
    /// one, widening the visible band so it stays on screen.
    pub fn move_focused_window_to_new_column(&mut self, to_left: bool) {
        let Some(id) = self.focused_window_id() else { return };
        if let Some(monitor) = self.workspace.active_monitor_mut() {
            monitor.move_window_to_new_column(id, to_left);
        }
        self.apply_layout();
    }

    /// Move the cursor to another monitor, wrapping. No-op on a single head.
    pub fn focus_monitor_by_offset(&mut self, motion: isize) {
        let Some(id) = self.workspace.monitor_id_by_offset(motion) else { return };
        self.set_active_monitor(id);
        self.apply_layout();
    }

    /// Carry the focused window to another monitor, cursor following it there.
    pub fn move_focused_window_to_monitor(&mut self, motion: isize) {
        let Some(id) = self.focused_window_id() else { return };
        let Some(dest_id) = self.workspace.monitor_id_by_offset(motion) else { return };
        // Split beside whatever is focused on the DESTINATION, along that
        // window's longer axis -- the rule new windows follow. Its rect has to be
        // computed against the destination's own output bounds, so `window_rect`
        // is no help: that one is scoped to the active monitor, which is still
        // the source at this point.
        let split = self.workspace.monitors.iter()
            .find(|m| m.id == dest_id)
            .and_then(|monitor| {
                let bounds = self.output_bounds_for(monitor.id)?;
                let active = monitor.active_window()?;
                monitor
                    .compute_layout(bounds, self.config.outer_gap, self.config.inner_gap)
                    .into_iter()
                    .find(|(wid, _)| *wid == active)
                    .map(|(_, rect)| rect)
            })
            .map(Rect::longer_axis)
            .unwrap_or(SplitDirection::Horizontal);
        self.workspace.move_window_to_monitor(id, dest_id, split);
        self.apply_layout();
    }

    /// Trade the active group with its neighbour in `direction`, cursor
    /// following the group that moved.
    pub fn swap_active_group_by_direction(&mut self, direction: Direction) {
        if let Some(monitor) = self.workspace.active_monitor_mut() {
            monitor.swap_active_group(direction);
        }
        self.apply_layout();
    }

    pub fn flip_focused_parent_split_direction(&mut self) {
        let Some(id) = self.focused_window_id() else { return };
        if let Some(monitor) = self.workspace.active_monitor_mut() {
            monitor.flip_split_direction(id);
        }
        self.apply_layout();
    }

    /// Move keyboard focus to the nearest window in `direction`, crossing group
    /// and column divides and wrapping at the edges of the cube.
    ///
    /// The destination is resolved geometrically against the cube plane (see
    /// `model::cube`), which spans every column and row -- including the ones
    /// off screen -- so a single press can reach a window the renderer has never
    /// drawn. `reveal_slot` is what then brings it into view, scrolling its
    /// column or spinning the cube as needed.
    ///
    /// Focus itself is left to `dispatch_nav`'s refocus tail: this function only
    /// moves the model's cursor and arms the transition.
    pub fn focus_window_by_direction(&mut self, direction: Direction) {
        // Read phase. focused_window_id may be None (nothing focused yet) or name
        // a fullscreen window, which sits outside the grid and so never appears
        // on the plane; find_target_by_direction falls back to the active slot in
        // both cases rather than giving up.
        let focused_id = self.focused_window_id();
        let Some(monitor) = self.workspace.active_monitor() else { return };
        let Some(bounds) = self.output_bounds_for(monitor.id) else { return };
        let Some(target) = find_target_by_direction(monitor, bounds, focused_id, direction) else { return };

        let Some(monitor) = self.workspace.active_monitor_mut() else { return };
        // Reveal first, THEN place the cursor: the rotation branch relocates the
        // group, so the slot to point at is the one reveal_slot reports, not the
        // one the target named.
        let Some((kind, col, row)) = monitor.reveal_slot(target.column, target.row, Some(direction)) else { return };
        match target.window {
            // The window nav actually chose, not the group's remembered one --
            // stepping one window right must not leapfrog into whatever that
            // group was last focused on. focus_window sets all three pointers.
            Some(id) => { monitor.focus_window(id); }
            // An empty group: nothing to focus, so just park the cursor there.
            None => { monitor.set_active_slot(col, row); }
        }

        // Armed before apply_layout, which consumes pending_transition via take().
        match kind {
            RevealKind::Scrolled { down } => self.pending_transition = Some(Transition::Scroll { down }),
            RevealKind::Rotated => self.pending_transition = Some(Transition::Rotate),
            RevealKind::Swapped => self.pending_transition = Some(Transition::Reveal),
            RevealKind::AlreadyVisible => {}
        }
    }

    pub fn move_focused_window_by_direction(&mut self,direction: Direction) {
        let Some(focused_id) = self.focused_window_id() else { return };
        // Read phase: two shared borrows of self (via active_monitor()) are
        // fine together, including the nested self.window_rect() call -- only
        // the write below needs the mutable borrow, taken separately.
        let Some(monitor) = self.workspace.active_monitor() else { return };
        let Some((c,g)) = monitor.find_group_by_direction(focused_id, direction) else { return };
        let split_direction = monitor.find_first_leaf_id(c,g)
            .and_then(|target_id| self.window_rect(target_id))
            .map(Rect::longer_axis)
            .unwrap_or(SplitDirection::Horizontal);
        let Some(monitor) = self.workspace.active_monitor_mut() else { return };
        monitor.move_window_to_group(focused_id, c, g, split_direction);
        self.apply_layout();
    }
}
