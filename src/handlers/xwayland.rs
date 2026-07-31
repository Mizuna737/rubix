use smithay::{
    delegate_xwayland_shell,
    desktop::Window,
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
    CalloopData, RubixState,
};

impl XWaylandShellHandler for RubixState {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
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
        let win = Window::new_x11_window(window);
        let id = self.next_window_id();
        self.windows.insert(id, win);

        // Mirror `xdg_shell::new_toplevel`: split the currently-focused window.
        let focused_id = self.focused_window_id();
        let direction = focused_id
            .and_then(|fid| self.window_rect(fid))
            .map(Rect::longer_axis)
            .unwrap_or(SplitDirection::Horizontal);
        if let Some(monitor) = self.workspace.active_monitor_mut() {
            monitor.add_window(direction, id, focused_id.unwrap_or(0));
        }
        self.apply_layout();
        // Focus follows spawn (mirrors xdg_shell::new_toplevel): name the new id
        // directly, since the focus-agnostic model won't surface it via re-derive.
        self.focus_by_id(id);
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
        // handshake completes.
        let target = window.wl_surface();
        let id = self
            .windows
            .iter()
            .find(|(_, win)| win.wl_surface().as_deref() == target.as_ref())
            .map(|(id, _)| *id);

        let rect = id.and_then(|id| self.window_rect(id)).map(|rect| {
            Rectangle::<i32, Logical>::new(
                (rect.x as i32, rect.y as i32).into(),
                (rect.width as i32, rect.height as i32).into(),
            )
        });

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
}

// NOTE: there is no `delegate_xwm!` macro in smithay 0.7.0 -- `XwmHandler` is
// consumed directly by calloop's X11 event source (`handle_event` in
// smithay::xwayland::xwm), not through a wayland-server `Dispatch` impl. It's
// still required on `RubixState` because `Dispatch<XwaylandShellV1, ..>` (see
// `delegate_xwayland_shell!` below) bounds its `D` on `XwmHandler +
// XWaylandShellHandler`, and `D` there is `RubixState` (the `Display`'s state
// type).
delegate_xwayland_shell!(RubixState);

// `X11Wm::start_wm`'s `D: XwmHandler + XWaylandShellHandler` is the calloop
// event-loop Data type, which in this project is `CalloopData` (wrapping
// `RubixState`), not `RubixState` directly. These two thin impls forward
// every call into the real logic on `RubixState` above.
impl XwmHandler for CalloopData {
    fn xwm_state(&mut self, xwm: XwmId) -> &mut X11Wm {
        self.state.xwm_state(xwm)
    }

    fn new_window(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.new_window(xwm, window)
    }

    fn new_override_redirect_window(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.new_override_redirect_window(xwm, window)
    }

    fn map_window_request(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.map_window_request(xwm, window)
    }

    fn mapped_override_redirect_window(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.mapped_override_redirect_window(xwm, window)
    }

    fn unmapped_window(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.unmapped_window(xwm, window)
    }

    fn destroyed_window(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.destroyed_window(xwm, window)
    }

    fn configure_request(
        &mut self,
        xwm: XwmId,
        window: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        reorder: Option<Reorder>,
    ) {
        self.state.configure_request(xwm, window, x, y, w, h, reorder)
    }

    fn configure_notify(
        &mut self,
        xwm: XwmId,
        window: X11Surface,
        geometry: Rectangle<i32, Logical>,
        above: Option<u32>,
    ) {
        self.state.configure_notify(xwm, window, geometry, above)
    }

    fn resize_request(&mut self, xwm: XwmId, window: X11Surface, button: u32, resize_edge: ResizeEdge) {
        self.state.resize_request(xwm, window, button, resize_edge)
    }

    fn move_request(&mut self, xwm: XwmId, window: X11Surface, button: u32) {
        self.state.move_request(xwm, window, button)
    }
}

impl XWaylandShellHandler for CalloopData {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        self.state.xwayland_shell_state()
    }
}
