//! M2: a self-contained PipeWire video producer for the ScreenCast portal.
//!
//! Architecture note: PipeWire's own loop runs on a **dedicated `std::thread`**
//! per stream (`pw::main_loop::MainLoopRc::run()`), not integrated into the
//! compositor's calloop `EventLoop`. This is the "acceptable fallback" the
//! milestone brief allows, taken deliberately over hand-rolling a raw
//! `pw_loop` fd source in calloop:
//!
//! - `pipewire-rs` 0.10's `Loop::iterate()` enters/dispatches/leaves in one
//!   call and is meant to be driven back-to-back on a thread that owns it;
//!   splitting "poll readiness via calloop" from "who calls iterate" adds a
//!   correctness surface (re-entrancy during `enter`/`leave`, timer sources,
//!   idle sources) that a real shipped compositor -- cosmic-comp's
//!   `xdg-desktop-portal-cosmic`, whose `screencast_thread.rs` this module's
//!   shape is deliberately modeled on -- also avoids by using a dedicated
//!   thread with `MainLoopRc::run()`.
//! - For M2's test pattern there is no renderer frame to hand across threads
//!   yet, so a dedicated thread is fully self-contained: `pw::init()`,
//!   connect, negotiate format, fill buffers, done. No cross-thread frame
//!   marshalling is needed at all this milestone.
//!
//! Implication for M3: once real compositor frames need to reach this stream,
//! they must cross from the calloop/render thread to this pw thread. That
//! will need either (a) a channel carrying dmabuf fds / shm handles into the
//! `process` callback's dequeued buffer, or (b) revisiting the calloop
//! integration once the frame-marshalling shape is known. Documented here so
//! M3 doesn't have to rediscover this.
//!
//! Per-session lifecycle: `spawn_test_pattern_stream` returns a
//! `pipewire::channel::Sender<()>` immediately; the caller stores it in the
//! session registry. Sending `()` on it (from `Session.Close`/`Request.Close`)
//! wakes the pw thread's attached receiver, which calls `MainLoop::quit()`,
//! unwinding `run()` and dropping the stream/core/context/loop before the
//! thread exits -- no leaks across sessions.

use std::io;

use pipewire as pw;
use pw::spa::pod::{self, Pod};
use pw::spa::utils::{Direction, Fraction, Id, Rectangle};

pub const WIDTH: u32 = 1280;
pub const HEIGHT: u32 = 720;
pub const FRAMERATE: u32 = 30;

/// Plain `Send` result of a successful `Start`: everything the zbus thread
/// needs to build the `(u, a{sv})` streams response.
#[derive(Clone, Copy, Debug)]
pub struct StreamStarted {
    pub node_id: u32,
    pub width: u32,
    pub height: u32,
}

struct OwnedPod(Vec<u8>);

impl OwnedPod {
    fn serialize(value: &pod::Value) -> Self {
        let mut bytes = Vec::new();
        let mut cursor = io::Cursor::new(&mut bytes);
        pod::serialize::PodSerializer::serialize(&mut cursor, value)
            .expect("pod serialization of a well-formed Value cannot fail");
        Self(bytes)
    }

    fn as_pod(&self) -> &Pod {
        // Unchecked version of `Pod::from_bytes`: we just serialized these
        // bytes ourselves above, so they are always a valid pod.
        unsafe { Pod::from_raw(self.0.as_ptr().cast()) }
    }
}

/// Build the single fixed `SPA_PARAM_EnumFormat` pod this test-pattern stream
/// offers: video/raw, BGRx, 1280x720 @ 30fps. No dmabuf, no choices -- one
/// concrete format, since we control both ends.
fn format_param() -> OwnedPod {
    OwnedPod::serialize(&pod::Value::Object(pod::Object {
        type_: pw::spa::sys::SPA_TYPE_OBJECT_Format,
        id: pw::spa::sys::SPA_PARAM_EnumFormat,
        properties: vec![
            pod::Property {
                key: pw::spa::sys::SPA_FORMAT_mediaType,
                flags: pod::PropertyFlags::empty(),
                value: pod::Value::Id(Id(pw::spa::sys::SPA_MEDIA_TYPE_video)),
            },
            pod::Property {
                key: pw::spa::sys::SPA_FORMAT_mediaSubtype,
                flags: pod::PropertyFlags::empty(),
                value: pod::Value::Id(Id(pw::spa::sys::SPA_MEDIA_SUBTYPE_raw)),
            },
            pod::Property {
                key: pw::spa::sys::SPA_FORMAT_VIDEO_format,
                flags: pod::PropertyFlags::empty(),
                value: pod::Value::Id(Id(pw::spa::sys::SPA_VIDEO_FORMAT_BGRx)),
            },
            pod::Property {
                key: pw::spa::sys::SPA_FORMAT_VIDEO_size,
                flags: pod::PropertyFlags::empty(),
                value: pod::Value::Rectangle(Rectangle { width: WIDTH, height: HEIGHT }),
            },
            pod::Property {
                key: pw::spa::sys::SPA_FORMAT_VIDEO_framerate,
                flags: pod::PropertyFlags::empty(),
                value: pod::Value::Fraction(Fraction { num: FRAMERATE, denom: 1 }),
            },
        ],
    }))
}

/// Per-stream data living entirely on the pw thread. Not `Send` -- never
/// crosses back to the calloop/zbus threads.
struct TestPatternState {
    frame: u64,
    node_id_reply: Option<async_channel::Sender<Result<StreamStarted, String>>>,
    /// `process` fires once per graph cycle, which runs at whatever quantum
    /// the shared PipeWire clock is using (often faster than our target
    /// framerate) -- not something this stream controls. We still service
    /// every cycle (skipping would stall the graph), but only repaint the
    /// moving bar at `FRAMERATE`; between repaints we resubmit the last
    /// frame's content unchanged, so playback rate matches `FRAMERATE`
    /// exactly rather than the (typically higher) driver cycle rate.
    last_paint: std::time::Instant,
}

fn state_changed(
    state: &mut TestPatternState,
    stream: &pw::stream::Stream,
    old: pw::stream::StreamState,
    new: pw::stream::StreamState,
) {
    tracing::debug!("[portal] pipewire stream state {old:?} -> {new:?}");
    match new {
        pw::stream::StreamState::Paused | pw::stream::StreamState::Streaming => {
            if let Some(reply) = state.node_id_reply.take() {
                let started =
                    StreamStarted { node_id: stream.node_id(), width: WIDTH, height: HEIGHT };
                if reply.try_send(Ok(started)).is_err() {
                    tracing::debug!("[portal] StartStream reply dropped (zbus side gone)");
                }
            }
        }
        pw::stream::StreamState::Error(msg) => {
            if let Some(reply) = state.node_id_reply.take() {
                let _ = reply.try_send(Err(format!("pipewire stream error: {msg}")));
            }
        }
        _ => {}
    }
}

/// Paint a moving vertical bar over a solid background directly into the
/// mapped buffer memory (`BGRx`, 4 bytes/px). Proves real frames flow without
/// needing a renderer yet.
fn paint_test_pattern(slice: &mut [u8], width: u32, height: u32, frame: u64) {
    let stride = width as usize * 4;
    let bar_width = (width / 16).max(1) as usize;
    let bar_x = (frame as usize * 6) % width as usize;

    for row in 0..height as usize {
        let row_start = row * stride;
        let Some(row_slice) = slice.get_mut(row_start..row_start + stride) else { break };
        for col in 0..width as usize {
            let px = &mut row_slice[col * 4..col * 4 + 4];
            let in_bar = col.abs_diff(bar_x) < bar_width / 2;
            if in_bar {
                // BGRx: bright bar.
                px.copy_from_slice(&[0x20, 0xE0, 0xE0, 0x00]);
            } else {
                // Dark teal background.
                px.copy_from_slice(&[0x30, 0x20, 0x10, 0x00]);
            }
        }
    }
}

fn process(state: &mut TestPatternState, stream: &pw::stream::Stream) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let datas = buffer.datas_mut();
    let Some(data) = datas.first_mut() else { return };

    let frame_interval = std::time::Duration::from_secs_f64(1.0 / FRAMERATE as f64);
    let now = std::time::Instant::now();
    if now.duration_since(state.last_paint) >= frame_interval {
        let frame = state.frame;
        state.frame = state.frame.wrapping_add(1);
        state.last_paint = now;
        if let Some(slice) = data.data() {
            paint_test_pattern(slice, WIDTH, HEIGHT, frame);
        }
    }
    // Either freshly painted above, or (buffer memory is stream-owned and
    // reused round-robin) still holding a recent frame from a previous
    // cycle -- either way the chunk metadata must be resubmitted every time.

    let stride = WIDTH * 4;
    let chunk = data.chunk_mut();
    *chunk.offset_mut() = 0;
    *chunk.stride_mut() = stride as i32;
    *chunk.size_mut() = stride * HEIGHT;
}

fn run_pw_thread(
    node_name: String,
    stop_rx: pw::channel::Receiver<()>,
    reply: async_channel::Sender<Result<StreamStarted, String>>,
) -> Result<(), pw::Error> {
    pw::init();

    let main_loop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&main_loop, None)?;
    let core = context.connect_rc(None)?;

    let stream = pw::stream::StreamRc::new(
        core,
        &node_name,
        pw::properties::properties! {
            *pw::keys::MEDIA_CLASS => "Video/Source",
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Screen",
            *pw::keys::MEDIA_ROLE => "Screen",
            *pw::keys::NODE_NAME => node_name.clone(),
            *pw::keys::NODE_DESCRIPTION => "Rubix ScreenCast test pattern",
        },
    )?;

    let data = TestPatternState {
        frame: 0,
        node_id_reply: Some(reply.clone()),
        last_paint: std::time::Instant::now(),
    };

    let _listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(|stream, data, old, new| state_changed(data, stream, old, new))
        .process(|stream, data| process(data, stream))
        .register()?;

    let format = format_param();
    let mut params = [format.as_pod()];

    let flags = pw::stream::StreamFlags::AUTOCONNECT
        | pw::stream::StreamFlags::MAP_BUFFERS
        | pw::stream::StreamFlags::RT_PROCESS;

    if let Err(e) = stream.connect(Direction::Output, None, flags, &mut params) {
        let _ = reply.try_send(Err(format!("stream connect failed: {e}")));
        return Err(e);
    }

    let weak_loop = main_loop.downgrade();
    let _stop_source = stop_rx.attach(main_loop.loop_(), move |()| {
        if let Some(l) = weak_loop.upgrade() {
            l.quit();
        }
    });

    main_loop.run();
    tracing::info!("[portal] pipewire test-pattern thread for {node_name} exiting");
    Ok(())
}

/// Spawn the dedicated PipeWire thread for one ScreenCast session's test
/// pattern. Returns immediately with a stop handle the caller must retain and
/// send `()` on when the session/request closes; the node id (or an error)
/// arrives later on `reply`, once the stream has actually connected.
pub fn spawn_test_pattern_stream(
    node_name: String,
    reply: async_channel::Sender<Result<StreamStarted, String>>,
) -> pw::channel::Sender<()> {
    let (stop_tx, stop_rx) = pw::channel::channel::<()>();
    let stop_tx_for_caller = stop_tx.clone();
    let reply_for_spawn_err = reply.clone();

    let spawned = std::thread::Builder::new()
        .name("rubix-portal-pw".to_string())
        .spawn(move || {
            if let Err(e) = run_pw_thread(node_name, stop_rx, reply.clone()) {
                tracing::warn!("[portal] pipewire thread exited with an error: {e}");
                let _ = reply.try_send(Err(e.to_string()));
            }
        });

    if let Err(e) = spawned {
        tracing::warn!("[portal] failed to spawn pipewire thread: {e}");
        let _ = reply_for_spawn_err.try_send(Err(format!("failed to spawn pipewire thread: {e}")));
    }

    stop_tx_for_caller
}
