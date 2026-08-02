use crate::{
    grabs::resize_grab,
    model::{geometry::Rect, tiling::SplitDirection},
    state::ClientState,
    RubixState,
};
use smithay::{
    backend::renderer::utils::{on_commit_buffer_handler, with_renderer_surface_state},
    reexports::wayland_server::{
        protocol::{wl_buffer, wl_surface::WlSurface},
        Client,
    },
    wayland::{
        buffer::BufferHandler,
        compositor::{
            get_parent, is_sync_subsurface, CompositorClientState, CompositorHandler, CompositorState,
        },
        shm::{ShmHandler, ShmState},
    },
    xwayland::XWaylandClientData,
};

use super::{layer_shell, xdg_shell};

impl CompositorHandler for RubixState {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        // XWayland's client is created by smithay's own machinery and carries
        // `XWaylandClientData`, not our `ClientState`, so check it first. All
        // other clients connect through the listening socket with `ClientState`.
        if let Some(state) = client.get_data::<XWaylandClientData>() {
            return &state.compositor_state;
        }
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }

            // First buffer commit for a staged (unmapped) toplevel: this is the
            // point a client actually "maps" -- promote it into the model now,
            // running exactly what `new_toplevel` used to do eagerly at creation
            // time. A client that creates a toplevel and never attaches a buffer
            // (e.g. a headless clipboard reader grabbing the selection) never
            // reaches this path and leaves no trace.
            let unmapped_id = self
                .unmapped
                .iter()
                .find(|(_, w)| w.toplevel().is_some_and(|t| t.wl_surface() == &root))
                .map(|(id, _)| *id);
            if let Some(id) = unmapped_id {
                let has_buffer =
                    with_renderer_surface_state(&root, |s| s.buffer().is_some()).unwrap_or(false);
                if has_buffer {
                    let window = self.unmapped.remove(&id).unwrap();
                    self.windows.insert(id, window);

                    let focused_id = self.focused_window_id();
                    let direction = focused_id
                        .and_then(|fid| self.window_rect(fid))
                        .map(Rect::longer_axis)
                        .unwrap_or(SplitDirection::Horizontal);
                    if let Some(monitor) = self.workspace.active_monitor_mut() {
                        monitor.add_window(direction, id, focused_id.unwrap_or(0));
                    }
                    self.apply_layout();
                    // Focus follows spawn: name the new id directly. The model is
                    // focus-agnostic, so focus_active_window would re-derive the
                    // group's top leaf and never land on the window we just mapped.
                    self.focus_by_id(id);
                    self.ipc_dirty = true;
                    tracing::info!(
                        "toplevel mapped -> window {id} ({} tracked, {} mapped in space)",
                        self.windows.len(),
                        self.space.elements().count(),
                    );
                }
            }

            if let Some(window) = self
                .space
                .elements()
                .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == &root))
            {
                window.on_commit();
            }
        };

        xdg_shell::handle_commit(&mut self.popups, &self.space, surface);
        if layer_shell::handle_commit(&self.space, surface) {
            // A bar committed; if it changed the reserved area (exclusive zone),
            // reflow tiled windows into the new bounds -- once per change, not
            // every repaint frame.
            // reserved_bounds tracks only the active monitor's exclusive zone
            // for now -- a bar reflow on a non-active monitor is a known
            // multi-monitor follow-up, not addressed in this stage.
            let bounds = self.workspace.active_monitor().and_then(|m| self.output_bounds_for(m.id));
            if bounds != self.reserved_bounds {
                self.reserved_bounds = bounds;
                self.apply_layout();
            }
            // A layer surface committed; if it asked for keyboard interactivity
            // (a launcher like rofi), route focus to it now that it's mapped.
            self.focus_interactive_layer(surface);
        }
        resize_grab::handle_commit(&mut self.space, surface);
    }
}

impl BufferHandler for RubixState {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for RubixState {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

// `delegate_compositor!`/`delegate_shm!` (and every other per-protocol
// `delegate_*!` macro) were removed by the HDR fork's dispatch2 rework; all
// protocols now go through a single `delegate_dispatch2!(RubixState)` call
// in `handlers/mod.rs`.
