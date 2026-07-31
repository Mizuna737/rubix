//! HDR color state builder for the fork's typed metadata API.
//!
//! Replaced the hand-rolled byte-packing with a function that returns the
//! fork's typed `ConnectorColorState` built from our existing default constants.

use smithay::backend::drm::{
    Colorspace, ConnectorColorState, CtaCoordinate, Eotf, HdrOutputMetadata,
};

/// Produce a default HDR `ConnectorColorState` for an output opted into HDR.
///
/// PQ EOTF (`SmpteSt2084`), BT.2020 RGB colorimetry, BT.2020 primaries with
/// D65 white point, `max_bpc: Some(10)`, and static default mastering luminance
/// (1000 / 0.0001 cd/m^2 units, CLL/FALL = 0). EDID-derived per-panel values
/// are Phase 1b.
pub fn default_hdr_color_state() -> ConnectorColorState {
    let hdr_metadata = HdrOutputMetadata {
        eotf: Eotf::SmpteSt2084,
        display_primaries: [
            CtaCoordinate::BT2020_RED,
            CtaCoordinate::BT2020_GREEN,
            CtaCoordinate::BT2020_BLUE,
        ],
        white_point: CtaCoordinate::D65_WHITE,
        max_display_mastering_luminance: 1000,
        min_display_mastering_luminance: 1,
        max_cll: 0,
        max_fall: 0,
    };

    ConnectorColorState {
        colorspace: Colorspace::Bt2020Rgb,
        hdr_metadata: Some(hdr_metadata),
        max_bpc: Some(10),
    }
}

#[cfg(test)]
#[path = "hdr_tests.rs"]
mod tests;
