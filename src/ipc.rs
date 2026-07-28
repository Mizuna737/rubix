//! External IPC over a Unix-domain socket: newline-delimited JSON requests in,
//! newline-delimited JSON replies out, plus a coalesced push stream for
//! subscribers. This is the introspection layer a cube-aware status bar (or
//! `socat`/`jq`/a shell script) uses to read the compositor's column/group
//! structure and to inject `NavAction`s -- waybar/i3 workspace models can't
//! represent Rubix's cube, so a bar has to read it from here instead.
//!
//! Best-effort throughout, mirroring `init_xwayland`'s posture: IPC is a
//! convenience layer, never a hard dependency. A failed bind/accept/parse/write
//! logs and drops the offending client (or the whole feature); it never panics
//! or kills the event loop. All serde/protocol code lives in this module --
//! `model/grid.rs` only exposes plain-data accessors.

use std::{
    cell::{Cell, RefCell},
    io::{ErrorKind, Read, Write},
    os::unix::{
        io::{AsFd, BorrowedFd},
        net::{UnixListener, UnixStream},
    },
    rc::Rc,
};

use serde::{Deserialize, Serialize};
use smithay::reexports::calloop::{generic::Generic, EventLoop, Interest, Mode, PostAction};
use smithay::wayland::seat::WaylandFocus;

use crate::{input::NavAction, state::RubixState, CalloopData};

/// One subscriber's write-side handle, kept in the shared registry so the
/// run-loop's coalesced broadcast (see `broadcast_snapshot`) can reach every
/// subscribed client without going through calloop's per-source callback.
pub(crate) struct ClientEntry {
    id: u64,
    write_stream: UnixStream,
    subscribed: Rc<Cell<bool>>,
}

/// Shared client registry: accept-loop inserts, per-client read callbacks
/// remove-on-EOF, broadcast walks it once per dirty cycle.
pub(crate) type ClientRegistry = Rc<RefCell<Vec<ClientEntry>>>;

/// Per-connection read-side state, owned directly by its calloop `Generic`
/// source (so the callback gets `&mut ClientIo` via `NoIoDrop::get_mut`,
/// mirroring the `Display` source in `state.rs::init_wayland_listener`).
struct ClientIo {
    id: u64,
    stream: UnixStream,
    buf: Vec<u8>,
    subscribed: Rc<Cell<bool>>,
    registry: ClientRegistry,
}

impl AsFd for ClientIo {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.stream.as_fd()
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Request {
    GetState,
    Action { action: NavAction },
    Subscribe,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Reply {
    State(StateSnapshot),
    Ok,
    Error { message: String },
}

#[derive(Serialize)]
struct StateSnapshot {
    visible_columns: usize,
    active_column: usize,
    columns: Vec<ColumnView>,
}

#[derive(Serialize)]
struct ColumnView {
    active_row: usize,
    groups: Vec<GroupView>,
}

#[derive(Serialize)]
struct GroupView {
    windows: Vec<WindowView>,
}

#[derive(Serialize)]
struct WindowView {
    id: u32,
    app_id: Option<String>,
    title: Option<String>,
    focused: bool,
}

/// Assemble a snapshot from `state.monitor`'s read-only accessors plus
/// `state.windows` for app_id/title. MVP: app_id/title extraction only covers
/// the cheap cases (wayland xdg toplevel surface data); anything else is left
/// `None` rather than blocking the feature on perfect title extraction.
/// The window id holding real seat keyboard focus, mapped from the focused
/// wl_surface. Unlike `monitor.active_window()` (which always resolves to a
/// group's first leaf), this reflects actual focus -- including mouse
/// click-to-focus onto a non-first window in a group -- so the bar highlights
/// the window the user is really typing into.
fn keyboard_focused_id(state: &RubixState) -> Option<u32> {
    let surface = state.seat.get_keyboard()?.current_focus()?;
    state
        .windows
        .iter()
        .find(|(_, window)| window.wl_surface().is_some_and(|s| s.as_ref() == &surface))
        .map(|(id, _)| *id)
}

fn build_snapshot(state: &RubixState) -> StateSnapshot {
    let focused = keyboard_focused_id(state);
    let columns = state
        .monitor
        .columns()
        .iter()
        .map(|column| ColumnView {
            active_row: column.active_row(),
            groups: column
                .groups()
                .iter()
                .map(|group| GroupView {
                    windows: group
                        .window_ids()
                        .into_iter()
                        .map(|id| window_view(state, id, focused))
                        .collect(),
                })
                .collect(),
        })
        .collect();

    StateSnapshot {
        visible_columns: state.monitor.visible_columns(),
        active_column: state.monitor.active_column(),
        columns,
    }
}

fn window_view(state: &RubixState, id: u32, focused: Option<u32>) -> WindowView {
    let (app_id, title) = state
        .windows
        .get(&id)
        .and_then(|window| window.toplevel())
        .map(|toplevel| {
            smithay::wayland::compositor::with_states(toplevel.wl_surface(), |states| {
                let attrs = states
                    .data_map
                    .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                    .map(|d| d.lock().unwrap());
                match attrs {
                    Some(attrs) => (attrs.app_id.clone(), attrs.title.clone()),
                    None => (None, None),
                }
            })
        })
        .unwrap_or((None, None));

    WindowView {
        id,
        app_id,
        title,
        focused: focused == Some(id),
    }
}

/// Split complete `\n`-terminated lines out of `buf`, leaving any trailing
/// partial line in place for the next read. JSON requests may arrive split
/// across reads, so nothing is parsed until a full line is available.
fn drain_lines(buf: &mut Vec<u8>) -> Vec<String> {
    let mut lines = Vec::new();
    loop {
        let Some(pos) = buf.iter().position(|&b| b == b'\n') else {
            break;
        };
        let line: Vec<u8> = buf.drain(..=pos).collect();
        let line = &line[..line.len() - 1];
        if let Ok(s) = std::str::from_utf8(line) {
            let s = s.trim_end_matches('\r');
            if !s.is_empty() {
                lines.push(s.to_string());
            }
        }
    }
    lines
}

fn write_reply(stream: &mut UnixStream, reply: &Reply) -> std::io::Result<()> {
    let Ok(mut line) = serde_json::to_string(reply) else {
        return Ok(());
    };
    line.push('\n');
    stream.write_all(line.as_bytes())
}

/// Handle every complete line currently buffered for `client`. Returns
/// `false` if the connection should be dropped (write failure -- read EOF is
/// detected by the caller before this runs).
fn handle_lines(client: &mut ClientIo, data: &mut CalloopData) -> bool {
    for line in drain_lines(&mut client.buf) {
        let reply = match serde_json::from_str::<Request>(&line) {
            Ok(Request::GetState) => Reply::State(build_snapshot(&data.state)),
            Ok(Request::Action { action }) => {
                data.state.dispatch_nav(action);
                Reply::Ok
            }
            Ok(Request::Subscribe) => {
                client.subscribed.set(true);
                Reply::State(build_snapshot(&data.state))
            }
            Err(e) => Reply::Error { message: e.to_string() },
        };
        if write_reply(&mut client.stream, &reply).is_err() {
            return false;
        }
    }
    true
}

/// Broadcast one freshly-built snapshot to every subscribed client. Called
/// once per dispatch cycle from `main.rs`'s `event_loop.run` closure, only
/// when `RubixState::ipc_dirty` was set since the last cycle -- this is what
/// coalesces a burst of mutations (e.g. several nav chords, or a window
/// map/unmap) into a single push per subscriber.
pub(crate) fn broadcast_snapshot(state: &RubixState, registry: &ClientRegistry) {
    let mut clients = registry.borrow_mut();
    if !clients.iter().any(|c| c.subscribed.get()) {
        return;
    }
    let snapshot = build_snapshot(state);
    let Ok(mut line) = serde_json::to_string(&Reply::State(snapshot)) else {
        return;
    };
    line.push('\n');
    clients.retain_mut(|client| {
        if !client.subscribed.get() {
            return true;
        }
        client.write_stream.write_all(line.as_bytes()).is_ok()
    });
}

/// Read every available byte off `client`'s socket without blocking. Returns
/// `true` on EOF/error (connection should be dropped).
fn read_available(client: &mut ClientIo) -> bool {
    loop {
        let mut chunk = [0u8; 4096];
        match client.stream.read(&mut chunk) {
            Ok(0) => return true,
            Ok(n) => client.buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == ErrorKind::WouldBlock => return false,
            Err(_) => return true,
        }
    }
}

/// Bind the IPC socket and register it (plus every accepted client) with the
/// calloop event loop. Best-effort: any failure (no `XDG_RUNTIME_DIR`, bind,
/// non-blocking, or source registration) logs a warning and returns `None` --
/// callers must treat a `None` registry as "IPC disabled for this run", not
/// panic. Socket path is `$XDG_RUNTIME_DIR/rubix-<xdisplay>.sock` when an
/// XWayland display number is already known, else `rubix.sock` -- at the point
/// `main.rs` calls this (right after backend init, before XWayland has
/// necessarily signaled `Ready`), `xdisplay` is almost always `None`, and
/// that's fine: the socket must exist independent of X11.
pub fn init_ipc(
    event_loop: &EventLoop<'static, CalloopData>,
    xdisplay: Option<u32>,
) -> Option<ClientRegistry> {
    let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") else {
        tracing::warn!("XDG_RUNTIME_DIR not set; IPC socket disabled");
        return None;
    };
    let sock_name = match xdisplay {
        Some(n) => format!("rubix-{n}.sock"),
        None => "rubix.sock".to_string(),
    };
    let path = std::path::Path::new(&runtime_dir).join(&sock_name);

    // A previous crashed run leaves this file behind; bind() fails on
    // EADDRINUSE otherwise. Best-effort unlink -- if it fails, bind() below
    // will and we report that instead.
    let _ = std::fs::remove_file(&path);

    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!("failed to bind IPC socket at {path:?} ({e}); IPC disabled");
            return None;
        }
    };
    if let Err(e) = listener.set_nonblocking(true) {
        tracing::warn!("failed to set IPC socket non-blocking ({e}); IPC disabled");
        return None;
    }

    let registry: ClientRegistry = Rc::new(RefCell::new(Vec::new()));
    let next_id = Rc::new(Cell::new(0u64));

    let loop_handle = event_loop.handle();
    let accept_handle = loop_handle.clone();
    let registry_for_accept = registry.clone();

    let registered = loop_handle.insert_source(
        Generic::new(listener, Interest::READ, Mode::Level),
        move |_readiness, listener, _data: &mut CalloopData| {
            // Safety: we never drop the listener out from under the source.
            let listener = unsafe { listener.get_mut() };
            loop {
                match listener.accept() {
                    Ok((stream, _addr)) => {
                        if let Err(e) = stream.set_nonblocking(true) {
                            tracing::warn!("failed to set IPC client non-blocking ({e}); dropping client");
                            continue;
                        }
                        let write_stream = match stream.try_clone() {
                            Ok(s) => s,
                            Err(e) => {
                                tracing::warn!("failed to clone IPC client stream ({e}); dropping client");
                                continue;
                            }
                        };
                        let id = next_id.get();
                        next_id.set(id + 1);
                        let subscribed = Rc::new(Cell::new(false));
                        registry_for_accept.borrow_mut().push(ClientEntry {
                            id,
                            write_stream,
                            subscribed: subscribed.clone(),
                        });
                        let client_io = ClientIo {
                            id,
                            stream,
                            buf: Vec::new(),
                            subscribed,
                            registry: registry_for_accept.clone(),
                        };
                        let insert = accept_handle.insert_source(
                            Generic::new(client_io, Interest::READ, Mode::Level),
                            move |_readiness, io, data: &mut CalloopData| {
                                // Safety: we never drop the client out from
                                // under the source.
                                let client = unsafe { io.get_mut() };
                                let eof = read_available(client);
                                let ok = handle_lines(client, data);
                                if eof || !ok {
                                    client.registry.borrow_mut().retain(|c| c.id != client.id);
                                    return Ok(PostAction::Remove);
                                }
                                Ok(PostAction::Continue)
                            },
                        );
                        if insert.is_err() {
                            tracing::warn!("failed to register IPC client source; dropping client");
                            registry_for_accept.borrow_mut().retain(|c| c.id != id);
                        }
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
            Ok(PostAction::Continue)
        },
    );

    match registered {
        Ok(_) => {
            tracing::info!("IPC socket listening at {}", path.display());
            Some(registry)
        }
        Err(e) => {
            tracing::warn!("failed to register IPC listener source ({e}); IPC disabled");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_view_shape_serializes() {
        let view = WindowView { id: 7, app_id: Some("foo".into()), title: None, focused: true };
        let json = serde_json::to_string(&view).unwrap();
        assert!(json.contains("\"id\":7"));
        assert!(json.contains("\"focused\":true"));
    }
}
