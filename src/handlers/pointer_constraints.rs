use crate::RubixState;
use smithay::{
    input::pointer::PointerHandle,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point},
    wayland::pointer_constraints::{PointerConstraintsHandler, with_pointer_constraint},
};

impl PointerConstraintsHandler for RubixState {
    /// Arm a freshly created constraint, but only while the pointer is already
    /// over the surface that asked for it.
    ///
    /// Activation is deliberately not automatic in Smithay -- it is the moment
    /// the client is told `locked`/`confined`, and the compositor decides the
    /// policy. Rubix's policy is the conventional one: the pointer must be on
    /// the surface. When it is not, the motion handler activates on entry
    /// instead. Nothing may act on a constraint before this runs, or the cursor
    /// freezes against a client that was never told it had the lock.
    fn new_constraint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>) {
        let Some(current_focus) = pointer.current_focus() else {
            return;
        };
        if &current_focus == surface {
            with_pointer_constraint(surface, pointer, |constraint| {
                if let Some(constraint) = constraint {
                    constraint.activate();
                }
            });
        }
    }

    /// On teardown, put the cursor where the client last said it was drawing it.
    ///
    /// A locked pointer is invisible to the compositor's own position tracking --
    /// the client has been moving a cursor of its own the whole time. Without the
    /// warp, releasing a lock drops the pointer back wherever it was frozen,
    /// which is why leaving a game left the cursor somewhere unrelated to what
    /// the game had been showing.
    fn remove_constraint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>) {
        // Only warp once the last constraint on the surface is gone.
        if !with_pointer_constraint(surface, pointer, |constraint| constraint.is_none()) {
            return;
        }
        let Some((hint_surface, hint_location)) = self.cursor_position_hint.take() else {
            return;
        };
        if &hint_surface != surface {
            return;
        }
        // The hint is surface-local; resolve the surface's current top-left to
        // put it back into global coordinates.
        let Some(origin) = self.window_location(surface) else {
            return;
        };
        self.pointer_location = self.clamp_to_outputs(origin + hint_location);
    }

    fn cursor_position_hint(
        &mut self,
        surface: &WlSurface,
        pointer: &PointerHandle<Self>,
        location: Point<f64, Logical>,
    ) {
        // Hints are only meaningful from a client that actually holds an active
        // lock; anything else is stale by the time the constraint ends.
        if with_pointer_constraint(surface, pointer, |constraint| {
            constraint.is_some_and(|c| c.is_active())
        }) {
            self.cursor_position_hint = Some((surface.clone(), location));
        }
    }
}
