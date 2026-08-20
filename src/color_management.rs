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
use smithay::reexports::wayland_server::{DisplayHandle, Resource};
use smithay::wayland::color::management::{
    get_surface_description, Chromaticities, ColorManagementHandler,
    ColorManagementState, Feature, ImageDescription, Primaries, PrimariesOption, TransferFunction,
};

use smithay::wayland::seat::WaylandFocus;

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
/// description, enables its HDR setting, then submits HDR-range content without
/// tagging its own surface. We then drive the connector to SDR underneath it.
/// Nothing errors; the picture is simply wrong, and the log said nothing at all.
///
/// The original diagnosis -- that the client had no advertised way to tag itself --
/// no longer holds: `init` now advertises `Feature::WindowsScrgb` and
/// `Feature::WindowsBt2100`, the two requests DXGI titles reach for. An untagged
/// surface today means the client had a way to speak and did not use it, most
/// likely because it never bound `wp_color_manager_v1` at all. The condition is
/// still worth reporting; only the explanation changed.
pub fn note_untagged_fullscreen(surface: &WlSurface, output_name: &str) {
    if surface_description_present(surface) {
        return;
    }
    if WARNED_UNTAGGED_FULLSCREEN.swap(true, Ordering::Relaxed) {
        return;
    }
    tracing::warn!(
        "fullscreen surface on HDR-capable output {output_name} committed NO color-management \
         image description -- treating as SDR and dropping the connector out of HDR. Rubix \
         advertises WindowsScrgb and WindowsBt2100, so a client that believes it is producing \
         HDR had a way to say so and did not use it -- most likely it never bound \
         wp_color_manager_v1."
    );
}

/// PQ/BT.2020 image description advertised on HDR-enabled outputs so HDR-aware
/// clients detect display HDR headroom. `ref_white` ties the advertised
/// reference white to the live SDR-white slider so SDR content sits at the same
/// level clients expect.
fn hdr_output_description(ref_white: u32, lum: crate::edid::HdrLuminance) -> ImageDescription {
    ImageDescription {
        transfer: TransferFunction::St2084Pq,
        primaries: PrimariesOption { named: Some(Primaries::Bt2020), values: None },
        max_cll: None,
        max_fall: None,
        mastering_luminance: None,
        mastering_primaries: None,
        // (min 0.0001 cd/m², max cd/m², reference white cd/m²). Peak and black
        // come from the display's own EDID (see `crate::edid`); ref white
        // follows the slider.
        //
        // This was a hardcoded 1000-nit peak for every HDR output. Clients use
        // `target_luminance` to decide how hard to tone-map, so on a panel that
        // does 604 that told a well-behaved client to grade for a display 65%
        // brighter than the real one -- and everything above the true peak
        // clipped to flat white, with us having said it was fine.
        luminances: Some((lum.min_0001_cd_m2, lum.max_cd_m2, ref_white)),
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
        //
        // The exact set gamescope 3.16.25 requires before it will use wp color
        // management at all (WaylandBackend.cpp, the
        // bSupportsGamescopeColorManagement lambda): Parametric, SetPrimaries,
        // SetMasteringDisplayPrimaries, ExtendedTargetVolume, SetLuminances,
        // WindowsScrgb, plus TF St2084Pq and primaries Srgb + Bt2020. Any one
        // missing and SupportsColorManagement() is false, which skips the
        // assignment to bExposeHDRSupport entirely -- so correct luminances
        // reach it and are simply never acted on. That is not a graceful
        // degradation path; it is all or nothing.
        //
        // SetPrimaries (custom chromaticities instead of a named set) and
        // ExtendedTargetVolume (target volume exceeding the primary volume) are
        // accepted on the same terms as the mastering metadata above: Rubix does
        // not tone-map, so content passes through and the panel clips.
        // `surface_decode_kind` keys off the transfer function and the Windows
        // flags, so custom primaries on PQ content decode through the BT.2020
        // path -- a small inaccuracy, against no HDR at all.
        [
            Feature::WindowsScrgb,
            Feature::WindowsBt2100,
            Feature::SetLuminances,
            Feature::SetPrimaries,
            Feature::SetMasteringDisplayPrimaries,
            Feature::ExtendedTargetVolume,
        ],
        [],
        |_client| true,
    )
}

/// Send every `wp_image_description_info_v1` data event for `desc`, but NOT
/// `done`.
///
/// Mirrors smithay's `send_image_description_info` with the destructor split
/// off, so the data can go out synchronously during request dispatch while the
/// destructor is deferred as its contract requires. See the call site for why
/// the ordering matters.
///
/// Returns whether anything was actually sent -- the caller logs a failure,
/// because "we were asked" and "we answered" are different facts and only the
/// second one matters.
fn send_info_events(info: &WpImageDescriptionInfoV1, desc: &ImageDescription) -> bool {
    if !info.is_alive() {
        return false;
    }
    let container = Chromaticities::from_option(desc.primaries);
    if let Some(p) = container {
        info.primaries(p.red.0, p.red.1, p.green.0, p.green.1, p.blue.0, p.blue.1, p.white.0, p.white.1);
    }
    if let Some(named) = desc.primaries.named {
        info.primaries_named(named);
    }
    info.tf_named(desc.transfer);

    // Always sent: clients read the reference white from here. gamescope's
    // `m_uReferenceLuminance` is this event's third field.
    let (min_lum, max_lum, reference_lum) = desc.luminances_or_default();
    info.luminances(min_lum, max_lum, reference_lum);

    let target = desc
        .mastering_primaries
        .unwrap_or(container.unwrap_or(Chromaticities::from_named(Primaries::Srgb)));
    info.target_primaries(
        target.red.0, target.red.1, target.green.0, target.green.1,
        target.blue.0, target.blue.1, target.white.0, target.white.1,
    );

    // Also always sent: without mastering luminances the target volume takes the
    // primary volume's range. gamescope's `m_uMaxTargetLuminance` is this
    // event's second field, and `bExposeHDRSupport` is precisely
    // `max_target > reference` -- so with (50, 1000) against a reference of 203
    // this is what flips HDR on for a nested game.
    let (target_min, target_max) = desc.mastering_luminance.unwrap_or((min_lum, max_lum));
    info.target_luminance(target_min, target_max);

    if let Some(max_cll) = desc.max_cll {
        info.target_max_cll(max_cll);
    }
    if let Some(max_fall) = desc.max_fall {
        info.target_max_fall(max_fall);
    }
    true
}

impl ColorManagementHandler for RubixState {
    fn color_management_state(&mut self) -> &mut ColorManagementState {
        &mut self.color_management_state
    }

    // `image_description_changed` keeps the trait's default body: the render
    // path reads the surface's OWN description every frame via
    // `surface_decode_kind` rather than reacting to a changed-surface
    // notification.

    /// What a client should send for best results on the output it is on.
    ///
    /// This -- not `description_for_output` -- is how a nested compositor
    /// discovers HDR headroom. gamescope goes straight to
    /// `get_surface_feedback` -> `get_preferred` -> `get_information` and never
    /// touches the output object, then computes
    /// `bExposeHDRSupport = max_target_luminance > reference_luminance`
    /// (WaylandBackend.cpp). Leaving this at the trait's sRGB default answered
    /// `luminances(2000, 80, 80)` / `target_luminance(2000, 80)`, so 80 > 80 was
    /// false and it refused to offer HDR to the game -- while our HDR output
    /// description sat on a code path it never called.
    ///
    /// Reporting PQ/BT.2020 here is a hint, not an instruction: SDR clients that
    /// ignore it are unaffected, and `surface_decode_kind` still keys off what a
    /// client actually commits, not off what we suggested.
    fn preferred_description_for_surface(&mut self, surface: &WlSurface) -> ImageDescription {
        // A surface can ask before it is mapped -- gamescope does, immediately
        // after creating it -- so "which output is it on" often has no answer
        // yet. Fall back to the active monitor, which is where a new surface
        // almost always lands, rather than to sRGB: guessing SDR on an HDR
        // display is the failure this whole function exists to fix.
        let output_name = self
            .space
            .elements()
            .find(|w| w.wl_surface().as_deref() == Some(surface))
            .and_then(|w| self.space.outputs_for_element(w).into_iter().next())
            .or_else(|| self.active_monitor_output())
            .map(|o| o.name());

        let is_hdr = output_name
            .as_deref()
            .is_some_and(|name| self.hdr_outputs.contains(name));
        tracing::info!(
            "preferred_description_for_surface(output {:?}) -> {}",
            output_name,
            if is_hdr { "PQ/BT.2020 HDR" } else { "sRGB" },
        );
        if is_hdr {
            // The panel's own range, or the fallback if its EDID said nothing.
            let luminance = output_name
                .as_deref()
                .and_then(|name| self.hdr_luminance.get(name).copied())
                .unwrap_or(crate::edid::HdrLuminance::FALLBACK);
            hdr_output_description(
                self.sdr_white_nits.round().clamp(1.0, 10_000.0) as u32,
                luminance,
            )
        } else {
            ImageDescription::SRGB
        }
    }

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
            let luminance = self
                .hdr_luminance
                .get(output.name().as_str())
                .copied()
                .unwrap_or(crate::edid::HdrLuminance::FALLBACK);
            hdr_output_description(
                self.sdr_white_nits.round().clamp(1.0, 10_000.0) as u32,
                luminance,
            )
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
        // Split deliberately: the DATA events go out now, synchronously, and
        // only `done` is deferred.
        //
        // smithay's `send_image_description_info` sends both, and its contract
        // requires deferring the whole thing, because `done` is a destructor
        // event and destroying the object inside the dispatch callback that
        // created it is a use-after-free. But deferring the data too is wrong
        // for a client that does get_information followed by a roundtrip: the
        // reply to `wl_display.sync` is written DURING dispatch, so an idle
        // callback puts our events after it and the roundtrip returns before
        // any of them arrive.
        //
        // That is exactly what gamescope does. It read its uninitialised
        // defaults (uMaxLum/uRefLum = 80/80), concluded the display had no HDR
        // headroom, and refused to expose HDR to the game -- while our log
        // showed us sending luminances (50, 1000, 203) 135ms later, to nobody.
        //
        // Sending the data first is safe: none of these are destructors. The
        // per-event client callbacks fire as they arrive, so the values land
        // before the roundtrip completes even though `done` follows it.
        let sent = send_info_events(&info, &desc);
        if !sent {
            tracing::warn!("image_description_info object was already dead; nothing sent");
        }
        self.loop_handle.insert_idle(move |_state| {
            if info.is_alive() {
                info.done();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hdr_output_description_is_pq_bt2020_with_ref_white() {
        let desc = hdr_output_description(203, crate::edid::HdrLuminance::FALLBACK);
        assert_eq!(desc.transfer, TransferFunction::St2084Pq);
        assert_eq!(desc.primaries.named, Some(Primaries::Bt2020));
        assert_eq!(desc.luminances, Some((50, 1000, 203)));
    }
}
