use crate::RubixState;
use smithay::{
    wayland::pointer_constraints::PointerConstraintsHandler,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    input::pointer::PointerHandle,
    utils::{Logical, Point},
};

impl PointerConstraintsHandler for RubixState {
    fn new_constraint(&mut self, _surface: &WlSurface, _pointer: &PointerHandle<Self>) {
        // A pointer constraint (lock or confine) was created.
        // We don't need to track this explicitly here; Smithay's constraint state
        // is stored per-surface and can be queried via `with_pointer_constraint`.
    }

    fn remove_constraint(&mut self, _surface: &WlSurface, _pointer: &PointerHandle<Self>) {
        // A pointer constraint was removed.
        // Pointer motion should return to normal clamped/absolute behavior.
    }

    fn cursor_position_hint(
        &mut self,
        _surface: &WlSurface,
        _pointer: &PointerHandle<Self>,
        _location: Point<f64, Logical>,
    ) {
        // Client provided a cursor position hint (only for locked pointers).
        // This is informational; we use it if rendering a client-provided cursor,
        // but for this phase we don't need to act on it.
    }
}
