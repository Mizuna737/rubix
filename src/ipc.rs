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

use crate::{input::NavAction, state::RubixState};

/// One subscriber's write-side handle, kept in the shared registry so the
/// run-loop's coalesced broadcast (see `broadcast_snapshot`) can reach every
/// subscribed client without going through calloop's per-source callback.
pub(crate) struct ClientEntry {
    id: u64,
    write_stream: UnixStream,
    subscribed: Rc<Cell<bool>>,
    // Separate opt-in from `subscribed`: a client can want the coalesced
    // snapshot stream, the chord echo, both, or neither. See
    // `broadcast_chord`'s doc comment for why this is gated at all.
    chords_subscribed: Rc<Cell<bool>>,
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
    chords_subscribed: Rc<Cell<bool>>,
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
    // DPMS-equivalent screen power (see `RubixState::set_screen_power` /
    // `udev::set_screen_power`). `output: None` means every output; `Some`
    // targets one by name. This is what `rubix screen on|off [DP-3]` sends
    // (see main.rs's `handle_screen_subcommand`) -- write the raw line
    // yourself for anything else, e.g. from a shell:
    //   printf '{"type":"set_screen_power","on":false,"output":null}\n' | \
    //     socat - UNIX-CONNECT:"$XDG_RUNTIME_DIR/rubix.sock"
    // Replies `Reply::Ok`. `Reply::State` (from `get_state`/`subscribe`)
    // carries a `screen_off: bool` -- true only once EVERY output is off
    // (`RubixState::all_outputs_off`); see `ScreenStatus` below for
    // per-output detail.
    SetScreenPower { on: bool, output: Option<String> },
    // Per-output power state -- what `rubix screen status` prints. Replies
    // `Reply::ScreenStatus`.
    ScreenStatus,
    // Change the wallpaper without touching the config file. `output: None`
    // sets every output (and clears any per-output overrides, so it cannot
    // visibly miss a monitor). Decoding happens synchronously, so a bad path or
    // an undecodable image comes back as `Reply::Error` rather than silently
    // drawing nothing:
    //   printf '{"type":"set_wallpaper","path":"/path/to/image.avif","output":null}\n' | \
    //     socat - UNIX-CONNECT:"$XDG_RUNTIME_DIR/rubix.sock"
    // The change is not written back to the config, so a reload restores
    // whatever the file says. See src/wallpaper.rs.
    SetWallpaper { path: String, output: Option<String> },
    // Focus one window by id, revealing its slot if it is out of view. Ids come
    // from `Reply::State`, whose per-group window list is the group's tree order
    // (`Group::window_ids`), so a client can address a sweep across a group's
    // windows directly instead of guessing which direction steps between them --
    // the directional `Action` chords only move along one axis, so a group split
    // the other way is unreachable with them:
    //   printf '{"type":"focus_window","id":82}\n' | \
    //     socat - UNIX-CONNECT:"$XDG_RUNTIME_DIR/rubix.sock"
    // Replies `Reply::Ok`, or `Reply::Error` when the id names no window on the
    // active monitor -- a sweep addressing a window that has since closed gets
    // told, rather than silently doing nothing.
    FocusWindow { id: u32 },
    // Opt in to `Reply::Chord` (see `broadcast_chord`). Independent of
    // `Subscribe` -- a listen-mode editor wants chords, not the cube
    // snapshot stream, so the two are separate flags rather than one
    // subscription covering everything.
    SubscribeChords,
    // Flash a glow along one edge (or all four) of the active monitor, e.g.
    // to confirm a gesture:
    //   printf '{"type":"feedback_pulse","edge":"left","intensity":0.8,"duration_ms":400}\n' | \
    //     socat - UNIX-CONNECT:"$XDG_RUNTIME_DIR/rubix.sock"
    // `edge` is left|right|top|bottom|all; intensity is clamped to 0..1 and
    // duration to one second. Replies `Reply::Ok`, or `Reply::Error` for an
    // unknown edge.
    FeedbackPulse { edge: String, intensity: f32, duration_ms: u64 },
    // Tell Rubix the user is active without any input event: restarts the idle
    // screen-off countdown and pings ext-idle-notify listeners exactly like
    // keyboard/pointer input (`RubixState::notify_activity`). Does NOT power a
    // blanked screen back on -- use `set_screen_power` for that. For presence
    // detectors that see the user when no input arrives (`rubix idle poke`
    // sends this):
    //   printf '{"type":"notify_activity"}\n' | \
    //     socat - UNIX-CONNECT:"$XDG_RUNTIME_DIR/rubix.sock"
    // Replies `Reply::Ok`.
    NotifyActivity,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Reply {
    State(StateSnapshot),
    // Struct variant (not a newtype-wrapping-a-Vec): internally-tagged
    // enums (`#[serde(tag = "type")]`) can only merge the tag into a
    // JSON object, and a bare `Vec` serializes as an array -- wrapping it
    // in a named field is what keeps this representable at all.
    ScreenStatus { outputs: Vec<OutputPowerView> },
    Ok,
    // Pushed unsolicited to every subscriber when the config is loaded or
    // reloaded with problems in it (see `broadcast_config_errors`). A bar
    // reading this stream must therefore match on `type` rather than assuming
    // every line is a `state` -- this is the first message that is not one.
    // `ThemeChanged`, below, is the same story with a different trigger.
    ConfigError { messages: Vec<String> },
    // Pushed unsolicited to every subscriber whenever `[theme]` is enabled and
    // the wallpaper theme is (re)solved -- on wallpaper resolve/set, on a
    // slideshow advance, and on any config change that alters the solve
    // inputs (see `RubixState::apply_theme_update`). Carries the same shape
    // `rubix theme` prints on stdout (see `handle_theme_subcommand` in
    // main.rs) and the same JSON written to `[theme] output_path`, so a
    // subscriber and a file-watcher agree on one format. As with
    // `ConfigError`, a bar reading this stream must match on `type`.
    ThemeChanged { theme: serde_json::Value },
    // Pushed unsolicited to clients that sent `subscribe_chords`, one per
    // resolved key press -- see `broadcast_chord` for the modifier gate that
    // keeps this from being a keylogger. `action` is `None` when the chord
    // matched no bind (still useful for listen-mode: it tells the editor the
    // physical chord it just saw, even unbound).
    Chord { chord: String, matched: bool, action: Option<NavAction> },
    Error { message: String },
}

#[derive(Serialize)]
struct StateSnapshot {
    visible_columns: usize,
    active_column: usize,
    columns: Vec<ColumnView>,
    /// True only once EVERY output is off -- see `Request::SetScreenPower`'s
    /// doc comment. A bar wanting per-output detail should use
    /// `Request::ScreenStatus` instead.
    screen_off: bool,
    /// Mirrors `RubixState::idle_inhibited`: whether at least one client
    /// currently holds an idle-inhibit lock. Exists so a bar or a debugging
    /// client can tell "no client ever asked for an inhibitor" from "an
    /// inhibitor is held" -- previously undiagnosable state (see
    /// `handlers/idle.rs`'s new logging for the same motivation).
    idle_inhibited: bool,
    /// Count of live idle-inhibit locks. Distinguishes "one client is
    /// inhibiting" from "several are" -- the latter matters when diagnosing
    /// why releasing one client's lock (e.g. closing a video call) didn't
    /// re-arm the idle timer: another inhibitor is still held.
    idle_inhibitors: usize,
    /// Whether any window is currently fullscreen (`RubixState::fullscreen_windows`
    /// is non-empty). Exists so an external resource guard can free VRAM
    /// *before* a game exhausts it rather than reacting to a 95%-full card --
    /// see the 2026-09-04 postmortem. A bar can also use it to hide chrome.
    fullscreen_active: bool,
}

#[derive(Serialize)]
struct OutputPowerView {
    name: String,
    on: bool,
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

/// Assemble a snapshot from the active monitor's read-only accessors plus
/// `state.windows` for app_id/title. MVP: app_id/title extraction only covers
/// the cheap cases (wayland xdg toplevel surface data); anything else is left
/// `None` rather than blocking the feature on perfect title extraction.
/// The window id holding real seat keyboard focus, mapped from the focused
/// wl_surface. Unlike `monitor.active_window()` (which always resolves to a
/// group's first leaf), this reflects actual focus -- including mouse
/// click-to-focus onto a non-first window in a group -- so the bar highlights
/// the window the user is really typing into.
fn keyboard_focused_id(state: &RubixState) -> Option<u32> {
    let surface = state.seat.get_keyboard()?.current_focus()?.surface()?;
    state
        .windows
        .iter()
        .find(|(_, window)| window.wl_surface().is_some_and(|s| s.as_ref() == &surface))
        .map(|(id, _)| *id)
}

fn build_snapshot(state: &RubixState) -> StateSnapshot {
    let focused = keyboard_focused_id(state);
    let Some(monitor) = state.workspace.active_monitor() else {
        // No active monitor (no output bound yet) -- empty snapshot rather
        // than panicking.
        return StateSnapshot {
            visible_columns: 0,
            active_column: 0,
            columns: Vec::new(),
            screen_off: state.all_outputs_off(),
            idle_inhibited: false,
            idle_inhibitors: 0,
            fullscreen_active: !state.fullscreen_windows.is_empty(),
        };
    };
    let columns = monitor
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
        visible_columns: monitor.visible_columns(),
        active_column: monitor.active_column(),
        columns,
        screen_off: state.all_outputs_off(),
        idle_inhibited: state.idle_inhibited,
        idle_inhibitors: state.idle_inhibitors.len(),
        fullscreen_active: !state.fullscreen_windows.is_empty(),
    }
}

fn window_view(state: &RubixState, id: u32, focused: Option<u32>) -> WindowView {
    let (app_id, title) = state.window_identity(id);

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
fn handle_lines(client: &mut ClientIo, data: &mut RubixState) -> bool {
    for line in drain_lines(&mut client.buf) {
        let reply = match serde_json::from_str::<Request>(&line) {
            Ok(Request::GetState) => Reply::State(build_snapshot(data)),
            Ok(Request::Action { action }) => {
                data.dispatch_nav(action);
                Reply::Ok
            }
            Ok(Request::FocusWindow { id }) => {
                if data.focus_window_by_id(id) {
                    Reply::Ok
                } else {
                    Reply::Error { message: format!("no window {id} on the active monitor") }
                }
            }
            Ok(Request::Subscribe) => {
                client.subscribed.set(true);
                Reply::State(build_snapshot(data))
            }
            Ok(Request::SubscribeChords) => {
                client.chords_subscribed.set(true);
                Reply::Ok
            }
            Ok(Request::SetScreenPower { on, output }) => {
                data.set_screen_power(on, output.as_deref());
                Reply::Ok
            }
            Ok(Request::NotifyActivity) => {
                data.notify_activity();
                Reply::Ok
            }
            Ok(Request::ScreenStatus) => Reply::ScreenStatus {
                outputs: data
                    .output_power_status()
                    .into_iter()
                    .map(|(name, off)| OutputPowerView { name, on: !off })
                    .collect(),
            },
            Ok(Request::SetWallpaper { path, output }) => {
                match data.wallpaper.set(output.as_deref(), std::path::Path::new(&path)) {
                    Ok(()) => {
                        // Nothing else marks the output damaged: the wallpaper
                        // is not a client surface, so no commit arrives to
                        // trigger a repaint on its own.
                        data.nudge_render();
                        Reply::Ok
                    }
                    Err(message) => Reply::Error { message },
                }
            }
            Ok(Request::FeedbackPulse { edge, intensity, duration_ms }) => {
                match crate::state::PulseEdge::parse(&edge) {
                    None => Reply::Error {
                        message: format!(
                            "unknown edge {edge:?}; valid values: {}",
                            crate::state::PulseEdge::VALID
                        ),
                    },
                    Some(edge) => match data.active_monitor_output() {
                        None => Reply::Error { message: "no active output".into() },
                        Some(output) => {
                            data.feedback_pulse = Some(crate::state::FeedbackPulse::new(
                                output.name(),
                                edge,
                                intensity,
                                std::time::Duration::from_millis(duration_ms),
                                std::time::Instant::now(),
                            ));
                            data.nudge_render();
                            Reply::Ok
                        }
                    },
                }
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

/// Push the freshly solved theme to every subscriber, out of band from the
/// snapshot stream -- same posture as `broadcast_config_errors`: a discrete
/// event tied to one wallpaper/config change, not coalesced.
pub(crate) fn broadcast_theme_changed(registry: &ClientRegistry, theme: &serde_json::Value) {
    let mut clients = registry.borrow_mut();
    if !clients.iter().any(|c| c.subscribed.get()) {
        return;
    }
    let reply = Reply::ThemeChanged { theme: theme.clone() };
    let Ok(mut line) = serde_json::to_string(&reply) else {
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

/// Push one resolved chord to every `subscribe_chords` client, out of band
/// from the snapshot stream -- same shape as `broadcast_theme_changed`. Only
/// called for chords that already passed the modifier gate in
/// `input.rs::process_input_event` (Super/Control/Alt present); this function
/// does not re-check that, so every caller of this function is a keylogger
/// surface if that gate is ever weakened -- see the comment at the call site.
pub(crate) fn broadcast_chord(registry: &ClientRegistry, chord: &str, action: Option<&NavAction>) {
    let mut clients = registry.borrow_mut();
    if !clients.iter().any(|c| c.chords_subscribed.get()) {
        return;
    }
    let reply = Reply::Chord { chord: chord.to_string(), matched: action.is_some(), action: action.cloned() };
    let Ok(mut line) = serde_json::to_string(&reply) else {
        return;
    };
    line.push('\n');
    clients.retain_mut(|client| {
        if !client.chords_subscribed.get() {
            return true;
        }
        client.write_stream.write_all(line.as_bytes()).is_ok()
    });
}

/// Push config problems to every subscriber, out of band from the snapshot
/// stream. Unlike `broadcast_snapshot` this is not coalesced: config errors are
/// discrete events tied to a specific edit, and collapsing two edits' problems
/// into one message would misattribute them.
pub(crate) fn broadcast_config_errors(registry: &ClientRegistry, messages: &[String]) {
    if messages.is_empty() {
        return;
    }
    let mut clients = registry.borrow_mut();
    if !clients.iter().any(|c| c.subscribed.get()) {
        return;
    }
    let reply = Reply::ConfigError { messages: messages.to_vec() };
    let Ok(mut line) = serde_json::to_string(&reply) else {
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
    event_loop: &EventLoop<'static, RubixState>,
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

    // A stale file from a crashed run leaves this path behind; bind() fails
    // on EADDRINUSE otherwise. But the path can *also* belong to a live
    // instance -- unlinking unconditionally steals its socket out from under
    // it (observed: the status bar sat disconnected for ~24h after a second
    // `rubix` launch). Probe with a connect first: success means someone is
    // listening, so leave the file alone and disable IPC for this run
    // instead of clobbering the running instance's.
    if UnixStream::connect(&path).is_ok() {
        tracing::warn!("IPC socket at {path:?} is already live (another rubix instance); IPC disabled for this run");
        return None;
    }
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
        move |_readiness, listener, _data: &mut RubixState| {
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
                        let chords_subscribed = Rc::new(Cell::new(false));
                        registry_for_accept.borrow_mut().push(ClientEntry {
                            id,
                            write_stream,
                            subscribed: subscribed.clone(),
                            chords_subscribed: chords_subscribed.clone(),
                        });
                        let client_io = ClientIo {
                            id,
                            stream,
                            buf: Vec::new(),
                            subscribed,
                            chords_subscribed,
                            registry: registry_for_accept.clone(),
                        };
                        let insert = accept_handle.insert_source(
                            Generic::new(client_io, Interest::READ, Mode::Level),
                            move |_readiness, io, data: &mut RubixState| {
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

    #[test]
    fn feedback_pulse_request_deserializes() {
        let line = r#"{"type":"feedback_pulse","edge":"left","intensity":0.5,"duration_ms":300}"#;
        match serde_json::from_str::<Request>(line).unwrap() {
            Request::FeedbackPulse { edge, intensity, duration_ms } => {
                assert_eq!(edge, "left");
                assert_eq!(intensity, 0.5);
                assert_eq!(duration_ms, 300);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn notify_activity_request_deserializes() {
        let request = serde_json::from_str::<Request>(r#"{"type":"notify_activity"}"#).unwrap();
        assert!(matches!(request, Request::NotifyActivity));
    }
}
