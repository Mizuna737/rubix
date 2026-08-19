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
    ColorManagementState, Feature, ImageDescription, Primaries, PrimariesOption, TransferFunction,
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
    /// Windows-scRGB: already-linear BT.709, extended range, 1.0 == 80 cd/m²
    /// (`DECODE_WINDOWS_SCRGB`). What DXGI titles produce via
    /// `VK_COLOR_SPACE_EXTENDED_SRGB_LINEAR_EXT`.
    WindowsScrgb,
}

impl DecodeKind {
    /// Whether this decode kind carries HDR-range content, and so should drive
    /// an HDR-capable connector into HDR rather than out of it.
    ///
    /// `HdrPq` and `WindowsScrgb` differ in encoding but not in intent -- both
    /// are asking for more range than SDR can carry, so every connector /
    /// pipeline decision keys off this rather than matching variants
    /// individually and forgetting one.
    pub fn is_hdr(self) -> bool {
        matches!(self, DecodeKind::HdrPq | DecodeKind::WindowsScrgb)
    }
}

/// Latches once so the "client declared an unsupported transfer function"
/// warning isn't spammed every frame -- it's a one-time visibility aid for
/// the journal, not per-frame diagnostics.
static WARNED_UNSUPPORTED_TF: AtomicBool = AtomicBool::new(false);

/// Same latch for the untagged-fullscreen-surface notice below.
static WARNED_UNTAGGED_FULLSCREEN: AtomicBool = AtomicBool::new(false);

/// Maps a surface's committed color-management state to a decode kind.
/// `St2084Pq` -> `HdrPq`; no description, `Srgb`, or any other (unsupported
/// this phase) transfer function -> `Sdr`, with a one-time `warn!` for the
/// unsupported-but-non-sRGB case so the SDR fallback is visible in the
/// journal.
pub fn surface_decode_kind(surface: &WlSurface) -> DecodeKind {
    let (description, _intent) = get_surface_description(surface);
    match description {
        // Checked before the transfer function: the Windows pre-defined
        // descriptions are identified by flag, not by a TF the client chose.
        // Windows-BT.2100 *is* PQ/BT.2020, so it reuses the PQ decode exactly;
        // Windows-scRGB is linear BT.709 and needs its own.
        Some(desc) if desc.windows_scrgb => DecodeKind::WindowsScrgb,
        Some(desc) if desc.windows_bt2100 => DecodeKind::HdrPq,
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

/// Whether `surface` has committed any color-management image description.
///
/// Distinguishes the two ways a surface ends up on the SDR decode path, which
/// [`surface_decode_kind`] deliberately collapses: a client that declared sRGB
/// (genuinely SDR, nothing to see) versus a client that declared nothing at all.
/// The second is the interesting one on an HDR output -- it is what an HDR-aware
/// client does when the feature it wanted to tag with was never advertised, and
/// it is indistinguishable from ordinary SDR content until you ask.
pub fn surface_description_present(surface: &WlSurface) -> bool {
    get_surface_description(surface).0.is_some()
}

/// One-shot notice that a fullscreen surface on an HDR-capable output carries no
/// image description, so it is being treated as SDR.
///
/// This is the shape of the KCD2 failure: the game reads our HDR output
/// description, enables its HDR setting, then has no advertised way to tag its
/// own surface -- `create_windows_scrgb` is the request DXGI titles reach for
/// and we do not advertise `Feature::WindowsScrgb` -- so it submits HDR-range
/// content untagged. We then drive the connector to SDR underneath it. Nothing
/// errors; the picture is simply wrong, and the log said nothing at all.
pub fn note_untagged_fullscreen(surface: &WlSurface, output_name: &str) {
    if surface_description_present(surface) {
        return;
    }
    if WARNED_UNTAGGED_FULLSCREEN.swap(true, Ordering::Relaxed) {
        return;
    }
    tracing::warn!(
        "fullscreen surface on HDR-capable output {output_name} committed NO color-management \
         image description -- treating as SDR and dropping the connector out of HDR. If this \
         client believes it is producing HDR, it had no advertised way to say so (see \
         color_management::init: no Feature::WindowsScrgb / WindowsBt2100)."
    );
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
        // WindowsScrgb / WindowsBt2100 are the two pre-defined descriptions DXGI
        // titles reach for, and map to real decode paths
        // (`DecodeKind::WindowsScrgb` / `HdrPq`).
        //
        // SetLuminances and SetMasteringDisplayPrimaries are advertised even
        // though nothing downstream reads the values yet, because withholding
        // them costs more than accepting them. wine-wayland's driver calls
        // set_luminances / set_mastering_display_primaries / set_max_cll /
        // set_max_fall when building a parametric HDR description; with those
        // features absent it cannot describe HDR the way it wants and declines
        // to tag the surface AT ALL -- observed with KCD2, which then submitted
        // HDR-range content untagged and got displayed as SDR.
        //
        // What ignoring them actually means: mastering primaries and max CLL /
        // max FALL are tone-mapping hints for a display less capable than the
        // mastering display. Rubix does not tone-map -- content passes through
        // and the panel clips -- which is a legitimate choice, and the same one
        // any non-tone-mapping compositor makes. Reference white from
        // set_luminances does not affect PQ decode either, PQ being absolute.
        // So we accept the metadata honestly and act on what we can.
        //
        // Still NOT advertised: SetPrimaries (custom primaries -- wine uses
        // set_primaries_named, which is core and ungated), SetTfPower,
        // ExtendedTargetVolume, IccV2V4. Parametric is added by smithay
        // regardless.
        [
            Feature::WindowsScrgb,
            Feature::WindowsBt2100,
            Feature::SetLuminances,
            Feature::SetMasteringDisplayPrimaries,
        ],
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
    /// so it must not touch the udev `RefCell` at all -- it reads the
    /// `hdr_outputs` cache instead.
    fn description_for_output(&mut self, output: &Output) -> ImageDescription {
        // Reads the `hdr_outputs` cache rather than the udev RefCell. The
        // previous version did `try_borrow(...).unwrap_or(false)`, so a
        // transient borrow conflict during dispatch silently downgraded an HDR
        // output to sRGB -- see `RubixState::hdr_outputs`.
        let is_hdr = self.hdr_outputs.contains(output.name().as_str());
        // Logged because this is the single point where a client learns whether
        // a display has HDR headroom, and getting it wrong is invisible from
        // our side -- the client just quietly decides HDR is unavailable. Fires
        // only on an explicit get_image_description, not per frame.
        tracing::info!(
            "description_for_output({}) -> {} (hdr_outputs = {:?})",
            output.name(),
            if is_hdr { "PQ/BT.2020 HDR" } else { "sRGB" },
            self.hdr_outputs,
        );
        if is_hdr {
            hdr_output_description(self.sdr_white_nits.round().clamp(1.0, 10_000.0) as u32)
        } else {
            ImageDescription::SRGB
        }
    }

    fn schedule_image_description_info(&mut self, info: WpImageDescriptionInfoV1, desc: ImageDescription) {
        tracing::info!(
            "image_description_info: tf={:?} primaries={:?} luminances={:?} \
             mastering_luminance={:?} scrgb={} bt2100={}",
            desc.transfer,
            desc.primaries.named,
            desc.luminances,
            desc.mastering_luminance,
            desc.windows_scrgb,
            desc.windows_bt2100,
        );
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
