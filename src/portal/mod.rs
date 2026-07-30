//! In-process `org.freedesktop.impl.portal.ScreenCast` backend (mutter-style:
//! served directly by the compositor process, not a separate binary).
//!
//! Milestone 1 (this module, as it stands): the D-Bus spine only. No PipeWire,
//! no frame capture. `xdg-desktop-portal-wlr` remains the live ScreenCast
//! backend throughout -- nothing in `~/.config/xdg-desktop-portal` routes to
//! us yet, so this can misbehave without affecting real screenshare. Gated by
//! `RUBIX_PORTAL` (default on; `RUBIX_PORTAL=0` disables it) for exactly that
//! reason.
//!
//! The interesting part is the cross-thread bridge: zbus's connection runs on
//! a dedicated `std::thread` driving its own `async-io` executor (Rubix's
//! main loop is single-threaded calloop with no async runtime, and
//! `RubixState` is not `Send`). The zbus thread never touches `RubixState`;
//! it sends a `PortalRequest` over a `calloop::channel` back onto the loop
//! thread and awaits the reply on an `async_channel`. `screencast::init_portal`
//! wires the receiving end into the event loop next to `init_ipc`; later
//! milestones (chooser UI, PipeWire producer) grow `PortalRequest` rather than
//! replacing this plumbing.

mod capture;
mod pipewire_stream;
pub mod screencast;

pub use screencast::init_portal;
