mod compositor;
mod dmabuf;
mod layer_shell;
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
use smithay::wayland::selection::data_device::{
    set_data_device_focus, DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler,
};
use smithay::wayland::selection::wlr_data_control::{DataControlHandler, DataControlState};
use smithay::wayland::selection::SelectionHandler;

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
}

impl DataDeviceHandler for RubixState {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}

impl WaylandDndGrabHandler for RubixState {}

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
