use smithay::{
    desktop::Window,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Rectangle},
    wayland::{
        seat::WaylandFocus,
        xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
    },
    xwayland::{
        xwm::{Reorder, ResizeEdge, XwmId},
        X11Surface, X11Wm, XwmHandler,
    },
};

use crate::{
    model::{geometry::Rect, tiling::SplitDirection},
    RubixState,
};

impl XWaylandShellHandler for RubixState {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }

    // Completion of the focus handover started in `map_window_request`, not a
    // new focus-stealing path: at map time the `wl_surface` didn't exist yet,
    // so `focus_by_id` there silently resolved to nothing and recorded its
    // intent in `pending_x11_focus`. This fires once that surface exists. Only
    // the window whose focus attempt was actually lost is re-focused here --
    // every other surface association is left alone.
    fn surface_associated(&mut self, _xwm: XwmId, _wl_surface: WlSurface, window: X11Surface) {
        if let Some(id) = self.window_id_for_x11(&window) {
            if self.pending_x11_focus == Some(id) {
                self.pending_x11_focus = None;
                self.focus_by_id(id);
                self.ipc_dirty = true;
            }
        }
    }
}

// Reverse-lookup the tracked id whose `Window` wraps this X11 surface (by
// comparing `wl_surface`s), then evict it from the model, the registry, and
// the space, and re-tile. Mirrors `xdg_shell::toplevel_destroyed`, but also
// unmaps from `space` explicitly -- unlike a destroyed wayland toplevel, an
// unmapped/destroyed X11Surface isn't guaranteed to go `!alive()` in time for
// `space.refresh()` to prune it on its own.
fn remove_x11_window(state: &mut RubixState, window: &X11Surface) {
    let target = window.wl_surface();
    // `target.as_ref()` below compares `Option<&WlSurface> == Option<&WlSurface>`,
    // so a `None` target would match the first tracked window that also has no
    // surface -- an arbitrary victim, since `self.windows` is a HashMap and its
    // iteration order isn't stable. Smithay detaches the wl_surface right after
    // `unmapped_window` returns, so this is defensive rather than a live path
    // today, but the failure mode it prevents is evicting an unrelated window.
    if target.is_none() {
        return;
    }
    let id = state
        .windows
        .iter()
        .find(|(_, w)| w.wl_surface().as_deref() == target.as_ref())
        .map(|(id, _)| *id);

    if let Some(id) = id {
        // No-op if this id was never added to the tiling model (OR windows). A
        // destroyed window may be on any monitor, not just the active one --
        // remove_window is id-based and a no-op when absent, so sweeping all
        // monitors is safe.
        for monitor in &mut state.workspace.monitors {
            monitor.remove_window(id);
        }
        if let Some(win) = state.windows.remove(&id) {
            state.space.unmap_elem(&win);
        }
        state.fullscreen_windows.remove(&id);
        state.iconified.remove(&id);
        state.apply_layout();
        state.ipc_dirty = true;
    }
}

impl XwmHandler for RubixState {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.xwm.as_mut().expect("xwm started")
    }

    fn new_window(&mut self, _xwm: XwmId, window: X11Surface) {
        tracing::debug!(window = window.window_id(), "new X11 window");
    }

    fn new_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        tracing::debug!(window = window.window_id(), "new X11 override-redirect window");
    }

    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let _ = window.set_mapped(true);
        // Read before the surface is moved into `Window::new_x11_window` below:
        // EWMH clients that want to start fullscreen set `_NET_WM_STATE` before
        // `XMapWindow`, and the pinned smithay fork already folds that into
        // `net_state` ahead of dispatching here, so `is_fullscreen()` is accurate
        // at this point. The post-map `_NET_WM_STATE` ClientMessage path
        // (`fullscreen_request`) never fires for this case.
        let starts_fullscreen = window.is_fullscreen();
        let class = window.class();
        let title = window.title();
        let win = Window::new_x11_window(window);
        let id = self.next_window_id();
        self.windows.insert(id, win);

        if starts_fullscreen {
            self.set_window_fullscreen(id, true);
            tracing::info!(
                "X11 window {id} maps already fullscreen (pre-map _NET_WM_STATE): class={class:?} title={title:?}"
            );
        }

        // Mirror `xdg_shell::new_toplevel`: split the currently-focused window.
        // A window that mapped already fullscreen lives outside the grid, so it
        // must not be given a tile slot here.
        if !starts_fullscreen {
            let focused_id = self.focused_window_id();
            let direction = focused_id
                .and_then(|fid| self.window_rect(fid))
                .map(Rect::longer_axis)
                .unwrap_or(SplitDirection::Horizontal);
            if let Some(monitor) = self.workspace.active_monitor_mut() {
                monitor.add_window(direction, id, focused_id.unwrap_or(0));
            }
        }
        // Focus BEFORE laying out. `apply_layout` emits a rect for a fullscreen
        // window only while it is focused, so laying out first hands a window
        // that mapped already fullscreen no rect at all -- it never reaches the
        // Space, and `sync_x11_iconic` immediately hides it for good measure.
        // Focus follows spawn (mirrors xdg_shell::new_toplevel): name the new id
        // directly, since the focus-agnostic model won't surface it via re-derive.
        self.focus_by_id(id);
        // XWayland associates the `wl_surface` in a separate ClientMessage that
        // usually lands after this one, and focus is resolved by matching that
        // surface -- so the focus above quietly went nowhere. Record the intent
        // and let `surface_associated` finish the job. Left unrecorded, a window
        // that maps fullscreen is never the focused window, never gets a rect,
        // and is iconified before it has drawn a frame.
        if self.focused_window_id() != Some(id) {
            self.pending_x11_focus = Some(id);
        }
        self.apply_layout();
        self.ipc_dirty = true;
        tracing::info!(
            "new X11 toplevel -> window {id} ({} tracked, {} mapped in space)",
            self.windows.len(),
            self.space.elements().count(),
        );
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        // OR windows own their own geometry -- no tiling, no configure. Insert
        // into `self.windows` (so it renders + is destroyable) but do NOT add
        // to any monitor in `self.workspace`.
        let loc = window.geometry().loc;
        let id = self.next_window_id();
        let win = Window::new_x11_window(window);
        self.windows.insert(id, win.clone());
        // activate=true so it stacks above the tiled windows.
        self.space.map_element(win, (loc.x, loc.y), true);
        self.ipc_dirty = true;
    }

    fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
        let _ = window.set_mapped(false);
        remove_x11_window(self, &window);
    }

    fn destroyed_window(&mut self, _xwm: XwmId, window: X11Surface) {
        remove_x11_window(self, &window);
    }

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        if window.is_override_redirect() {
            // OR windows own their geometry -- honor the request, filling any
            // missing fields from the current geometry.
            let current = window.geometry();
            let rect = Rectangle::<i32, Logical>::new(
                (x.unwrap_or(current.loc.x), y.unwrap_or(current.loc.y)).into(),
                (
                    w.map(|w| w as i32).unwrap_or(current.size.w),
                    h.map(|h| h as i32).unwrap_or(current.size.h),
                )
                    .into(),
            );
            let _ = window.configure(Some(rect));
            return;
        }

        // Tiled: deny the client geometry, tiling owns it. Reply with our
        // current tile rect if we have one yet, else echo the request so the
        // handshake completes. Fullscreen windows get the full output bounds.
        let target = window.wl_surface();
        let id = self
            .windows
            .iter()
            .find(|(_, win)| win.wl_surface().as_deref() == target.as_ref())
            .map(|(id, _)| *id);

        let rect = if let Some(id) = id {
            if self.fullscreen_windows.contains(&id) {
                // Fullscreen: provide the full output geometry
                self.workspace
                    .active_monitor()
                    .and_then(|m| self.output_bounds_for(m.id))
                    .map(|bounds| {
                        Rectangle::<i32, Logical>::new(
                            (bounds.x as i32, bounds.y as i32).into(),
                            (bounds.width as i32, bounds.height as i32).into(),
                        )
                    })
            } else {
                // Tiled: provide the current tile rect
                self.window_rect(id).map(|rect| {
                    Rectangle::<i32, Logical>::new(
                        (rect.x as i32, rect.y as i32).into(),
                        (rect.width as i32, rect.height as i32).into(),
                    )
                })
            }
        } else {
            None
        };

        // A game that "goes fullscreen" by sizing itself to the screen, rather
        // than by setting _NET_WM_STATE_FULLSCREEN, is indistinguishable from an
        // ordinary resize unless the request itself is visible. Logging what was
        // asked for against what tiling granted is what separates "the client
        // never asked" from "we said no". Clients ask rarely, so this is not a
        // per-frame path.
        tracing::info!(
            "X11 configure_request: class={:?} asked={:?}x{:?} at {:?},{:?} -> granted {:?} (window {:?}, fullscreen={})",
            window.class(),
            w,
            h,
            x,
            y,
            rect,
            id,
            id.is_some_and(|i| self.fullscreen_windows.contains(&i)),
        );

        match rect {
            Some(rect) => {
                let _ = window.configure(Some(rect));
            }
            None => {
                let current = window.geometry();
                let echoed = Rectangle::<i32, Logical>::new(
                    (x.unwrap_or(current.loc.x), y.unwrap_or(current.loc.y)).into(),
                    (
                        w.map(|w| w as i32).unwrap_or(current.size.w),
                        h.map(|h| h as i32).unwrap_or(current.size.h),
                    )
                        .into(),
                );
                let _ = window.configure(Some(echoed));
            }
        }
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        geometry: Rectangle<i32, Logical>,
        _above: Option<u32>,
    ) {
        // Only matters for OR windows -- tiled windows are geometry we drove,
        // so this is a no-op for them.
        if !window.is_override_redirect() {
            return;
        }
        let id = self
            .windows
            .iter()
            .find(|(_, win)| win.wl_surface().as_deref() == window.wl_surface().as_ref())
            .map(|(id, _)| *id);
        if let Some(win) = id.and_then(|id| self.windows.get(&id).cloned()) {
            self.space.map_element(win, geometry.loc, false);
        }
    }

    fn resize_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32, _resize_edge: ResizeEdge) {
        // Rubix owns layout -- no client-driven resize.
    }

    fn move_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32) {
        // Rubix owns layout -- no client-driven move.
    }

    fn fullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        // Logged because the post-map path was previously silent: a game that
        // sets _NET_WM_STATE_FULLSCREEN after mapping left no trace at all, so
        // "the game never went fullscreen" and "the game never asked" were
        // indistinguishable in a session log. Rare enough not to be noise.
        tracing::info!(
            "X11 fullscreen_request: class={:?} title={:?} geometry={:?} -> window {:?}",
            window.class(),
            window.title(),
            window.geometry(),
            self.window_id_for_x11(&window),
        );
        if let Some(id) = self.window_id_for_x11(&window) {
            self.set_window_fullscreen(id, true);
            self.apply_layout();
            self.ipc_dirty = true;
        }
    }

    fn unfullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        tracing::info!(
            "X11 unfullscreen_request: class={:?} title={:?} -> window {:?}",
            window.class(),
            window.title(),
            self.window_id_for_x11(&window),
        );
        if let Some(id) = self.window_id_for_x11(&window) {
            self.set_window_fullscreen(id, false);
            self.apply_layout();
            self.ipc_dirty = true;
        }
    }

    /// `_NET_ACTIVE_WINDOW` -- the X11 half of the reveal seam.
    ///
    /// This is what `wmctrl -a`, a taskbar click, and an app raising its own
    /// existing window all send, and it is the exact counterpart to
    /// wlr-foreign-toplevel's `Activate` (see `foreign_toplevel.rs`). Both route
    /// to `focus_by_id`, which is what makes the request meaningful rather than
    /// merely accepted: an X11 window can be off-screen in two ways the grid
    /// won't fix on its own -- scrolled to a non-active row, or in a column past
    /// `visible_columns` -- and `focus_by_id` reveals it, syncs the active
    /// monitor, and arms the matching transition before handing over the
    /// keyboard.
    ///
    /// Honoured unconditionally, matching the Wayland path. Neither seam gates
    /// on focus-stealing heuristics today; if one ever grows them, both should,
    /// or the two protocols disagree about what a window is allowed to do.
    ///
    /// `timestamp` and `currently_active_window` are unused: Rubix has no
    /// focus-stealing-prevention window to compare the timestamp against, and
    /// the source window only matters for same-client focus handoff, which ends
    /// at the same place regardless of where it came from.
    fn active_window_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        _timestamp: u32,
        _currently_active_window: Option<X11Surface>,
    ) {
        if let Some(id) = self.window_id_for_x11(&window) {
            self.focus_by_id(id);
            self.ipc_dirty = true;
        }
    }
}

impl RubixState {
    /// Rubix's window id for an `X11Surface`, matched through its `WlSurface`.
    ///
    /// XWayland hands the handlers an `X11Surface`, but every Rubix-side map is
    /// keyed by the compositor's own u32, so each entry point has to cross that
    /// boundary before it can do anything.
    fn window_id_for_x11(&self, window: &X11Surface) -> Option<u32> {
        let target = window.wl_surface();
        self.windows
            .iter()
            .find(|(_, w)| w.wl_surface().as_deref() == target.as_ref())
            .map(|(id, _)| *id)
    }
}

// NOTE: there is no `delegate_xwm!` macro -- `XwmHandler` is consumed directly
// by calloop's X11 event source (`handle_event` in smithay::xwayland::xwm),
// not through a wayland-server `Dispatch` impl. It's still required on
// `RubixState` because the fork's unified `delegate_dispatch2!(RubixState)`
// (handlers/mod.rs) generates `Dispatch<XwaylandShellV1, ..>` for any state
// implementing `XWaylandShellHandler`, and the surface bind path additionally
// needs `XwmHandler` bound, with `D` there being `RubixState` (the `Display`'s
// state type).
//
// `RubixState` is now the calloop event-loop `Data` type directly (the
// `CalloopData` wrapper was removed), so the impls above already satisfy
// `X11Wm::start_wm`'s `D: XwmHandler + XWaylandShellHandler` bound with no
// forwarding shim needed.
//
// The fork's `X11Wm::start_wm` additionally requires `D: SeatHandler +
// DndGrabHandler` (plus `DndFocus<D>` on the pointer/touch focus types, which
// is blanket-implemented for `WlSurface` given `SeatHandler + DataDeviceHandler`
// -- both already implemented for `RubixState` in `handlers/mod.rs`).
// `DndGrabHandler`'s `dropped`/`cancelled` hooks default to no-ops; Rubix owns
// layout/focus itself and doesn't need custom XWayland drag'n'drop behavior.
impl smithay::input::dnd::DndGrabHandler for RubixState {}
