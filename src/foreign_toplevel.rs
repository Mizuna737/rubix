//! wlr-foreign-toplevel-management-unstable-v1
//! (`zwlr_foreign_toplevel_manager_v1` / `_handle_v1`).
//!
//! The window-list protocol: it publishes one handle per toplevel, carrying
//! title/app_id/state, and lets a client activate or close one. This is what
//! `rofi -show window` binds to under Wayland -- without it rofi refuses the
//! mode outright ("compositor does not support wlr-foreign-toplevel-management")
//! -- and it is equally what taskbars and `wlrctl` use.
//!
//! It matters more here than in a conventional compositor: a Rubix window can be
//! genuinely unreachable by keyboard alone. Fullscreen windows live outside the
//! tiling grid, so `focus_active_window` -- which walks active_column ->
//! active_row -> first leaf -- can never land on one, and X11 fullscreen clients
//! are additionally hidden rather than un-fullscreened (see
//! `reconcile_focus_state`). `activate` is the way back to them.
//!
//! Smithay 0.7 ships no helper for this protocol -- its `foreign_toplevel_list`
//! module implements the newer `ext-foreign-toplevel-list-v1`, which is
//! list-only and has no `activate` -- so, as with `screencopy.rs`, the
//! `GlobalDispatch`/`Dispatch` impls are hand-rolled on the raw bindings smithay
//! re-exports from `wayland-protocols-wlr`.
//!
//! State is pushed, not polled: [`refresh`] diffs the live window set against
//! what each handle was last told and emits only the changes, driven from the
//! same `ipc_dirty` edge in the run loop that feeds the status bar.

use std::collections::HashMap;

use smithay::{
    output::Output,
    reexports::{
        wayland_protocols_wlr::foreign_toplevel::v1::server::{
            zwlr_foreign_toplevel_handle_v1::{self, State as ToplevelState, ZwlrForeignToplevelHandleV1},
            zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
        },
        wayland_server::{
            Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
        },
    },
};

use crate::RubixState;
use crate::state::MaximizeState;

/// Advertise the manager global. Version 3 (adds `parent`; rofi binds 3).
pub fn init(dh: &DisplayHandle) {
    dh.create_global::<RubixState, ZwlrForeignToplevelManagerV1, ()>(3, ());
}

/// Per-handle `Dispatch` user-data: the Rubix window id the handle stands for.
/// Handles outlive their window by one round trip (the client may still send
/// requests after `closed`), so every request re-resolves the id and quietly
/// does nothing when the window is gone.
pub struct ToplevelHandleData {
    id: u32,
}

/// What one window's handles have already been told, so `refresh` can send
/// deltas instead of the whole identity every cycle.
struct ToplevelEntry {
    handles: Vec<ZwlrForeignToplevelHandleV1>,
    app_id: Option<String>,
    title: Option<String>,
    states: Vec<ToplevelState>,
    outputs: Vec<Output>,
}

#[derive(Default)]
pub struct ForeignToplevelState {
    managers: Vec<ZwlrForeignToplevelManagerV1>,
    toplevels: HashMap<u32, ToplevelEntry>,
}

impl GlobalDispatch<ZwlrForeignToplevelManagerV1, ()> for RubixState {
    fn bind(
        state: &mut Self,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrForeignToplevelManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let manager = data_init.init(resource, ());
        state.foreign_toplevel.managers.push(manager);
        // A newly bound manager has been told nothing, so the next refresh must
        // announce every existing window to it.
        state.ipc_dirty = true;
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for RubixState {
    fn request(
        state: &mut Self,
        _client: &Client,
        manager: &ZwlrForeignToplevelManagerV1,
        request: zwlr_foreign_toplevel_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_foreign_toplevel_manager_v1::Request::Stop => {
                // `finished` is a destructor event: the client gets no further
                // toplevels, and existing handles stay valid until it destroys
                // them, so only the manager is forgotten here.
                manager.finished();
                state.foreign_toplevel.managers.retain(|m| m != manager);
            }
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        manager: &ZwlrForeignToplevelManagerV1,
        _data: &(),
    ) {
        state.foreign_toplevel.managers.retain(|m| m != manager);
    }
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ToplevelHandleData> for RubixState {
    fn request(
        state: &mut Self,
        _client: &Client,
        handle: &ZwlrForeignToplevelHandleV1,
        request: zwlr_foreign_toplevel_handle_v1::Request,
        data: &ToplevelHandleData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let id = data.id;
        if !state.windows.contains_key(&id) {
            return;
        }

        match request {
            // The point of the protocol for us: the only route back to a window
            // the grid cannot reach. Two distinct cases, both handled inside
            // focus_by_id:
            //
            //   fullscreen -- outside the grid entirely. Focusing a hidden X11
            //   fullscreen client is enough to restore it: `apply_layout` admits
            //   a fullscreen window to the visible set exactly when it holds
            //   focus, and `sync_x11_iconic` un-hides whatever is visible.
            //
            //   tiled but off-screen -- `Monitor::reveal_window` scrolls its
            //   column to the right row, or trades its group into the active
            //   slot when the column itself is past visible_columns.
            zwlr_foreign_toplevel_handle_v1::Request::Activate { .. } => {
                state.focus_by_id(id);
                state.ipc_dirty = true;
            }
            zwlr_foreign_toplevel_handle_v1::Request::Close => {
                state.close_window(id);
            }
            zwlr_foreign_toplevel_handle_v1::Request::SetFullscreen { .. } => {
                state.set_window_fullscreen(id, true);
                state.apply_layout();
                state.ipc_dirty = true;
            }
            zwlr_foreign_toplevel_handle_v1::Request::UnsetFullscreen => {
                state.set_window_fullscreen(id, false);
                state.apply_layout();
                state.ipc_dirty = true;
            }
            // A client asking to be maximized means the whole work area -- the
            // Group stage is a Rubix-internal step in the keybind's cycle, not
            // anything the protocol can express.
            zwlr_foreign_toplevel_handle_v1::Request::SetMaximized => {
                state.maximized = MaximizeState::Monitor(id);
                state.apply_layout();
                state.ipc_dirty = true;
            }
            zwlr_foreign_toplevel_handle_v1::Request::UnsetMaximized => {
                if state.maximized.window() == Some(id) {
                    state.maximized = MaximizeState::None;
                    state.apply_layout();
                    state.ipc_dirty = true;
                }
            }
            // Minimize is not a state a client may ask for here: hiding is a
            // consequence of the layout (`sync_x11_iconic`), not a property a
            // window owns, so honouring these would put the two out of sync.
            // `set_rectangle` only ever fed minimize animations.
            zwlr_foreign_toplevel_handle_v1::Request::SetMinimized
            | zwlr_foreign_toplevel_handle_v1::Request::UnsetMinimized
            | zwlr_foreign_toplevel_handle_v1::Request::SetRectangle { .. } => {}
            zwlr_foreign_toplevel_handle_v1::Request::Destroy => {
                forget_handle(state, id, handle);
            }
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        handle: &ZwlrForeignToplevelHandleV1,
        data: &ToplevelHandleData,
    ) {
        forget_handle(state, data.id, handle);
    }
}

fn forget_handle(state: &mut RubixState, id: u32, handle: &ZwlrForeignToplevelHandleV1) {
    if let Some(entry) = state.foreign_toplevel.toplevels.get_mut(&id) {
        entry.handles.retain(|h| h != handle);
    }
}

/// The states a window is currently in, in the order the protocol enumerates
/// them so the comparison against the last-sent set is a plain slice equality.
fn states_for(state: &RubixState, id: u32, focused: Option<u32>) -> Vec<ToplevelState> {
    let mut states = Vec::new();
    if state.maximized.window() == Some(id) {
        states.push(ToplevelState::Maximized);
    }
    // Hidden-by-layout is the closest thing Rubix has to minimized, and it is
    // what a taskbar wants to grey out.
    if state.iconified.contains(&id) {
        states.push(ToplevelState::Minimized);
    }
    if focused == Some(id) {
        states.push(ToplevelState::Activated);
    }
    if state.fullscreen_windows.contains(&id) {
        states.push(ToplevelState::Fullscreen);
    }
    states
}

/// Push window-list changes to every bound manager.
///
/// Called on the `ipc_dirty` edge in the run loop, alongside the status-bar
/// broadcast -- the same signal, since both describe "the window set changed".
pub fn refresh(state: &mut RubixState) {
    // Taken out wholesale so the diff can read `state` freely; put back intact
    // below. Nothing in the loop can re-enter here (no client dispatch runs).
    let mut ft = std::mem::take(&mut state.foreign_toplevel);
    let dh = state.display_handle.clone();
    let focused = state.focused_window_id();

    // Gone: tell every handle its window is closed, then drop the entry. The
    // handle objects survive until the client destroys them, per the protocol.
    ft.toplevels.retain(|id, entry| {
        if state.windows.contains_key(id) {
            return true;
        }
        for handle in &entry.handles {
            handle.closed();
        }
        false
    });

    for id in state.windows.keys().copied().collect::<Vec<_>>() {
        let (app_id, title) = state.window_identity(id);
        let states = states_for(state, id, focused);
        let outputs: Vec<Output> = state
            .windows
            .get(&id)
            .map(|window| state.space.outputs_for_element(window))
            .unwrap_or_default();

        let entry = ft.toplevels.entry(id).or_insert_with(|| ToplevelEntry {
            handles: Vec::new(),
            // Deliberately the inverse of "nothing known": a fresh entry must
            // send its identity even when it is empty, so seed with values the
            // diff below cannot match.
            app_id: None,
            title: None,
            states: Vec::new(),
            outputs: Vec::new(),
        });

        // Announce the window to any manager that has not seen it yet.
        let mut fresh: Vec<ZwlrForeignToplevelHandleV1> = Vec::new();
        for manager in &ft.managers {
            let Some(client) = manager.client() else { continue };
            let seen = entry
                .handles
                .iter()
                .any(|h| h.client().is_some_and(|c| c.id() == client.id()));
            if seen {
                continue;
            }
            let Ok(handle) = client.create_resource::<ZwlrForeignToplevelHandleV1, _, RubixState>(
                &dh,
                manager.version(),
                ToplevelHandleData { id },
            ) else {
                continue;
            };
            manager.toplevel(&handle);
            fresh.push(handle);
        }

        let identity_changed = entry.app_id != app_id || entry.title != title;
        let states_changed = entry.states != states;
        let outputs_changed = entry.outputs != outputs;

        // Existing handles get only what actually changed; brand-new ones get
        // the full picture regardless.
        for handle in entry.handles.iter().chain(fresh.iter()) {
            let is_fresh = fresh.iter().any(|h| h == handle);
            let mut dirty = is_fresh;

            if is_fresh || identity_changed {
                if let Some(app_id) = &app_id {
                    handle.app_id(app_id.clone());
                }
                if let Some(title) = &title {
                    handle.title(title.clone());
                }
                dirty = true;
            }
            if is_fresh || outputs_changed {
                let Some(client) = handle.client() else { continue };
                // wl_output is per-client, so each handle's events must carry
                // that client's own bindings of the output.
                if !is_fresh {
                    for output in entry.outputs.iter().filter(|o| !outputs.contains(o)) {
                        for wl_output in output.client_outputs(&client) {
                            handle.output_leave(&wl_output);
                        }
                    }
                }
                for output in outputs.iter().filter(|o| is_fresh || !entry.outputs.contains(o)) {
                    for wl_output in output.client_outputs(&client) {
                        handle.output_enter(&wl_output);
                    }
                }
                dirty = true;
            }
            if is_fresh || states_changed {
                handle.state(
                    states
                        .iter()
                        .flat_map(|s| (*s as u32).to_ne_bytes())
                        .collect(),
                );
                dirty = true;
            }

            if dirty {
                handle.done();
            }
        }

        entry.handles.extend(fresh);
        entry.app_id = app_id;
        entry.title = title;
        entry.states = states;
        entry.outputs = outputs;
    }

    state.foreign_toplevel = ft;
}
