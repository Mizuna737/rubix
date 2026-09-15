mod compositor;
mod decoration;
pub(crate) use decoration::RUBIX_KDE_DEFAULT_MODE;
mod dmabuf;
mod idle;
mod layer_shell;
mod pointer_constraints;
mod xdg_shell;
pub mod xwayland;

use crate::focus::KeyboardFocusTarget;
use crate::RubixState;

//
// Wl Seat
//

use smithay::delegate_dispatch2;
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Resource;
use smithay::wayland::output::OutputHandler;
use smithay::input::dnd::{DnDGrab, GrabType, Source};
use smithay::input::pointer::Focus;
use smithay::utils::Serial;
use smithay::wayland::selection::data_device::{
    set_data_device_focus, DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler,
};
use smithay::wayland::selection::wlr_data_control::{DataControlHandler, DataControlState};
use smithay::wayland::selection::{SelectionHandler, SelectionTarget};

impl SeatHandler for RubixState {
    type KeyboardFocus = KeyboardFocusTarget;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<RubixState> {
        &mut self.seat_state
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: smithay::input::pointer::CursorImageStatus) {
        self.cursor_status = image;
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&KeyboardFocusTarget>) {
        let dh = &self.display_handle;
        let client = focused
            .and_then(|t| t.surface())
            .and_then(|s| dh.get_client(s.id()).ok());
        set_data_device_focus(dh, seat, client);
    }
}

//
// Wl Data Device
//

impl SelectionHandler for RubixState {
    type SelectionUserData = ();

    // A Wayland client just set the clipboard. Forward it into XWM so X11
    // apps see it as their own CLIPBOARD selection -- without this, focusing
    // an X11 window after copying from a Wayland one pastes stale/empty data.
    // Primary selection is unsupported anywhere in Rubix (see
    // `DataControlState::new` in state.rs), so there's nothing to forward for
    // that target.
    fn new_selection(&mut self, ty: SelectionTarget, source: Option<smithay::wayland::selection::SelectionSource>, _seat: Seat<Self>) {
        if ty != SelectionTarget::Clipboard {
            return;
        }
        if let Some(xwm) = &mut self.xwm {
            if let Err(err) = xwm.new_selection(ty, source.map(|s| s.mime_types())) {
                tracing::error!(?err, "failed to forward wayland clipboard selection to XWayland");
            }
        }
    }

    // An X11 client (via XWM's `send_selection`, see handlers/xwayland.rs)
    // wants the data behind a selection Rubix itself set from XWM's side.
    // XWM owns fulfilling that read, so just hand the request through.
    fn send_selection(
        &mut self,
        ty: SelectionTarget,
        mime_type: String,
        fd: std::os::unix::io::OwnedFd,
        _seat: Seat<Self>,
        _user_data: &Self::SelectionUserData,
    ) {
        if ty != SelectionTarget::Clipboard {
            return;
        }
        if let Some(xwm) = &mut self.xwm {
            if let Err(err) = xwm.send_selection(ty, mime_type, fd) {
                tracing::error!(?err, "failed to read XWayland clipboard selection for a wayland client");
            }
        }
    }
}

impl DataDeviceHandler for RubixState {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}

impl WaylandDndGrabHandler for RubixState {
    /// Install the drag grab that actually carries a drag-and-drop session.
    ///
    /// This trait's default body is `source.cancel()` -- a "you must implement
    /// this" stub rather than a working default -- so leaving it underived
    /// rejected every drag the instant it began: clients got
    /// `wl_data_source.cancelled` about a millisecond after `start_drag`, with
    /// a valid implicit grab and no error. That silently broke DnD for every
    /// client (browsers, file managers, `weston-dnd` alike) while leaving
    /// clipboard selection working, since selection takes a different path.
    ///
    /// Setting the grab is what makes the compositor deliver `wl_data_device`
    /// enter/motion/drop to the surface under the cursor for the duration.
    fn dnd_requested<S: Source>(
        &mut self,
        source: S,
        icon: Option<WlSurface>,
        seat: Seat<Self>,
        serial: Serial,
        type_: GrabType,
    ) {
        // Tracked so compose.rs can draw a ghost under the cursor for the
        // drag's duration. Offset starts at zero and accumulates buffer
        // deltas on commit (handlers/compositor.rs) as the client re-attaches.
        self.dnd_icon = icon.map(|surface| crate::state::DndIcon { surface, offset: (0, 0).into() });
        // Cloned up front because `set_grab` takes `&mut self`.
        let dh = self.display_handle.clone();
        match type_ {
            GrabType::Pointer => {
                let Some(pointer) = seat.get_pointer() else { return };
                let Some(start_data) = pointer.grab_start_data() else { return };
                pointer.set_grab(
                    self,
                    DnDGrab::new_pointer(&dh, start_data, source, seat),
                    serial,
                    Focus::Keep,
                );
            }
            GrabType::Touch => {
                let Some(touch) = seat.get_touch() else { return };
                let Some(start_data) = touch.grab_start_data() else { return };
                touch.set_grab(self, DnDGrab::new_touch(&dh, start_data, source, seat), serial);
            }
        }
    }
}

impl DataControlHandler for RubixState {
    fn data_control_state(&mut self) -> &mut DataControlState {
        &mut self.data_control_state
    }
}

//
// Wl Output & Xdg Output
//

impl OutputHandler for RubixState {}

// Every per-protocol `delegate_*!` macro (seat, data device, data control,
// output, compositor, shm, dmabuf, layer shell, xdg shell, xwayland shell)
// was removed by the HDR fork's `Dispatch2`/`GlobalDispatch2` rework in favor
// of one blanket `Dispatch`/`GlobalDispatch` impl per state type. This single
// call is the fork's replacement for all of them.
delegate_dispatch2!(RubixState);
