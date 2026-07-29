use smithay::{
    delegate_layer_shell,
    desktop::{layer_map_for_output, LayerSurface, Space, Window, WindowSurfaceType},
    output::Output,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    reexports::wayland_server::protocol::wl_output::WlOutput,
    utils::SERIAL_COUNTER,
    wayland::compositor::with_states,
    wayland::shell::wlr_layer::{
        Layer, LayerSurface as WlrLayerSurface, LayerSurfaceData, WlrLayerShellHandler, WlrLayerShellState,
    },
};

use crate::focus::KeyboardFocusTarget;
use crate::RubixState;

impl WlrLayerShellHandler for RubixState {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: WlrLayerSurface,
        wl_output: Option<WlOutput>,
        _layer: Layer,
        namespace: String,
    ) {
        // Resolve the target output: the client's requested output if given and
        // still mappable, else the first (only, today) output known to the
        // space. No output yet (e.g. a layer client racing compositor startup
        // before winit/udev has mapped one) -- skip-map, never unwrap. The
        // surface stays unmapped; there is no protocol requirement to map it.
        let output = wl_output
            .as_ref()
            .and_then(Output::from_resource)
            .or_else(|| self.space.outputs().next().cloned());

        let Some(output) = output else {
            tracing::debug!("new layer surface ({namespace}) with no output available; skipping map");
            return;
        };

        let mut map = layer_map_for_output(&output);
        if let Err(e) = map.map_layer(&LayerSurface::new(surface, namespace)) {
            tracing::warn!("failed to map layer surface: {e}");
        }
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        // If the dismissed layer (e.g. rofi) currently holds keyboard focus,
        // hand focus back to the model's active window rather than leaving it
        // stranded on a dead surface. Captured before the unmap below.
        let had_focus = self
            .seat
            .get_keyboard()
            .and_then(|k| k.current_focus())
            .and_then(|t| t.surface())
            .as_ref()
            == Some(surface.wl_surface());

        // Find the output whose layer map contains this surface, unmap it, and
        // re-arrange so the remaining layers reclaim the space.
        let mut arranged = false;
        for output in self.space.outputs().cloned().collect::<Vec<_>>() {
            let mut map = layer_map_for_output(&output);
            let found = map
                .layers()
                .find(|l| l.wl_surface() == surface.wl_surface())
                .cloned();
            if let Some(layer) = found {
                map.unmap_layer(&layer);
                map.arrange();
                arranged = true;
            }
            // Drop the layer-map borrow before output_bounds()/apply_layout,
            // which re-borrow the same RefCell.
            drop(map);
            if arranged {
                break;
            }
        }
        // A bar went away; reflow tiled windows back into the reclaimed area.
        if arranged {
            let bounds = self.output_bounds();
            if bounds != self.reserved_bounds {
                self.reserved_bounds = bounds;
                self.apply_layout();
            }
        }
        if had_focus {
            self.focus_active_window();
        }
    }
}

impl RubixState {
    /// If the just-committed layer surface requested keyboard interactivity
    /// (Exclusive/OnDemand -- e.g. a wayland-native rofi), route keyboard focus
    /// to it so it actually receives keystrokes. Without this a launcher maps
    /// and renders but eats nothing. Idempotent: a no-op once the surface
    /// already holds focus, so the per-commit call cost stays flat, and nav
    /// chords keep working because the input filter intercepts them regardless
    /// of who holds focus.
    ///
    /// Known gap (fine to refine later): this focuses *any* interactive layer,
    /// so a second interactive layer mapping over the first would steal focus;
    /// today rofi is the only such client.
    pub(crate) fn focus_interactive_layer(&mut self, surface: &WlSurface) {
        let wants = self.space.outputs().any(|output| {
            layer_map_for_output(output)
                .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
                .map(|l| l.can_receive_keyboard_focus())
                .unwrap_or(false)
        });
        if !wants {
            return;
        }
        let keyboard = self
            .seat
            .get_keyboard()
            .expect("keyboard added to seat at startup");
        if keyboard.current_focus().and_then(|t| t.surface()).as_ref() == Some(surface) {
            return;
        }
        let serial = SERIAL_COUNTER.next_serial();
        keyboard.set_focus(self, Some(KeyboardFocusTarget::Wayland(surface.clone())), serial);
    }
}

delegate_layer_shell!(RubixState);

/// Should be called on `WlSurface::commit`, mirroring `xdg_shell::handle_commit`.
/// Phase 1: `arrange()` always runs (it positions AND sizes the surface -- an
/// anchored-to-all-edges client like swaybg won't attach a buffer until it gets
/// a configure carrying a real size, which comes from arrange). `arrange()`
/// itself only auto-sends a configure on *changes after* the initial one (see
/// its doc comment), so the very first configure has to be sent explicitly
/// here once the size has been computed.
/// Returns `true` if `surface` is a layer surface that was (re)arranged -- the
/// caller uses this to reflow tiled windows when a bar's exclusive zone changes.
pub fn handle_commit(space: &Space<Window>, surface: &WlSurface) -> bool {
    for output in space.outputs() {
        let mut map = layer_map_for_output(output);
        let Some(layer) = map
            .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
            .cloned()
        else {
            continue;
        };

        let initial_configure_sent = with_states(surface, |states| {
            states
                .data_map
                .get::<LayerSurfaceData>()
                .map(|d| d.lock().unwrap().initial_configure_sent)
                .unwrap_or(false)
        });

        map.arrange();
        if !initial_configure_sent {
            layer.layer_surface().send_configure();
        }
        return true;
    }
    false
}
