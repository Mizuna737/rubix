//! HDR static metadata: packs the Linux DRM uAPI `hdr_output_metadata` struct
//! (see `include/uapi/drm/drm_mode.h`) into the exact byte layout the kernel
//! expects for the `HDR_OUTPUT_METADATA` connector property blob.
//!
//! This module is pure (no DRM/hardware access) so it's fully unit-testable.
//! The DRM-path consumer lives in `src/udev.rs`, gated on `OutputConfig::hdr`.
//!
//! Kernel layout (native C struct layout, no `#[repr(packed)]`):
//!
//! ```c
//! struct hdr_metadata_infoframe {
//!     __u8 eotf;
//!     __u8 metadata_type;
//!     struct { __u16 x, y; } display_primaries[3];
//!     struct { __u16 x, y; } white_point;
//!     __u16 max_display_mastering_luminance;
//!     __u16 min_display_mastering_luminance;
//!     __u16 max_cll;
//!     __u16 max_fall;
//! }; // size 26, align 2
//!
//! struct hdr_output_metadata {
//!     __u32 metadata_type;
//!     union { struct hdr_metadata_infoframe hdmi_metadata_type1; };
//! }; // size 32 (2 bytes trailing pad after the union), align 4
//! ```
//!
//! Byte offsets within the 32-byte blob:
//! - `0..4`   metadata_type (u32, outer -- always 0 = HDMI_STATIC_METADATA_TYPE1)
//! - `4`      eotf (u8)
//! - `5`      metadata_type (u8, inner -- static metadata type 1)
//! - `6..30`  hdr_metadata_infoframe body (display_primaries, white_point, luminance, CLL/FALL)
//! - `30..32` trailing struct padding (zeroed, never read by the kernel)
//!
//! Integers are written native-endian (`to_ne_bytes`); this blob is memory
//! handed directly to the kernel ioctl on the local host, so "native" here
//! means the architecture Rubix actually runs on (x86_64/aarch64, both LE).

/// A chromaticity coordinate in the kernel's fixed-point encoding: units of
/// 0.00002 (i.e. `raw = round(coordinate / 0.00002)`). CIE 1931 x/y values are
/// in `[0, 1]`, so this comfortably fits `u16` (max useful value ~50000).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChromaticityCoord {
    pub x: u16,
    pub y: u16,
}

impl ChromaticityCoord {
    /// Build from CIE 1931 xy floats (e.g. `0.708`), converting to the
    /// kernel's 0.00002-unit fixed point.
    const fn from_xy(x: f64, y: f64) -> Self {
        // const fn can't call f64::round, so add 0.5 and truncate manually.
        let xr = (x * 50_000.0 + 0.5) as u16;
        let yr = (y * 50_000.0 + 0.5) as u16;
        ChromaticityCoord { x: xr, y: yr }
    }
}

/// Friendly-unit parameters for a static HDR metadata (type 1) blob.
/// Mirrors `struct hdr_metadata_infoframe` field-for-field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HdrMetadata {
    /// Transfer function. `2` = SMPTE ST 2084 (PQ); `1` = HLG (Rubix only uses PQ).
    pub eotf: u8,
    /// Static metadata descriptor type. `1` = "type 1" (the only one the
    /// kernel/uAPI currently defines).
    pub metadata_type: u8,
    /// Display's red, green, blue primaries, in CIE 1931 xy, 0.00002 units.
    pub display_primaries: [ChromaticityCoord; 3],
    /// Display's white point, in CIE 1931 xy, 0.00002 units.
    pub white_point: ChromaticityCoord,
    /// Mastering display's max luminance, in whole cd/m^2 (units of 1).
    pub max_display_mastering_luminance: u16,
    /// Mastering display's min luminance, in units of 0.0001 cd/m^2.
    pub min_display_mastering_luminance: u16,
    /// Max content light level, in cd/m^2. `0` = unknown/unspecified.
    pub max_cll: u16,
    /// Max frame-average light level, in cd/m^2. `0` = unknown/unspecified.
    pub max_fall: u16,
}

/// SMPTE ST 2084 (PQ) EOTF value for `hdr_metadata_infoframe.eotf`.
pub const EOTF_ST2084_PQ: u8 = 2;
/// Static metadata descriptor "type 1" for `hdr_metadata_infoframe.metadata_type`.
pub const METADATA_TYPE_STATIC_1: u8 = 1;
/// Outer `hdr_output_metadata.metadata_type`: selects the union arm as
/// `hdmi_metadata_type1` (the only variant the uAPI defines today).
const HDMI_STATIC_METADATA_TYPE1: u32 = 0;

impl Default for HdrMetadata {
    /// BT.2020 primaries + D65 white point, PQ EOTF, a conservative 1000
    /// cd/m^2 / 0.0001 cd/m^2 mastering luminance range, and CLL/FALL left
    /// unknown (0). Good enough as groundwork defaults; a real per-panel
    /// EDID-derived profile is out of scope for this phase.
    fn default() -> Self {
        HdrMetadata {
            eotf: EOTF_ST2084_PQ,
            metadata_type: METADATA_TYPE_STATIC_1,
            // BT.2020 primaries (CIE 1931 xy): R(0.708,0.292) G(0.170,0.797) B(0.131,0.046)
            display_primaries: [
                ChromaticityCoord::from_xy(0.708, 0.292),
                ChromaticityCoord::from_xy(0.170, 0.797),
                ChromaticityCoord::from_xy(0.131, 0.046),
            ],
            // D65 white point (CIE 1931 xy): (0.3127, 0.3290)
            white_point: ChromaticityCoord::from_xy(0.3127, 0.3290),
            max_display_mastering_luminance: 1000,
            min_display_mastering_luminance: 1,
            max_cll: 0,
            max_fall: 0,
        }
    }
}

/// Total byte size of the kernel's `struct hdr_output_metadata` (native C
/// layout: 4-byte `metadata_type` + 26-byte `hdr_metadata_infoframe` + 2
/// bytes trailing struct padding to satisfy the outer struct's 4-byte align).
pub const HDR_OUTPUT_METADATA_SIZE: usize = 32;

/// Byte offset of `hdr_metadata_infoframe` within `hdr_output_metadata`
/// (right after the outer `metadata_type` u32).
const INFOFRAME_OFFSET: usize = 4;

/// Pack an [`HdrMetadata`] into the exact byte layout of the kernel's
/// `struct hdr_output_metadata`, ready to hand to `drmModeCreatePropertyBlob`
/// (or drm-rs's `create_property_blob`) for the `HDR_OUTPUT_METADATA`
/// connector property.
pub fn build_hdr_metadata_blob(params: HdrMetadata) -> Vec<u8> {
    let mut buf = vec![0u8; HDR_OUTPUT_METADATA_SIZE];

    // Outer hdr_output_metadata.metadata_type (u32, offset 0..4): selects the
    // hdmi_metadata_type1 union arm.
    buf[0..4].copy_from_slice(&HDMI_STATIC_METADATA_TYPE1.to_ne_bytes());

    // hdr_metadata_infoframe body starts at INFOFRAME_OFFSET.
    let base = INFOFRAME_OFFSET;
    buf[base] = params.eotf;
    buf[base + 1] = params.metadata_type;

    let mut off = base + 2;
    for coord in &params.display_primaries {
        buf[off..off + 2].copy_from_slice(&coord.x.to_ne_bytes());
        buf[off + 2..off + 4].copy_from_slice(&coord.y.to_ne_bytes());
        off += 4;
    }

    buf[off..off + 2].copy_from_slice(&params.white_point.x.to_ne_bytes());
    buf[off + 2..off + 4].copy_from_slice(&params.white_point.y.to_ne_bytes());
    off += 4;

    buf[off..off + 2].copy_from_slice(&params.max_display_mastering_luminance.to_ne_bytes());
    off += 2;
    buf[off..off + 2].copy_from_slice(&params.min_display_mastering_luminance.to_ne_bytes());
    off += 2;
    buf[off..off + 2].copy_from_slice(&params.max_cll.to_ne_bytes());
    off += 2;
    buf[off..off + 2].copy_from_slice(&params.max_fall.to_ne_bytes());
    off += 2;

    debug_assert_eq!(off, HDR_OUTPUT_METADATA_SIZE - 2, "trailing 2 bytes are struct padding");
    buf
}

#[cfg(test)]
#[path = "hdr_tests.rs"]
mod tests;
