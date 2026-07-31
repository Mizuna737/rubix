//! `org.freedesktop.impl.portal.ScreenCast` backend interface, plus the
//! `Session`/`Request` companion objects the impl-portal contract requires,
//! and the calloop <-> zbus bridge that lets the D-Bus thread ask the
//! compositor's single-threaded loop for plain, `Send` data.
//!
//! M1 scope: every method logs its arguments and returns a well-formed but
//! empty/stub response. `Start` always reports zero streams (no PipeWire
//! producer exists yet -- that's M2+). `SelectSources` is the one method that
//! actually exercises the loop bridge (`PortalRequest::ListSources`), which is
//! the pattern later milestones (chooser, capture) build on.
//!
//! Interface version: we advertise `2` (cursor_mode support only). We don't
//! implement `persist_mode`/`restore_data` (v4), `mapping_id` (v5), or
//! `pipewire-serial` (v6) semantics yet, so advertising those versions would
//! be a lie the frontend could act on.
//!
//! `OpenPipeWireRemote` is, per the current freedesktop XML
//! (`org.freedesktop.impl.portal.ScreenCast.xml`), NOT part of this
//! backend-facing interface -- it lives only on the client-facing
//! `org.freedesktop.portal.ScreenCast` interface, which `xdg-desktop-portal`
//! core handles itself using the node id out of `Start`'s results. We still
//! expose a stub method here (extra methods on our own interface are
//! harmless) so the dispatch shape matches the milestone brief and is ready
//! to wire up if a later milestone needs it; in the real request flow it will
//! never be invoked.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use smithay::reexports::calloop::{self, timer::TimeoutAction, timer::Timer, EventLoop};
use zbus::fdo;
use zbus::interface;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{
    Array, Dict, ObjectPath, OwnedObjectPath, OwnedValue, Signature, Structure, StructureBuilder,
    Value,
};
use zbus::Connection;

use crate::portal::capture::{self, CaptureTarget};
use crate::portal::chooser;
use crate::portal::pipewire_stream::{self, StreamStarted};
use crate::state::RubixState;

/// Capture cadence: how often the loop-thread timer re-renders every active
/// session's target and refreshes its `FrameSlot`. ~30fps, matching
/// `pipewire_stream::FRAMERATE`.
const CAPTURE_INTERVAL: Duration = Duration::from_millis(33);

/// `session_handle -> (what it streams, where the newest frame lands)`.
/// Written by the loop thread (`StartStream` handling inserts, the capture
/// timer updates frames in place); removed by either thread on teardown
/// (`Session.Close` on the zbus thread, or `StartStream` itself if the
/// session raced closed underneath it).
type ActiveCaptures = Arc<Mutex<HashMap<OwnedObjectPath, (CaptureTarget, capture::FrameSlot)>>>;

/// A window or monitor the compositor can offer as a capture source. Plain
/// `Send` data -- this is what crosses the calloop/zbus thread boundary, never
/// `RubixState` itself.
#[derive(Clone, Debug)]
pub struct SourceInfo {
    pub id: u32,
    pub kind: SourceKind,
    pub label: String,
}

#[derive(Clone, Copy, Debug)]
pub enum SourceKind {
    Window,
    Monitor,
}

/// Messages the zbus thread sends onto the calloop loop thread. Grows as
/// later milestones need more compositor-side reads (chooser, PipeWire
/// producer wiring); every variant carries its own reply channel so the
/// zbus-side `async fn` can simply `.await` it.
pub enum PortalRequest {
    ListSources { reply: async_channel::Sender<Vec<SourceInfo>> },
    /// `Start` asking the loop thread to spin up the M2 PipeWire test-pattern
    /// producer for a session. The loop thread only spawns the (self
    /// contained) pipewire thread and stashes its stop handle in the session
    /// registry -- it never touches PipeWire itself. `reply` is resolved
    /// later, directly by the pipewire thread, once the stream has actually
    /// connected and has a node id.
    StartStream {
        session_handle: OwnedObjectPath,
        target: CaptureTarget,
        reply: async_channel::Sender<Result<StreamStarted, String>>,
    },
}

/// Bind `init_portal`'s calloop channel receiver into the event loop. The
/// handler runs on the loop thread, reads `RubixState` directly, and replies over the
/// enclosed `async_channel::Sender` -- the only place `RubixState` data is
/// ever read for this feature.
fn build_source_list(state: &RubixState) -> Vec<SourceInfo> {
    let mut sources = Vec::new();

    for (&id, window) in &state.windows {
        let label = window
            .toplevel()
            .and_then(|toplevel| {
                smithay::wayland::compositor::with_states(toplevel.wl_surface(), |states| {
                    states
                        .data_map
                        .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                        .map(|d| d.lock().unwrap())
                        .and_then(|attrs| attrs.app_id.clone().or_else(|| attrs.title.clone()))
                })
            })
            .unwrap_or_else(|| format!("window-{id}"));
        sources.push(SourceInfo { id, kind: SourceKind::Window, label });
    }

    for (idx, output) in state.space.outputs().enumerate() {
        sources.push(SourceInfo { id: idx as u32, kind: SourceKind::Monitor, label: output.name() });
    }

    sources
}

/// Per-session state recorded by `SelectSources`/`Start`. Keyed by the
/// session object path in the registry below.
#[derive(Default)]
struct SessionState {
    source_types: u32,
    multiple: bool,
    /// Set once `Start` has spun up the M2 test-pattern PipeWire thread for
    /// this session. Sending `()` on it tells that thread to quit its main
    /// loop and tear itself down; `Session.Close`/`Request.Close` use this to
    /// avoid leaking a PipeWire stream/thread per session.
    stream_stop: Option<pipewire::channel::Sender<()>>,
}

type SessionRegistry = Arc<Mutex<HashMap<OwnedObjectPath, SessionState>>>;

/// The `org.freedesktop.impl.portal.ScreenCast` object itself, served at
/// `/org/freedesktop/portal/desktop`.
struct ScreenCastIface {
    /// Wrapped in a `Mutex` purely for `Sync`: `calloop::channel::Sender` is
    /// `Send` (backed by `std::sync::mpsc::Sender`, cloneable across
    /// threads) but not `Sync`, and zbus requires interface types to be
    /// `Send + Sync + 'static` since methods can run concurrently.
    bridge_tx: Mutex<calloop::channel::Sender<PortalRequest>>,
    sessions: SessionRegistry,
    active_captures: ActiveCaptures,
}

#[interface(name = "org.freedesktop.impl.portal.ScreenCast")]
impl ScreenCastIface {
    // Portal interfaces expose the version property lowercase ("version"); zbus
    // would otherwise derive PascalCase "Version", which the frontend can't find
    // -> it falls back to client-facing ScreenCast version 0 (unusable).
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        2
    }

    #[zbus(property, name = "AvailableSourceTypes")]
    fn available_source_types(&self) -> u32 {
        // MONITOR (1) | WINDOW (2)
        0b11
    }

    #[zbus(property, name = "AvailableCursorModes")]
    fn available_cursor_modes(&self) -> u32 {
        // HIDDEN (1) | EMBEDDED (2)
        0b11
    }

    async fn create_session(
        &self,
        handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        app_id: String,
        options: HashMap<String, OwnedValue>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<(u32, HashMap<String, OwnedValue>)> {
        tracing::info!(
            "[portal] CreateSession handle={handle} session_handle={session_handle} app_id={app_id:?} options={options:?}"
        );

        self.sessions.lock().unwrap().insert(session_handle.clone(), SessionState::default());

        let session_obj = SessionObj {
            handle: session_handle.clone(),
            sessions: self.sessions.clone(),
            active_captures: self.active_captures.clone(),
        };
        if let Err(e) = conn.object_server().at(&session_handle, session_obj).await {
            tracing::warn!("[portal] failed to export Session object at {session_handle}: {e}");
        }

        let request_obj = RequestObj { handle: handle.clone() };
        if let Err(e) = conn.object_server().at(&handle, request_obj).await {
            tracing::warn!("[portal] failed to export Request object at {handle}: {e}");
        }

        Ok((0, HashMap::new()))
    }

    async fn select_sources(
        &self,
        handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        app_id: String,
        options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<(u32, HashMap<String, OwnedValue>)> {
        tracing::info!(
            "[portal] SelectSources handle={handle} session_handle={session_handle} app_id={app_id:?} options={options:?}"
        );

        let types: u32 = options
            .get("types")
            .and_then(|v| u32::try_from(v.clone()).ok())
            .unwrap_or(0);
        let multiple: bool = options
            .get("multiple")
            .and_then(|v| bool::try_from(v.clone()).ok())
            .unwrap_or(false);

        if let Some(state) = self.sessions.lock().unwrap().get_mut(&session_handle) {
            state.source_types = types;
            state.multiple = multiple;
        }

        // Prove the loop bridge: ask the compositor thread for its current
        // window/monitor list and log what comes back. This is the exact
        // cross-thread read the chooser (M-later) and capture wiring reuse.
        let (reply_tx, reply_rx) = async_channel::bounded(1);
        let sent = self.bridge_tx.lock().unwrap().send(PortalRequest::ListSources { reply: reply_tx });
        match sent {
            Ok(()) => match reply_rx.recv().await {
                Ok(sources) => tracing::info!("[portal] SelectSources: compositor sources = {sources:?}"),
                Err(e) => tracing::warn!("[portal] SelectSources: loop bridge reply channel closed ({e})"),
            },
            Err(e) => tracing::warn!("[portal] SelectSources: loop bridge send failed ({e})"),
        }

        Ok((0, HashMap::new()))
    }

    async fn start(
        &self,
        handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        app_id: String,
        parent_window: String,
        options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<(u32, HashMap<String, OwnedValue>)> {
        tracing::info!(
            "[portal] Start handle={handle} session_handle={session_handle} app_id={app_id:?} parent_window={parent_window:?} options={options:?}"
        );

        let source_types = match self.sessions.lock().unwrap().get(&session_handle) {
            Some(state) => state.source_types,
            None => {
                tracing::warn!("[portal] Start: unknown session {session_handle}");
                return Ok((2, HashMap::new()));
            }
        };

        // Debug-only bypass of the chooser, e.g. for scripted testing where a
        // rofi prompt can't be driven interactively. Format: "window:<id>" or
        // "monitor:<name>". Unset by default -- the chooser below is the real
        // path a live client goes through.
        let forced_target = std::env::var("RUBIX_PORTAL_FORCE_TARGET").ok().and_then(|raw| {
            let (kind, value) = raw.split_once(':')?;
            match kind {
                "window" => value.parse::<u32>().ok().map(CaptureTarget::Window),
                "monitor" => Some(CaptureTarget::Monitor(value.to_string())),
                _ => None,
            }
        });

        let target = match forced_target {
            Some(target) => {
                tracing::warn!(
                    "[portal] Start: RUBIX_PORTAL_FORCE_TARGET set, bypassing chooser -> {target:?}"
                );
                target
            }
            None => {
                let (reply_tx, reply_rx) = async_channel::bounded(1);
                let sent =
                    self.bridge_tx.lock().unwrap().send(PortalRequest::ListSources { reply: reply_tx });
                if let Err(e) = sent {
                    tracing::warn!("[portal] Start: loop bridge send failed while listing sources ({e})");
                    return Ok((2, HashMap::new()));
                }
                let sources = match reply_rx.recv().await {
                    Ok(sources) => sources,
                    Err(e) => {
                        tracing::warn!("[portal] Start: loop bridge reply channel closed ({e})");
                        return Ok((2, HashMap::new()));
                    }
                };

                match chooser::choose_target(&sources, source_types).await {
                    Some(target) => target,
                    None => {
                        tracing::info!(
                            "[portal] Start: chooser cancelled or unavailable for session {session_handle}"
                        );
                        // 1 == user cancelled, per the impl-portal Request/Session
                        // response-code convention.
                        return Ok((1, HashMap::new()));
                    }
                }
            }
        };

        // WINDOW (2) / MONITOR (1), per org.freedesktop.portal.ScreenCast's
        // SourceType -- reported in the `streams` dict below.
        let source_type_value: u32 = match target {
            CaptureTarget::Window(_) => 2,
            CaptureTarget::Monitor(_) => 1,
        };

        let (reply_tx, reply_rx) = async_channel::bounded(1);
        let sent = self.bridge_tx.lock().unwrap().send(PortalRequest::StartStream {
            session_handle: session_handle.clone(),
            target,
            reply: reply_tx,
        });
        if let Err(e) = sent {
            tracing::warn!("[portal] Start: loop bridge send failed ({e})");
            return Ok((2, HashMap::new()));
        }

        let started = match reply_rx.recv().await {
            Ok(Ok(started)) => started,
            Ok(Err(e)) => {
                tracing::warn!("[portal] Start: pipewire producer failed: {e}");
                return Ok((2, HashMap::new()));
            }
            Err(e) => {
                tracing::warn!("[portal] Start: loop bridge reply channel closed ({e})");
                return Ok((2, HashMap::new()));
            }
        };

        tracing::info!(
            "[portal] Start: pipewire node {} ready ({}x{})",
            started.node_id,
            started.width,
            started.height
        );

        let mut stream_props: HashMap<String, Value> = HashMap::new();
        stream_props.insert(
            "size".to_string(),
            Value::Structure(Structure::from((started.width as i32, started.height as i32))),
        );
        stream_props.insert("source_type".to_string(), Value::U32(source_type_value));

        let stream_dict = Dict::from(stream_props);
        let stream_struct = StructureBuilder::new()
            .append_field(Value::U32(started.node_id))
            .append_field(Value::Dict(stream_dict))
            .build()
            .expect("(u, a{sv}) fields always build a valid structure");
        let stream_entry = Value::Structure(stream_struct);

        let streams_sig = Signature::try_from("(ua{sv})").expect("valid signature literal");
        let mut streams = Array::new(&streams_sig);
        streams.append(stream_entry).expect("stream entry matches the (ua{sv}) signature");
        let streams_value: OwnedValue =
            Value::Array(streams).try_to_owned().expect("array of one valid entry always converts");

        let mut results = HashMap::new();
        results.insert("streams".to_string(), streams_value);
        Ok((0, results))
    }

    /// NOT part of the current `org.freedesktop.impl.portal.ScreenCast`
    /// contract (see module docs) -- kept as an inert stub for parity with
    /// the milestone brief. Real clients never reach this: the client-facing
    /// interface's `OpenPipeWireRemote` is handled entirely by
    /// `xdg-desktop-portal` core.
    async fn open_pipe_wire_remote(
        &self,
        session_handle: OwnedObjectPath,
        options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<zbus::zvariant::OwnedFd> {
        tracing::warn!(
            "[portal] OpenPipeWireRemote session_handle={session_handle} options={options:?}: not implemented (M1)"
        );
        Err(fdo::Error::NotSupported("OpenPipeWireRemote: not implemented (M1)".into()))
    }
}

/// `org.freedesktop.impl.portal.Session`, exported per-session at
/// `session_handle`.
struct SessionObj {
    handle: OwnedObjectPath,
    sessions: SessionRegistry,
    active_captures: ActiveCaptures,
}

#[interface(name = "org.freedesktop.impl.portal.Session")]
impl SessionObj {
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        2
    }

    async fn close(
        &self,
        #[zbus(connection)] conn: &Connection,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) {
        tracing::info!("[portal] Session.Close {}", self.handle);
        // Stop the capture cadence for this session first: once the pw thread
        // is asked to quit below, nothing reads `FrameSlot` again anyway, but
        // this also drops it out of the timer's iteration on the very next
        // tick rather than racing a capture against a dying stream.
        self.active_captures.lock().unwrap().remove(&self.handle);
        let removed = self.sessions.lock().unwrap().remove(&self.handle);
        if let Some(state) = removed {
            if let Some(stop_tx) = state.stream_stop {
                if let Err(e) = stop_tx.send(()) {
                    tracing::debug!(
                        "[portal] Session.Close {}: pipewire stream already gone ({e:?})",
                        self.handle
                    );
                }
            }
        }
        let _ = Self::closed(&emitter).await;
        if let Err(e) = conn.object_server().remove::<Self, _>(&self.handle).await {
            tracing::debug!("[portal] Session object at {} already gone: {e}", self.handle);
        }
    }

    #[zbus(signal)]
    async fn closed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

/// `org.freedesktop.impl.portal.Request`, exported per-call at `handle`.
struct RequestObj {
    handle: OwnedObjectPath,
}

#[interface(name = "org.freedesktop.impl.portal.Request")]
impl RequestObj {
    async fn close(&self, #[zbus(connection)] conn: &Connection) {
        tracing::info!("[portal] Request.Close {}", self.handle);
        if let Err(e) = conn.object_server().remove::<Self, _>(&self.handle).await {
            tracing::debug!("[portal] Request object at {} already gone: {e}", self.handle);
        }
    }
}

const BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.rubix";
const OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";

async fn run_dbus_service(
    bridge_tx: calloop::channel::Sender<PortalRequest>,
    sessions: SessionRegistry,
    active_captures: ActiveCaptures,
) -> zbus::Result<()> {
    let iface = ScreenCastIface { bridge_tx: Mutex::new(bridge_tx), sessions, active_captures };

    let path = ObjectPath::try_from(OBJECT_PATH).expect("valid object path literal");
    let _conn = zbus::connection::Builder::session()?
        .name(BUS_NAME)?
        .serve_at(path, iface)?
        .build()
        .await?;

    tracing::info!("[portal] ScreenCast backend registered as {BUS_NAME} at {OBJECT_PATH}");

    // Keep this future (and therefore the connection + interface data) alive
    // forever; the zbus/async-io executor keeps servicing the connection in
    // the background regardless.
    std::future::pending::<()>().await;
    Ok(())
}

/// Wire the D-Bus spine into the compositor: a calloop channel source on the
/// loop thread, and a dedicated `std::thread` driving the zbus connection via
/// `async_io::block_on`. Best-effort, same posture as `init_ipc` /
/// `init_xwayland`: any failure logs and leaves the portal off for this run.
///
/// Gated on `RUBIX_PORTAL` (default enabled; `RUBIX_PORTAL=0` disables) so it
/// can be turned off without a rebuild if it misbehaves. Does NOT touch
/// `~/.config/xdg-desktop-portal` -- `xdg-desktop-portal-wlr` remains the
/// live ScreenCast backend regardless of whether this registers.
pub fn init_portal(event_loop: &EventLoop<'static, RubixState>) {
    if std::env::var("RUBIX_PORTAL").as_deref() == Ok("0") {
        tracing::info!("[portal] RUBIX_PORTAL=0; ScreenCast portal backend disabled");
        return;
    }

    let (bridge_tx, bridge_channel) = calloop::channel::channel::<PortalRequest>();

    // Shared with the zbus thread's `ScreenCastIface` below: the loop thread
    // needs it to stash each session's PipeWire stop handle after spawning
    // the M2 producer; `Session.Close`/`Request.Close` (zbus thread) need it
    // to retrieve that handle and tear the stream down.
    let sessions: SessionRegistry = Arc::new(Mutex::new(HashMap::new()));
    let sessions_for_loop = sessions.clone();

    // Shared with the zbus thread's `SessionObj::close` and the capture timer
    // below: which session streams what, and where its newest frame lands.
    let active_captures: ActiveCaptures = Arc::new(Mutex::new(HashMap::new()));
    let active_captures_for_loop = active_captures.clone();
    let active_captures_for_timer = active_captures.clone();

    let registered =
        event_loop.handle().insert_source(bridge_channel, move |event, _, data: &mut RubixState| {
            match event {
                calloop::channel::Event::Msg(PortalRequest::ListSources { reply }) => {
                    let sources = build_source_list(data);
                    if reply.try_send(sources).is_err() {
                        tracing::debug!("[portal] ListSources reply dropped (zbus side gone)");
                    }
                }
                calloop::channel::Event::Msg(PortalRequest::StartStream {
                    session_handle,
                    target,
                    reply,
                }) => {
                    if !sessions_for_loop.lock().unwrap().contains_key(&session_handle) {
                        tracing::warn!(
                            "[portal] StartStream: unknown session {session_handle}, refusing"
                        );
                        let _ = reply.try_send(Err("unknown session".to_string()));
                        return;
                    }

                    // Re-validate the chooser's pick against current
                    // compositor state -- it may have vanished (window
                    // closed, output unplugged) between the chooser running
                    // on the zbus thread and this message landing here.
                    let target_exists = match &target {
                        CaptureTarget::Window(id) => data.windows.contains_key(id),
                        CaptureTarget::Monitor(name) => {
                            data.space.outputs().any(|o| &o.name() == name)
                        }
                    };
                    if !target_exists {
                        let _ = reply.try_send(Err(format!("{target:?} no longer available")));
                        return;
                    }

                    let Some((width, height)) = capture::target_size(data, &target) else {
                        let _ = reply.try_send(Err(format!("could not resolve size for {target:?}")));
                        return;
                    };

                    // pw node.name dislikes '/'; the session handle is an
                    // object path, so give it a flat, still-unique name.
                    let node_name =
                        format!("rubix-screencast-{}", session_handle.as_str().replace('/', "_"));
                    let slot: capture::FrameSlot = Arc::new(Mutex::new(None));
                    let stop_tx =
                        pipewire_stream::spawn_stream(node_name, width, height, slot.clone(), reply);
                    if let Some(state) = sessions_for_loop.lock().unwrap().get_mut(&session_handle) {
                        state.stream_stop = Some(stop_tx);
                        active_captures_for_loop
                            .lock()
                            .unwrap()
                            .insert(session_handle, (target, slot));
                    } else {
                        // Session vanished (raced with Close) between the
                        // check above and here; stop the thread we just
                        // spawned instead of leaking it.
                        let _ = stop_tx.send(());
                    }
                }
                calloop::channel::Event::Closed => {}
            }
        });

    if let Err(e) = registered {
        tracing::warn!("[portal] failed to register loop bridge source ({e}); portal disabled");
        return;
    }

    // Capture cadence: re-render every active session's target at ~30fps and
    // refresh its `FrameSlot`. A no-op (empty map, no renderer touched) while
    // no session is streaming, so this runs unconditionally rather than being
    // spawned/torn down per-session. Only produces real frames on the udev
    // backend (`state.udev_handle` is `None` under nested winit); see
    // `run_capture_tick`.
    let capture_timer = Timer::from_duration(CAPTURE_INTERVAL);
    if let Err(e) = event_loop.handle().insert_source(capture_timer, move |_, _, data: &mut RubixState| {
        run_capture_tick(&active_captures_for_timer, data);
        TimeoutAction::ToDuration(CAPTURE_INTERVAL)
    }) {
        tracing::warn!("[portal] failed to register capture timer ({e}); ScreenCast frames disabled");
    }

    let spawned = std::thread::Builder::new().name("rubix-portal-dbus".to_string()).spawn(move || {
        async_io::block_on(async move {
            if let Err(e) = run_dbus_service(bridge_tx, sessions, active_captures).await {
                tracing::warn!("[portal] ScreenCast D-Bus service exited with an error: {e}");
            }
        });
    });

    match spawned {
        Ok(_) => tracing::info!("[portal] ScreenCast D-Bus thread spawned"),
        Err(e) => tracing::warn!("[portal] failed to spawn ScreenCast D-Bus thread ({e}); portal disabled"),
    }
}

/// One capture-timer tick: re-render every active session's target and
/// refresh its `FrameSlot`. Renderer acquisition mirrors `udev.rs`'s own
/// render path (`render()`'s `single_renderer` call): only the udev backend
/// populates `state.udev_handle`, so this is a silent no-op under nested
/// winit -- there is no equivalent renderer handle to reach for there (see
/// the M3/M4 milestone report for why real-content validation is deferred to
/// the user's next udev/TTY session rather than exercised here).
fn run_capture_tick(active_captures: &ActiveCaptures, data: &mut RubixState) {
    let Some(udev) = data.udev_handle.clone() else { return };

    let targets: Vec<(OwnedObjectPath, CaptureTarget, capture::FrameSlot)> = {
        let guard = active_captures.lock().unwrap();
        if guard.is_empty() {
            return;
        }
        guard.iter().map(|(handle, (target, slot))| (handle.clone(), target.clone(), slot.clone())).collect()
    };

    let mut guard = udev.borrow_mut();
    let primary = guard.primary_gpu;
    let mut renderer = match guard.gpus.single_renderer(&primary) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("[portal] capture tick: failed to acquire renderer: {e}");
            return;
        }
    };

    for (session_handle, target, slot) in targets {
        match capture::capture_frame(data, &mut renderer, &target) {
            Ok(frame) => *slot.lock().unwrap() = Some(Arc::new(frame)),
            Err(e) => {
                tracing::debug!("[portal] capture tick: {session_handle} ({target:?}) failed: {e}")
            }
        }
    }
}
