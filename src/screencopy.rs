//! wlr-screencopy-unstable-v1 (`zwlr_screencopy_manager_v1` / `_frame_v1`).
//!
//! Smithay 0.7 ships no helper for this protocol (anvil never implemented it),
//! so the `GlobalDispatch`/`Dispatch` impls are hand-rolled on top of the raw
//! bindings smithay re-exports from `wayland-protocols-wlr`. niri and
//! cosmic-comp do exactly this.
//!
//! Flow: a client `capture_output`s -> we announce an SHM `buffer` + `buffer_done`
//! -> client allocates a wl_shm buffer and `copy`s it. The protocol is "capture
//! the *next* frame", so the copy is not serviced in the request handler: it is
//! queued onto `RubixState::pending_screencopy` and drained by each backend's
//! render path (winit.rs / udev.rs) right after it presents a frame, via
//! [`fulfill_pending`].
//!
//! The readback re-renders the output's element list into our own offscreen
//! buffer (rather than reading a backend-specific scanout buffer) so one generic
//! path serves both the winit `GlesRenderer` and the udev `MultiRenderer`.
//!
//! Scope (v1): full-output and region SHM capture. `overlay_cursor` is honoured.
//! Linux-dmabuf buffers are not advertised (SHM only); multi-output assumes the
//! single output sits at the origin (see the multi-monitor sprint item).

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            damage::OutputDamageTracker,
            element::RenderElement,
            gles::GlesRenderbuffer,
            Bind, ExportMem, ImportAll, ImportMem, Offscreen, Renderer, Texture, TextureMapping,
        },
    },
    output::Output,
    reexports::{
        wayland_protocols_wlr::screencopy::v1::server::{
            zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
            zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
        },
        wayland_server::{
            protocol::{wl_buffer::WlBuffer, wl_output::WlOutput, wl_shm},
            Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
        },
    },
    utils::{Buffer as BufferCoord, Physical, Point, Rectangle, Transform},
    wayland::shm::{with_buffer_contents, with_buffer_contents_mut},
};

use crate::cursor::RubixRenderElement;
use crate::RubixState;

/// A capture waiting for the next presented frame. Built in the frame's `copy`
/// handler, drained by the backend render path.
pub struct PendingScreencopy {
    pub frame: ZwlrScreencopyFrameV1,
    pub buffer: WlBuffer,
    pub output: Output,
    /// Capture rect in output-local physical pixels.
    pub region: Rectangle<i32, Physical>,
    pub overlay_cursor: bool,
    pub with_damage: bool,
}

/// Per-frame protocol state (`Dispatch` user-data). `used` guards the one-shot
/// `copy`/`copy_with_damage` contract.
pub struct ScreencopyFrameState {
    output: Option<Output>,
    region: Rectangle<i32, Physical>,
    overlay_cursor: bool,
    used: Mutex<bool>,
}

/// Advertise the manager global. Version 3 (SHM + region + buffer_done).
pub fn init(dh: &DisplayHandle) {
    dh.create_global::<RubixState, ZwlrScreencopyManagerV1, ()>(3, ());
}

impl GlobalDispatch<ZwlrScreencopyManagerV1, ()> for RubixState {
    fn bind(
        _state: &mut Self,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrScreencopyManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwlrScreencopyManagerV1, ()> for RubixState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _manager: &ZwlrScreencopyManagerV1,
        request: zwlr_screencopy_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        use zwlr_screencopy_manager_v1::Request;
        match request {
            Request::CaptureOutput {
                frame,
                overlay_cursor,
                output,
            } => create_frame(frame, overlay_cursor != 0, &output, None, data_init),
            Request::CaptureOutputRegion {
                frame,
                overlay_cursor,
                output,
                x,
                y,
                width,
                height,
            } => {
                // Region is in output-logical coords; scale is 1.0 throughout, so
                // it maps to physical numerically.
                let region = Rectangle::new(
                    Point::from((x, y)),
                    (width.max(0), height.max(0)).into(),
                );
                create_frame(frame, overlay_cursor != 0, &output, Some(region), data_init)
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

/// Resolve the target output, clip the requested region to it, announce the SHM
/// buffer parameters, and install the frame's `Dispatch` state.
fn create_frame(
    frame: New<ZwlrScreencopyFrameV1>,
    overlay_cursor: bool,
    wl_output: &WlOutput,
    region: Option<Rectangle<i32, Physical>>,
    data_init: &mut DataInit<'_, RubixState>,
) {
    let output = Output::from_resource(wl_output);
    let full = output.as_ref().and_then(|o| {
        let mode = o.current_mode()?;
        // transform_size swaps w/h for 90/270; Normal/Flipped180 pass through.
        let size = o.current_transform().transform_size(mode.size);
        Some(Rectangle::new(Point::from((0, 0)), size))
    });

    let Some((output, full)) = output.zip(full) else {
        // Unknown output / no mode: hand back an inert frame and fail it so the
        // client destroys it instead of blocking on buffer_done.
        let frame = data_init.init(
            frame,
            ScreencopyFrameState {
                output: None,
                region: Rectangle::default(),
                overlay_cursor,
                used: Mutex::new(true),
            },
        );
        frame.failed();
        return;
    };

    let capture = match region {
        Some(r) => r.intersection(full).unwrap_or(full),
        None => full,
    };

    let width = capture.size.w as u32;
    let height = capture.size.h as u32;
    let stride = width * 4;

    let frame = data_init.init(
        frame,
        ScreencopyFrameState {
            output: Some(output),
            region: capture,
            overlay_cursor,
            used: Mutex::new(false),
        },
    );

    frame.buffer(wl_shm::Format::Xrgb8888, width, height, stride);
    if frame.version() >= 3 {
        frame.buffer_done();
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ScreencopyFrameState> for RubixState {
    fn request(
        state: &mut Self,
        _client: &Client,
        frame: &ZwlrScreencopyFrameV1,
        request: zwlr_screencopy_frame_v1::Request,
        data: &ScreencopyFrameState,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        use zwlr_screencopy_frame_v1::Request;
        let (buffer, with_damage) = match request {
            Request::Copy { buffer } => (buffer, false),
            Request::CopyWithDamage { buffer } => (buffer, true),
            Request::Destroy => return,
            _ => return,
        };

        // One-shot: a frame may back exactly one copy.
        {
            let mut used = data.used.lock().unwrap();
            if *used {
                frame.post_error(
                    zwlr_screencopy_frame_v1::Error::AlreadyUsed,
                    "frame already used to copy a buffer",
                );
                return;
            }
            *used = true;
        }

        let Some(output) = data.output.clone() else {
            frame.failed();
            return;
        };

        // Validate the client buffer: managed SHM, matching size, 32-bit format.
        let valid = with_buffer_contents(&buffer, |_ptr, _len, bd| {
            matches!(bd.format, wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888)
                && bd.width == data.region.size.w
                && bd.height == data.region.size.h
                && bd.stride >= data.region.size.w * 4
        });
        match valid {
            Ok(true) => {}
            Ok(false) => {
                frame.post_error(
                    zwlr_screencopy_frame_v1::Error::InvalidBuffer,
                    "buffer format or dimensions do not match the announced frame",
                );
                return;
            }
            Err(_) => {
                frame.post_error(
                    zwlr_screencopy_frame_v1::Error::InvalidBuffer,
                    "supplied buffer is not a managed wl_shm buffer",
                );
                return;
            }
        }

        state.pending_screencopy.push(PendingScreencopy {
            frame: frame.clone(),
            buffer,
            output,
            region: data.region,
            overlay_cursor: data.overlay_cursor,
            with_damage,
        });
        // winit repaints continuously; udev is idle-until-nudged, and grim blocks
        // until a frame is presented, so kick a repaint.
        state.nudge_render();
    }
}

/// Drain every queued capture targeting `output`, servicing each against the
/// just-rendered frame. Called from both backend render paths with their live
/// renderer. Captures for other outputs are left queued.
pub fn fulfill_pending<R>(state: &mut RubixState, renderer: &mut R, output: &Output)
where
    R: Renderer + ImportAll + ImportMem + ExportMem + Offscreen<GlesRenderbuffer> + Bind<GlesRenderbuffer>,
    R::TextureId: Texture + Clone + Send + 'static,
    RubixRenderElement<R>: RenderElement<R>,
    R: crate::rounding::GlesAccess,
{
    if state.pending_screencopy.is_empty() {
        return;
    }
    let pendings = std::mem::take(&mut state.pending_screencopy);
    let mut remaining = Vec::new();
    for pending in pendings {
        if &pending.output != output {
            remaining.push(pending);
            continue;
        }
        match copy_output(state, renderer, output, &pending) {
            Ok(y_invert) => {
                let flags = if y_invert {
                    zwlr_screencopy_frame_v1::Flags::YInvert
                } else {
                    zwlr_screencopy_frame_v1::Flags::empty()
                };
                pending.frame.flags(flags);
                if pending.with_damage {
                    pending.frame.damage(
                        0,
                        0,
                        pending.region.size.w as u32,
                        pending.region.size.h as u32,
                    );
                }
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default();
                let secs = now.as_secs();
                pending.frame.ready(
                    (secs >> 32) as u32,
                    (secs & 0xFFFF_FFFF) as u32,
                    now.subsec_nanos(),
                );
            }
            Err(err) => {
                tracing::warn!("screencopy: copy failed: {err}");
                pending.frame.failed();
            }
        }
    }
    state.pending_screencopy = remaining;
}

/// One-shot per capture path: what `Mapping::flipped()` actually reports.
///
/// `screencopy.rs` and `portal/capture.rs` do byte-identical readbacks -- same
/// offscreen, same `Transform::Normal`, same `copy_framebuffer`, same
/// conditional row flip -- and there is no second flip in the PipeWire feed. So
/// they cannot legitimately differ in orientation, yet grim output is verifiably
/// upside down while Teams screenshare is reported right way up. One of those
/// two observations has to be wrong, and guessing which would mean "fixing" one
/// path by breaking the other.
pub(crate) fn log_readback_orientation(path: &str, flipped: bool) {
    use std::sync::atomic::{AtomicU8, Ordering};
    static SEEN: AtomicU8 = AtomicU8::new(0);
    let bit = if path == "screencopy" { 1 } else { 2 };
    let prev = SEEN.fetch_or(bit, Ordering::Relaxed);
    if prev & bit == 0 {
        tracing::info!("capture readback [{path}]: Mapping::flipped() = {flipped}");
    }
}

/// Which source row feeds destination row `y`, given what `Mapping::flipped()`
/// reported.
///
/// The subtle part, and the reason this is a named, tested function rather than
/// an inline conditional: smithay documents `flipped()` as "whether the mapped
/// buffer is flipped on the y-axis **compared to the lower left being (0, 0)**".
/// The reference frame is GL's lower-left origin, so `flipped() == true` means
/// the buffer is ALREADY top-down -- upper-left origin, exactly what SHM buffers
/// and PNG want -- and must be copied straight through. `false` means it is
/// bottom-up and needs reversing.
///
/// Both capture paths had this backwards, flipping precisely when they should
/// not have. Screenshots came out upside down (verified: waybar along the bottom
/// edge, text vertically mirrored) and so did Teams screenshare.
pub(crate) fn source_row(y: usize, rows: usize, mapping_flipped: bool) -> usize {
    if mapping_flipped {
        y
    } else {
        rows - 1 - y
    }
}

/// Re-render `output`'s full element list into an offscreen renderbuffer, read
/// back the requested sub-region, and blit it into the client's SHM buffer.
/// Returns whether the copied contents are y-inverted (for the `flags` event).
fn copy_output<R>(
    state: &RubixState,
    renderer: &mut R,
    output: &Output,
    pending: &PendingScreencopy,
) -> Result<bool, String>
where
    R: Renderer + ImportAll + ImportMem + ExportMem + Offscreen<GlesRenderbuffer> + Bind<GlesRenderbuffer>,
    R::TextureId: Texture + Clone + Send + 'static,
    RubixRenderElement<R>: RenderElement<R>,
    R: crate::rounding::GlesAccess,
{
    let mode = output.current_mode().ok_or("output has no mode")?;
    let phys = output.current_transform().transform_size(mode.size);
    let scale = 1.0_f64;

    let elements = build_elements(state, renderer, output, pending.overlay_cursor);

    // Offscreen sized to the whole output; render Normal (the winit display's
    // Flipped180 is a window quirk, not the content) then read back the sub-rect.
    let buffer_size = smithay::utils::Size::<i32, BufferCoord>::from((phys.w, phys.h));
    let mut target = renderer
        .create_buffer(Fourcc::Xrgb8888, buffer_size)
        .map_err(|e| format!("create_buffer: {e:?}"))?;
    {
        let mut fb = renderer
            .bind(&mut target)
            .map_err(|e| format!("bind: {e:?}"))?;
        let mut damage_tracker = OutputDamageTracker::new(phys, scale, Transform::Normal);
        damage_tracker
            .render_output(renderer, &mut fb, 0, &elements, [0.1, 0.1, 0.1, 1.0])
            .map_err(|e| format!("render_output: {e:?}"))?;

        let region = Rectangle::<i32, BufferCoord>::new(
            Point::from((pending.region.loc.x, pending.region.loc.y)),
            (pending.region.size.w, pending.region.size.h).into(),
        );
        let mapping = renderer
            .copy_framebuffer(&fb, region, Fourcc::Xrgb8888)
            .map_err(|e| format!("copy_framebuffer: {e:?}"))?;
        // Normalize to top-down ourselves and report no inversion, rather than
        // depend on the client honouring the `y_invert` flag (grim does not
        // reliably here). See `source_row` for the orientation semantics.
        let mapping_flipped = mapping.flipped();
        log_readback_orientation("screencopy", mapping_flipped);
        let src = renderer
            .map_texture(&mapping)
            .map_err(|e| format!("map_texture: {e:?}"))?;

        write_shm(&pending.buffer, src, pending.region.size.w, pending.region.size.h, mapping_flipped)?;
        // Always false: we already delivered top-down pixels above.
        Ok(false)
    }
}

/// Copy the tightly-packed BGRX source into the client SHM buffer, honouring the
/// client's (possibly padded) stride. When `flip` is set the source is bottom-up
/// (GL readback), so rows are written in reverse to produce top-down output.
fn write_shm(
    buffer: &WlBuffer,
    src: &[u8],
    width: i32,
    height: i32,
    // Straight from `Mapping::flipped()`; see `source_row` for what it means.
    mapping_flipped: bool,
) -> Result<(), String> {
    let src_stride = (width * 4) as usize;
    let rows = height as usize;
    with_buffer_contents_mut(buffer, |ptr, len, bd| {
        let dst_stride = bd.stride as usize;
        let offset = bd.offset as usize;
        let row_bytes = src_stride.min(dst_stride);
        for y in 0..rows {
            let src_row = source_row(y, rows, mapping_flipped);
            let s = src_row * src_stride;
            let d = offset + y * dst_stride;
            if s + row_bytes > src.len() || d + row_bytes > len {
                continue;
            }
            // SAFETY: bounds checked above; dst is the mmap'd pool, src our mapping.
            unsafe {
                std::ptr::copy_nonoverlapping(src.as_ptr().add(s), ptr.add(d), row_bytes);
            }
        }
    })
    .map_err(|e| format!("shm write: {e:?}"))
}

/// Build the output's render element list, top-to-bottom: cursor (optional) ->
/// overlay -> top -> tiled windows -> bottom -> background. Mirrors the backend
/// render paths' z-order, minus the rotation ghosts (irrelevant to a still
/// capture). Assumes a single output at the origin.
fn build_elements<R>(
    state: &RubixState,
    renderer: &mut R,
    output: &Output,
    overlay_cursor: bool,
) -> Vec<RubixRenderElement<R>>
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Texture + Clone + Send + 'static,
    RubixRenderElement<R>: RenderElement<R>,
    R: crate::rounding::GlesAccess,
{
    let scale = 1.0_f64;
    // HDR Phase 4a. A capture destination is an ordinary 8-bit sRGB buffer, so
    // an HDR surface's texels have to be decoded and tone-mapped rather than
    // handed over as if they were already sRGB -- which is what made a captured
    // HDR game look like a blown-out white sky. Gated on there actually being an
    // HDR surface present, so the overwhelmingly common all-SDR capture takes
    // exactly the path it always did.
    let tonemap = crate::udev::output_has_hdr_content(state, output);
    // Without HDR content: RoundMode::Plain, since no colour conversion is in
    // play and a rounded element takes the plain texture program (falling back
    // to the batched call at corner_radius = 0). With it: each window resolved
    // to its own fused decode+tone-map.
    let space_mode = if tonemap {
        crate::rounding::SpaceMode::TonemapSdr
    } else {
        crate::rounding::SpaceMode::Fixed(crate::rounding::RoundMode::Plain)
    };

    crate::compose::compose_output_elements(
        state,
        renderer,
        output,
        crate::compose::ComposeOptions {
            scale,
            space_mode,
            wrap: crate::compose::WrapMode::Sdr { tonemap },
            include_wallpaper: true,
            cursor: if overlay_cursor {
                crate::compose::CursorMode::Global
            } else {
                crate::compose::CursorMode::Hidden
            },
            suppress_chrome: false,
        },
        Vec::new(),
        Vec::new(),
    )
}

#[cfg(test)]
mod orientation_tests {
    use super::source_row;

    // smithay: flipped() is "flipped on the y-axis compared to the LOWER LEFT
    // being (0, 0)". So true == already top-down == copy straight through.
    // Getting this backwards is what shipped upside-down screenshots and an
    // upside-down Teams screenshare, and it reads equally plausible either way,
    // which is why it is pinned here rather than left to a comment.
    #[test]
    fn flipped_true_means_already_top_down_so_rows_pass_through() {
        let rows = 4;
        let mapped: Vec<usize> = (0..rows).map(|y| source_row(y, rows, true)).collect();
        assert_eq!(mapped, vec![0, 1, 2, 3]);
    }

    #[test]
    fn flipped_false_means_bottom_up_so_rows_reverse() {
        let rows = 4;
        let mapped: Vec<usize> = (0..rows).map(|y| source_row(y, rows, false)).collect();
        assert_eq!(mapped, vec![3, 2, 1, 0]);
    }

    // Every destination row must draw from a distinct source row in range --
    // catches an off-by-one in the reversing branch, which would silently
    // duplicate one row and drop another.
    #[test]
    fn both_directions_are_a_permutation_of_the_rows() {
        for flipped in [true, false] {
            let rows = 7;
            let mut seen: Vec<usize> = (0..rows).map(|y| source_row(y, rows, flipped)).collect();
            seen.sort_unstable();
            assert_eq!(seen, (0..rows).collect::<Vec<_>>(), "flipped = {flipped}");
        }
    }

    #[test]
    fn single_row_is_identity_either_way() {
        assert_eq!(source_row(0, 1, true), 0);
        assert_eq!(source_row(0, 1, false), 0);
    }
}
