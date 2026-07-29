use std::borrow::Cow;

use smithay::{
    backend::input::KeyState,
    desktop::Window,
    input::{
        keyboard::{KeyboardTarget, KeysymHandle, ModifiersState},
        Seat,
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{IsAlive, Serial},
    wayland::seat::WaylandFocus,
    xwayland::X11Surface,
};

use crate::RubixState;

/// Seat keyboard-focus target. Splitting wayland vs. X11 is what drives X11
/// input focus: `KeyboardTarget for X11Surface::enter` issues XSetInputFocus /
/// WM_TAKE_FOCUS, and that only runs when the focus target *is* the X11Surface
/// rather than its bare wl_surface. Wayland clients need neither, so they keep
/// the plain-surface path. (`set_activated`, i.e. the `_NET_WM_STATE_FOCUSED`
/// atom, is already driven separately via `Window::set_activated`.)
#[derive(Debug, Clone, PartialEq)]
pub enum KeyboardFocusTarget {
    Wayland(WlSurface),
    X11(X11Surface),
}

impl KeyboardFocusTarget {
    /// Build a focus target from a mapped window: X11 windows focus as their
    /// `X11Surface` (so activation is driven), everything else as their
    /// wl_surface. `None` if the window has no surface yet.
    pub fn from_window(window: &Window) -> Option<Self> {
        if let Some(x11) = window.x11_surface() {
            return Some(Self::X11(x11.clone()));
        }
        window.wl_surface().map(|s| Self::Wayland(s.into_owned()))
    }

    /// The underlying wl_surface (owned), for data-device focus and focus
    /// comparisons. Named distinctly from the `WaylandFocus::wl_surface` trait
    /// method (which returns a `Cow`) to avoid inherent/trait ambiguity.
    pub fn surface(&self) -> Option<WlSurface> {
        match self {
            Self::Wayland(s) => Some(s.clone()),
            // X11Surface::wl_surface is inherent and already returns an owned
            // Option<WlSurface>, unlike the WaylandFocus Cow variants.
            Self::X11(s) => s.wl_surface(),
        }
    }
}

// Required by delegate_seat!/delegate_data_device!: the seat extracts the
// focused client's wl_surface through this trait.
impl WaylandFocus for KeyboardFocusTarget {
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        match self {
            Self::Wayland(s) => Some(Cow::Borrowed(s)),
            Self::X11(s) => s.wl_surface().map(Cow::Owned),
        }
    }
}

impl IsAlive for KeyboardFocusTarget {
    fn alive(&self) -> bool {
        match self {
            Self::Wayland(s) => s.alive(),
            Self::X11(s) => s.alive(),
        }
    }
}

// Delegate every keyboard event to the inner target. The X11 arm is the whole
// point: routing `enter` into `X11Surface` is what sets X11 input focus.
impl KeyboardTarget<RubixState> for KeyboardFocusTarget {
    fn enter(
        &self,
        seat: &Seat<RubixState>,
        data: &mut RubixState,
        keys: Vec<KeysymHandle<'_>>,
        serial: Serial,
    ) {
        match self {
            Self::Wayland(s) => KeyboardTarget::enter(s, seat, data, keys, serial),
            Self::X11(s) => KeyboardTarget::enter(s, seat, data, keys, serial),
        }
    }

    fn leave(&self, seat: &Seat<RubixState>, data: &mut RubixState, serial: Serial) {
        match self {
            Self::Wayland(s) => KeyboardTarget::leave(s, seat, data, serial),
            Self::X11(s) => KeyboardTarget::leave(s, seat, data, serial),
        }
    }

    fn key(
        &self,
        seat: &Seat<RubixState>,
        data: &mut RubixState,
        key: KeysymHandle<'_>,
        state: KeyState,
        serial: Serial,
        time: u32,
    ) {
        match self {
            Self::Wayland(s) => KeyboardTarget::key(s, seat, data, key, state, serial, time),
            Self::X11(s) => KeyboardTarget::key(s, seat, data, key, state, serial, time),
        }
    }

    fn modifiers(
        &self,
        seat: &Seat<RubixState>,
        data: &mut RubixState,
        modifiers: ModifiersState,
        serial: Serial,
    ) {
        match self {
            Self::Wayland(s) => KeyboardTarget::modifiers(s, seat, data, modifiers, serial),
            Self::X11(s) => KeyboardTarget::modifiers(s, seat, data, modifiers, serial),
        }
    }
}
