//! HDR Phase 1b: server-side `wp_color_management_v1`.
//!
//! Lets clients (mpv `--vo=gpu-next`, Firefox) attach a parametric image
//! description (transfer function + primaries) to a surface, describing
//! whether its content is SDR sRGB or HDR PQ/BT.2020. This module is
//! metadata-only -- it advertises support and exposes the committed
//! description via [`get_surface_description`]; the actual decode (which
//! shader a surface's render elements go through) lives in
//! `udev::render_surface_hdr`, keyed off [`surface_decode_kind`].
//!
//! Only sRGB and ST 2084 PQ transfer functions are advertised (this phase's
//! explicit scope -- see the spec's "Deferred" section for HLG/scRGB/etc.).
//! No mastering-metadata features are advertised: Rubix doesn't tone-map or
//! parse target-volume metadata this phase (Phase 1a's static mastering
//! metadata in `src/hdr.rs` is unrelated and untouched).

use std::sync::atomic::{AtomicBool, Ordering};

use smithay::output::Output;
use smithay::reexports::wayland_protocols::wp::color_management::v1::server::wp_image_description_info_v1::WpImageDescriptionInfoV1;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::wayland::color::management::{
    get_surface_description, send_image_description_info, ColorManagementHandler,
    ColorManagementState, ImageDescription, Primaries, PrimariesOption, TransferFunction,
};

use crate::RubixState;

/// Which decode shader a surface's render elements should go through
/// (`udev::render_surface_hdr`'s per-run tex-program override). Mirrors
/// `TransferFunction` but collapsed to exactly the two decode paths this
/// phase implements.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeKind {
    /// sRGB EOTF -> BT.709->BT.2020 -> `sdr_white_nits` scaling (`DECODE_SDR`).
    Sdr,
    /// ST 2084 (PQ) inverse EOTF, BT.2020 passthrough (`DECODE_HDR_PQ`).
    HdrPq,
}

/// Latches once so the "client declared an unsupported transfer function"
/// warning isn't spammed every frame -- it's a one-time visibility aid for
/// the journal, not per-frame diagnostics.
static WARNED_UNSUPPORTED_TF: AtomicBool = AtomicBool::new(false);

/// Maps a surface's committed color-management state to a decode kind.
/// `St2084Pq` -> `HdrPq`; no description, `Srgb`, or any other (unsupported
/// this phase) transfer function -> `Sdr`, with a one-time `warn!` for the
/// unsupported-but-non-sRGB case so the SDR fallback is visible in the
/// journal.
pub fn surface_decode_kind(surface: &WlSurface) -> DecodeKind {
    let (description, _intent) = get_surface_description(surface);
    match description {
        Some(desc) if desc.transfer == TransferFunction::St2084Pq => DecodeKind::HdrPq,
        Some(desc) if desc.transfer == TransferFunction::Srgb => DecodeKind::Sdr,
        Some(desc) => {
            if !WARNED_UNSUPPORTED_TF.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    transfer = ?desc.transfer,
                    "surface declared an unsupported color-management transfer function; \
                     falling back to SDR decode (safe: looks like today)"
                );
            }
            DecodeKind::Sdr
        }
        None => DecodeKind::Sdr,
    }
}

/// PQ/BT.2020 image description advertised on HDR-enabled outputs so HDR-aware
/// clients detect display HDR headroom. `ref_white` ties the advertised
/// reference white to the live SDR-white slider so SDR content sits at the same
/// level clients expect.
fn hdr_output_description(ref_white: u32) -> ImageDescription {
    ImageDescription {
        transfer: TransferFunction::St2084Pq,
        primaries: PrimariesOption { named: Some(Primaries::Bt2020), values: None },
        max_cll: None,
        max_fall: None,
        mastering_luminance: None,
        mastering_primaries: None,
        // (min 0.0001 cd/m², max cd/m², reference white cd/m²). 1000-nit peak
        // matches our Phase 1a mastering metadata; ref white follows the slider.
        luminances: Some((50, 1000, ref_white)),
        windows_scrgb: false,
        windows_bt2100: false,
    }
}

/// Advertises the `wp_color_management_v1` global and constructs its state.
/// Called from `RubixState::new` (mirrors the other Smithay protocol states
/// built there, e.g. `CompositorState::new::<Self>(&dh)`) --
/// `ColorManagementState::new` creates the global itself, same as those.
///
/// Advertises only what Deliverable 1's shaders implement: transfer
/// functions `Srgb` and `St2084Pq`, primaries `Srgb` (BT.709) and `Bt2020`.
/// No optional `Feature`s (mastering metadata, arbitrary primaries, etc.) --
/// `Feature::Parametric` and `RenderIntent::Perceptual` are advertised
/// automatically by `ColorManagementState::new` regardless. Visible to every
/// client (no per-client filtering needed).
pub fn init(dh: &DisplayHandle) -> ColorManagementState {
    ColorManagementState::new::<RubixState, _>(
        dh,
        [TransferFunction::Srgb, TransferFunction::St2084Pq],
        [Primaries::Srgb, Primaries::Bt2020],
        [],
        [],
        |_client| true,
    )
}

impl ColorManagementHandler for RubixState {
    fn color_management_state(&mut self) -> &mut ColorManagementState {
        &mut self.color_management_state
    }

    // `image_description_changed` and `preferred_description_for_surface`
    // keep the trait's sRGB-default bodies: Rubix doesn't advertise a
    // preferred-surface description this phase (Deferred: no tone mapping),
    // and the render path reads the surface's OWN description on every frame
    // via `surface_decode_kind` rather than reacting to a changed-surface
    // notification.

    /// Advertises PQ/BT.2020 on outputs currently running the HDR pipeline
    /// (`SurfaceData::hdr`, flipped live by `udev::toggle_hdr`), sRGB
    /// otherwise. This runs during protocol dispatch (`GetImageDescription`),
    /// so the `udev_handle` borrow MUST be non-blocking: `try_borrow`, fall
    /// back to SRGB if the state is already borrowed elsewhere rather than
    /// panicking.
    fn description_for_output(&mut self, output: &Output) -> ImageDescription {
        let is_hdr = self
            .udev_handle
            .as_ref()
            .and_then(|udev| {
                udev.try_borrow().ok().map(|u| {
                    u.backends
                        .values()
                        .any(|b| b.surfaces.values().any(|s| &s.output == output && s.hdr))
                })
            })
            .unwrap_or(false);
        if is_hdr {
            hdr_output_description(self.sdr_white_nits.round().clamp(1.0, 10_000.0) as u32)
        } else {
            ImageDescription::SRGB
        }
    }

    fn schedule_image_description_info(&mut self, info: WpImageDescriptionInfoV1, desc: ImageDescription) {
        // Must be deferred to an event-loop idle callback -- the trait's doc
        // comment warns that `done` is a destructor event and destroying the
        // object inside the very dispatch callback that created it corrupts
        // wayland-backend's bookkeeping (use-after-free on the next flush).
        self.loop_handle.insert_idle(move |_state| {
            send_image_description_info(&info, &desc);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hdr_output_description_is_pq_bt2020_with_ref_white() {
        let desc = hdr_output_description(203);
        assert_eq!(desc.transfer, TransferFunction::St2084Pq);
        assert_eq!(desc.primaries.named, Some(Primaries::Bt2020));
        assert_eq!(desc.luminances, Some((50, 1000, 203)));
    }
}
