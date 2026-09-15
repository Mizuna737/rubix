//! idle-inhibit-unstable-v1 + ext-idle-notify-v1 handlers.
//!
//! Both protocols' `Dispatch2` plumbing is wired automatically by the single
//! `delegate_dispatch2!(RubixState)` call in `handlers/mod.rs` -- this module
//! only needs the two handler traits Smithay's state machinery calls into.
//!
//! Inhibitor lifecycle: Smithay's `IdleInhibitorState::Dispatch2` impl calls
//! `IdleInhibitHandler::uninhibit` only on an explicit `zwp_idle_inhibitor_v1
//! .destroy` request -- NOT when the client crashes or otherwise disconnects
//! without sending it. A client that dies mid-inhibit would pin the screen on
//! forever, which on an OLED panel is the exact failure this feature exists
//! to prevent. The fix lives in `handlers/compositor.rs`'s
//! `CompositorHandler::destroyed` override: `wl_surface` destruction is a
//! hard wayland-server guarantee on every teardown path (explicit destroy,
//! and every surface belonging to a client that disconnects/crashes), so
//! purging `idle_inhibitors` by surface identity there covers both cases
//! symmetrically -- see that file for the actual hook.

use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Resource;
use smithay::wayland::idle_inhibit::IdleInhibitHandler;
use smithay::wayland::idle_notify::{IdleNotifierHandler, IdleNotifierState};

use crate::RubixState;

impl IdleInhibitHandler for RubixState {
    fn inhibit(&mut self, surface: WlSurface) {
        self.idle_inhibitors.insert(surface.clone());
        // Idle-inhibit state was previously undiagnosable: nothing logged
        // when a client took or released a lock, so "why won't the screen
        // blank" had no answer short of reading source. Client pid and
        // surface id only -- NEVER log window titles here (or anywhere else
        // idle state is reported): titles carry document names and message
        // content, and this path is reachable from an ordinary idle-inhibit
        // request, not something the user opted into sharing.
        let client_pid = surface
            .client()
            .and_then(|c| c.get_credentials(&self.display_handle).ok())
            .map(|cred| cred.pid);
        tracing::info!(
            "idle inhibitor taken by client pid={client_pid:?} surface={:?} ({} total)",
            surface.id(),
            self.idle_inhibitors.len(),
        );
        self.sync_idle_inhibited();
    }

    fn uninhibit(&mut self, surface: WlSurface) {
        self.idle_inhibitors.remove(&surface);
        let client_pid = surface
            .client()
            .and_then(|c| c.get_credentials(&self.display_handle).ok())
            .map(|cred| cred.pid);
        tracing::info!(
            "idle inhibitor released by client pid={client_pid:?} surface={:?} ({} total)",
            surface.id(),
            self.idle_inhibitors.len(),
        );
        self.sync_idle_inhibited();
    }
}

impl IdleNotifierHandler for RubixState {
    fn idle_notifier_state(&mut self) -> &mut IdleNotifierState<Self> {
        &mut self.idle_notifier_state
    }
}
