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

use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Resource;
use smithay::wayland::output::OutputHandler;
use smithay::wayland::selection::data_device::{
    set_data_device_focus, ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
};
use smithay::wayland::selection::SelectionHandler;
use smithay::{delegate_data_device, delegate_output, delegate_seat};

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

delegate_seat!(RubixState);

//
// Wl Data Device
//

impl SelectionHandler for RubixState {
    type SelectionUserData = ();
}

impl DataDeviceHandler for RubixState {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for RubixState {}
impl ServerDndGrabHandler for RubixState {}

delegate_data_device!(RubixState);

//
// Wl Output & Xdg Output
//

impl OutputHandler for RubixState {}
delegate_output!(RubixState);
