//! Server-side decoration negotiation (HDR Phase 5a).
//!
//! Rubix is a tiling compositor: it owns window geometry outright, so a
//! client-drawn titlebar is wasted space and a second, disagreeing source of
//! truth about where a window starts. Both handlers here answer every client
//! the same way -- `ServerSide` -- regardless of what the client asked for.
//! That is explicitly the compositor's call in both protocols; a client may
//! state a preference but does not get a veto.
//!
//! "Server side" currently means *no* decoration at all: we draw no titlebar
//! and no border yet. The visible effect is that titlebars disappear. Border
//! rendering (Phase 5b) hangs off the same negotiation without needing clients
//! to be told anything new.
//!
//! Two protocols because toolkits are split on which one they speak:
//! `xdg-decoration` is the standard (GTK's Wayland backend, SDL, Firefox,
//! wlroots-era apps) and `org_kde_kwin_server_decoration` is the older KDE one
//! that Qt still binds. Answering only one leaves the other half of the
//! desktop drawing its own titlebars.
//!
//! Known limitation, not a bug: GTK4/libadwaita apps are CSD-only. They never
//! bind either protocol and their headerbars are part of the application's own
//! layout, so no compositor can remove them.

use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;
use smithay::reexports::wayland_protocols_misc::server_decoration::server::{
    org_kde_kwin_server_decoration::{Mode as KdeMode, OrgKdeKwinServerDecoration},
    org_kde_kwin_server_decoration_manager::Mode as KdeDefaultMode,
};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::WEnum;
use smithay::wayland::shell::kde::decoration::{KdeDecorationHandler, KdeDecorationState};
use smithay::wayland::shell::xdg::decoration::XdgDecorationHandler;
use smithay::wayland::shell::xdg::ToplevelSurface;

use crate::RubixState;

/// The mode Rubix advertises to every toplevel, in both protocols' spellings.
/// Named rather than inlined at each of the four call sites so the "we always
/// answer the same thing" invariant is one edit, not four.
const RUBIX_XDG_MODE: Mode = Mode::ServerSide;
const RUBIX_KDE_MODE: KdeMode = KdeMode::Server;
pub(crate) const RUBIX_KDE_DEFAULT_MODE: KdeDefaultMode = KdeDefaultMode::Server;

impl RubixState {
    /// Pin a toplevel to server-side decoration.
    ///
    /// Deliberately does NOT force a configure when the initial one is still
    /// pending: `get_toplevel_decoration` normally arrives before the client's
    /// first commit, and the initial configure that commit triggers will carry
    /// the mode along with the geometry. Sending one here instead would emit a
    /// configure before the client has committed at all. Once the surface is
    /// live, `send_pending_configure` is a no-op unless something actually
    /// changed, so the later calls are cheap.
    fn set_server_side_decoration(&mut self, toplevel: &ToplevelSurface) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(RUBIX_XDG_MODE);
        });
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }
}

impl XdgDecorationHandler for RubixState {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        self.set_server_side_decoration(&toplevel);
    }

    /// The client's stated preference is recorded by the protocol but not
    /// honoured -- a tiling compositor that let clients opt into drawing their
    /// own titlebars would have windows whose usable area disagreed with their
    /// tile. Answering with our mode is the protocol-sanctioned response to a
    /// request we decline.
    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: Mode) {
        self.set_server_side_decoration(&toplevel);
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        self.set_server_side_decoration(&toplevel);
    }
}

impl KdeDecorationHandler for RubixState {
    fn kde_decoration_state(&self) -> &KdeDecorationState {
        &self.kde_decoration_state
    }

    /// Overrides the trait default, which echoes back whatever mode the client
    /// asked for. Rubix answers `Server` unconditionally, matching the
    /// xdg-decoration side.
    fn request_mode(
        &mut self,
        _surface: &WlSurface,
        decoration: &OrgKdeKwinServerDecoration,
        _mode: WEnum<KdeMode>,
    ) {
        decoration.mode(RUBIX_KDE_MODE);
    }
}
