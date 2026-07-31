//! PipeWire video producer for the ScreenCast portal.
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
//!
//! M3/M4: real compositor frames now cross from the calloop/render thread to
//! this pw thread via [`crate::portal::capture::FrameSlot`] -- a
//! `Arc<Mutex<Option<Arc<FrameBuffer>>>>` the capture cadence (a calloop timer
//! in `screencast.rs`) writes into and this module's `process` callback reads
//! out of. The lock is only ever held for an `Arc` clone/compare, never a byte
//! copy; the actual per-cycle memcpy into the dequeued PipeWire buffer happens
//! here, on the pw thread.
//!
//! Per-session lifecycle: `spawn_stream` returns a `pipewire::channel::Sender<()>`
//! immediately; the caller stores it in the session registry. Sending `()` on
//! it (from `Session.Close`/`Request.Close`) wakes the pw thread's attached
//! receiver, which calls `MainLoop::quit()`, unwinding `run()` and dropping the
//! stream/core/context/loop before the thread exits -- no leaks across
//! sessions.

use std::io;
use std::sync::Arc;

use pipewire as pw;
use pw::spa::pod::{self, Pod};
use pw::spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Direction, Fraction, Id, Rectangle};

use crate::portal::capture::{FrameBuffer, FrameSlot};

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

/// Build the single fixed `SPA_PARAM_EnumFormat` pod this stream offers:
/// video/raw, BGRx, at the target's resolved size, @ `FRAMERATE`fps. No
/// dmabuf, no choices -- one concrete format, since we control both ends.
/// `BGRx` is a deliberate match for `capture.rs`'s `Xrgb8888` readback (see
/// that module's doc comment): identical byte order, so `process` below can
/// memcpy captured frames straight into the negotiated buffer.
fn format_param(width: u32, height: u32) -> OwnedPod {
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
                value: pod::Value::Rectangle(Rectangle { width, height }),
            },
            pod::Property {
                key: pw::spa::sys::SPA_FORMAT_VIDEO_framerate,
                flags: pod::PropertyFlags::empty(),
                value: pod::Value::Fraction(Fraction { num: FRAMERATE, denom: 1 }),
            },
        ],
    }))
}

/// Build the `SPA_PARAM_Buffers` pod advertising this stream's buffer
/// requirements, plus a `SPA_PARAM_Meta` pod for the standard `Header`
/// metadata. Emitted from the `param_changed` callback once the format has
/// negotiated (see `param_changed` below) -- without this, strict consumers
/// (gstreamer's `pipewiresrc`, Teams' `mod.client-node` path) fail buffer
/// allocation with `-22 (EINVAL)` even though lenient consumers tolerate its
/// absence. `data_type` is a bitmask (`1 << SPA_DATA_*`) of the buffer memory
/// kinds we're willing to hand out; MemFd must be included for strict
/// consumers even though `process` below currently writes through MemPtr.
fn buffers_params(width: u32, height: u32) -> (OwnedPod, OwnedPod) {
    let stride = width * 4;
    let size = stride * height;
    let data_type = (1u32 << pw::spa::sys::SPA_DATA_MemPtr) | (1u32 << pw::spa::sys::SPA_DATA_MemFd);

    let buffers = OwnedPod::serialize(&pod::Value::Object(pod::Object {
        type_: pw::spa::sys::SPA_TYPE_OBJECT_ParamBuffers,
        id: pw::spa::sys::SPA_PARAM_Buffers,
        properties: vec![
            pod::Property {
                key: pw::spa::sys::SPA_PARAM_BUFFERS_buffers,
                flags: pod::PropertyFlags::empty(),
                value: pod::Value::Choice(pod::ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range { default: 4, min: 2, max: 8 },
                ))),
            },
            pod::Property {
                key: pw::spa::sys::SPA_PARAM_BUFFERS_blocks,
                flags: pod::PropertyFlags::empty(),
                value: pod::Value::Int(1),
            },
            pod::Property {
                key: pw::spa::sys::SPA_PARAM_BUFFERS_size,
                flags: pod::PropertyFlags::empty(),
                value: pod::Value::Int(size as i32),
            },
            pod::Property {
                key: pw::spa::sys::SPA_PARAM_BUFFERS_stride,
                flags: pod::PropertyFlags::empty(),
                value: pod::Value::Int(stride as i32),
            },
            pod::Property {
                key: pw::spa::sys::SPA_PARAM_BUFFERS_align,
                flags: pod::PropertyFlags::empty(),
                value: pod::Value::Int(16),
            },
            pod::Property {
                key: pw::spa::sys::SPA_PARAM_BUFFERS_dataType,
                flags: pod::PropertyFlags::empty(),
                value: pod::Value::Choice(pod::ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Flags { default: data_type as i32, flags: vec![data_type as i32] },
                ))),
            },
        ],
    }));

    let meta = OwnedPod::serialize(&pod::Value::Object(pod::Object {
        type_: pw::spa::sys::SPA_TYPE_OBJECT_ParamMeta,
        id: pw::spa::sys::SPA_PARAM_Meta,
        properties: vec![
            pod::Property {
                key: pw::spa::sys::SPA_PARAM_META_type,
                flags: pod::PropertyFlags::empty(),
                value: pod::Value::Id(Id(pw::spa::sys::SPA_META_Header)),
            },
            pod::Property {
                key: pw::spa::sys::SPA_PARAM_META_size,
                flags: pod::PropertyFlags::empty(),
                value: pod::Value::Int(std::mem::size_of::<pw::spa::sys::spa_meta_header>() as i32),
            },
        ],
    }));

    (buffers, meta)
}

/// `param_changed` stream callback: once the concrete `SPA_PARAM_Format`
/// lands (the only format we ever offer, so negotiation is trivial -- see
/// `format_param`'s doc comment), push the `Buffers`/`Meta` params so strict
/// consumers can actually allocate buffers. Without this round-trip the link
/// negotiates a format but dies at `port_use_buffers` with `-22`.
fn param_changed(state: &mut StreamState, stream: &pw::stream::Stream, id: u32, param: Option<&Pod>) {
    if id != pw::spa::sys::SPA_PARAM_Format || param.is_none() {
        return;
    }

    tracing::debug!("[portal] format negotiated, advertising buffer params ({}x{})", state.width, state.height);

    let (buffers, meta) = buffers_params(state.width, state.height);
    let mut params = [buffers.as_pod(), meta.as_pod()];
    if let Err(e) = stream.update_params(&mut params) {
        tracing::warn!("[portal] failed to update stream buffer params: {e}");
    }
}

/// Per-stream data living entirely on the pw thread. Not `Send` -- never
/// crosses back to the calloop/zbus threads.
struct StreamState {
    width: u32,
    height: u32,
    node_id_reply: Option<async_channel::Sender<Result<StreamStarted, String>>>,
    /// Where the capture cadence (loop thread) deposits the newest frame.
    frame_slot: FrameSlot,
    /// The last frame actually painted into a pw buffer. Kept so a `process`
    /// cycle that races ahead of the ~33ms capture cadence (pw's graph often
    /// runs faster than our capture rate) re-submits real content instead of
    /// blanking -- exactly the M2 "resubmit last frame's content unchanged"
    /// behaviour, now with real frames instead of a painted test pattern.
    last_frame: Option<Arc<FrameBuffer>>,
}

fn state_changed(
    state: &mut StreamState,
    stream: &pw::stream::Stream,
    old: pw::stream::StreamState,
    new: pw::stream::StreamState,
) {
    tracing::debug!("[portal] pipewire stream state {old:?} -> {new:?}");
    match new {
        pw::stream::StreamState::Paused | pw::stream::StreamState::Streaming => {
            if let Some(reply) = state.node_id_reply.take() {
                let started =
                    StreamStarted { node_id: stream.node_id(), width: state.width, height: state.height };
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

/// Pull the newest captured frame (if any) into the dequeued PipeWire buffer.
/// Falls back to the last frame actually painted (process can run faster than
/// the capture cadence), and to black (never uninitialized memory) before the
/// very first capture has landed.
fn process(state: &mut StreamState, stream: &pw::stream::Stream) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let datas = buffer.datas_mut();
    let Some(data) = datas.first_mut() else { return };

    if let Some(frame) = state.frame_slot.lock().unwrap().clone() {
        state.last_frame = Some(frame);
    }

    let stride = state.width * 4;
    let expected_len = (stride * state.height) as usize;

    if let Some(slice) = data.data() {
        match &state.last_frame {
            Some(frame) if frame.width == state.width && frame.height == state.height => {
                // Row-wise, honouring the captured stride (`capture.rs` packs
                // tightly today, but this stays correct if that ever changes)
                // against our own negotiated (also tight) stride.
                let dst_stride = stride as usize;
                let src_stride = frame.stride as usize;
                let row_bytes = dst_stride.min(src_stride);
                let rows = state.height as usize;
                for y in 0..rows {
                    let s = y * src_stride;
                    let d = y * dst_stride;
                    if s + row_bytes > frame.data.len() || d + row_bytes > slice.len() {
                        break;
                    }
                    slice[d..d + row_bytes].copy_from_slice(&frame.data[s..s + row_bytes]);
                }
            }
            _ => {
                // No frame captured yet, or its size no longer matches this
                // stream's negotiated format (e.g. target resized -- dynamic
                // renegotiation is out of scope here) -- black beats garbage.
                let n = expected_len.min(slice.len());
                slice[..n].fill(0);
            }
        }
    }

    let chunk = data.chunk_mut();
    *chunk.offset_mut() = 0;
    *chunk.stride_mut() = stride as i32;
    *chunk.size_mut() = stride * state.height;
}

fn run_pw_thread(
    node_name: String,
    width: u32,
    height: u32,
    frame_slot: FrameSlot,
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
            *pw::keys::NODE_DESCRIPTION => "Rubix ScreenCast",
        },
    )?;

    let data = StreamState {
        width,
        height,
        node_id_reply: Some(reply.clone()),
        frame_slot,
        last_frame: None,
    };

    let _listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(|stream, data, old, new| state_changed(data, stream, old, new))
        .param_changed(|stream, data, id, param| param_changed(data, stream, id, param))
        .process(|stream, data| process(data, stream))
        .register()?;

    let format = format_param(width, height);
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
    tracing::info!("[portal] pipewire thread for {node_name} exiting");
    Ok(())
}

/// Spawn the dedicated PipeWire thread for one ScreenCast session. Returns
/// immediately with a stop handle the caller must retain and send `()` on
/// when the session/request closes; the node id (or an error) arrives later
/// on `reply`, once the stream has actually connected. `frame_slot` is shared
/// with the loop-thread capture cadence (`screencast.rs`); this thread only
/// ever reads it.
pub fn spawn_stream(
    node_name: String,
    width: u32,
    height: u32,
    frame_slot: FrameSlot,
    reply: async_channel::Sender<Result<StreamStarted, String>>,
) -> pw::channel::Sender<()> {
    let (stop_tx, stop_rx) = pw::channel::channel::<()>();
    let stop_tx_for_caller = stop_tx.clone();
    let reply_for_spawn_err = reply.clone();

    let spawned = std::thread::Builder::new()
        .name("rubix-portal-pw".to_string())
        .spawn(move || {
            if let Err(e) = run_pw_thread(node_name, width, height, frame_slot, stop_rx, reply.clone()) {
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
