//! EDID parsing for HDR display capability.
//!
//! Only one thing is read out of the EDID here: the CTA-861 **HDR Static
//! Metadata Data Block**, which is where a display states how bright it can
//! actually get. That number goes straight into what `wp_color_management_v1`
//! advertises to clients as `target_luminance`, and clients use it to decide how
//! hard to tone-map.
//!
//! Getting it wrong is not cosmetic. Rubix previously advertised a hardcoded
//! 1000-nit peak to every client on every HDR output. On a panel that does 604,
//! that instructs a well-behaved client to grade for a display 65% brighter than
//! the one in front of it, and everything above the real peak clips to flat
//! white -- with the compositor having told the client that was fine.
//!
//! Parsed by hand rather than through `libdisplay-info`: the `display-info`
//! feature of `smithay-drm-extras` is deliberately off (its
//! `libdisplay-info-sys` pins `< 0.3.0` while the system ships 0.3.0 -- see
//! Cargo.toml), and this is one small, well-specified block.

use smithay::reexports::drm::control::{connector, property, Device as ControlDevice};

/// What a display says about its own luminance range, from the EDID.
///
/// Units match `wp_color_management_v1`'s `set_luminances` exactly, so this
/// drops straight in: min in 0.0001 cd/m², max and frame-average in cd/m².
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HdrLuminance {
    /// Peak, cd/m². "Desired content max luminance" in CTA-861 terms.
    pub max_cd_m2: u32,
    /// Sustained full-frame, cd/m². Lower than the peak on essentially every
    /// real panel, because of the brightness limiter. Not advertised over the
    /// protocol today (there is no field for it) but logged, because content
    /// exceeding it is what makes a display dim itself mid-scene.
    pub max_frame_average_cd_m2: u32,
    /// Black level, in units of 0.0001 cd/m².
    pub min_0001_cd_m2: u32,
}

impl HdrLuminance {
    /// Used when a display claims HDR (or the config forces it) but the EDID has
    /// no usable HDR Static Metadata Block. These are the values Rubix
    /// advertised unconditionally before the EDID was consulted, kept as the
    /// fallback so behaviour is unchanged for displays that say nothing.
    pub(crate) const FALLBACK: Self = HdrLuminance {
        max_cd_m2: 1000,
        max_frame_average_cd_m2: 1000,
        min_0001_cd_m2: 50,
    };
}

/// CTA-861 luminance coding: `50 * 2^(code/32)` cd/m².
fn luminance_from_code(code: u8) -> u32 {
    (50.0_f64 * 2.0_f64.powf(code as f64 / 32.0)).round() as u32
}

/// CTA-861 min-luminance coding: `max * (code/255)^2 / 100` cd/m², reported here
/// in the protocol's 0.0001 cd/m² units.
fn min_luminance_0001(code: u8, max_cd_m2: f64) -> u32 {
    let ratio = code as f64 / 255.0;
    ((max_cd_m2 * ratio * ratio / 100.0) * 10_000.0).round() as u32
}

/// Pull the HDR luminance range out of a raw EDID blob.
///
/// `None` means "this display told us nothing usable" -- no CTA-861 extension,
/// no HDR Static Metadata Block, PQ not among its supported EOTFs, or the block
/// present but truncated before the luminance bytes (all three luminance fields
/// are optional in the spec). Callers fall back to [`HdrLuminance::FALLBACK`]
/// rather than dropping HDR.
pub(crate) fn hdr_luminance(edid: &[u8]) -> Option<HdrLuminance> {
    // Base block is 128 bytes; byte 126 counts the extensions that follow.
    if edid.len() < 128 {
        return None;
    }
    let extensions = edid[126] as usize;
    for i in 0..extensions {
        let base = 128 * (i + 1);
        let Some(ext) = edid.get(base..base + 128) else { continue };
        // 0x02 == CTA-861 extension. Anything else (DisplayID, etc.) is skipped.
        if ext[0] != 0x02 {
            continue;
        }
        // Byte 2 is where the detailed timing descriptors begin, i.e. the end of
        // the data block collection. 0..=4 means there is no collection at all.
        let dtd_start = ext[2] as usize;
        if dtd_start <= 4 || dtd_start > 128 {
            continue;
        }
        let mut p = 4usize;
        while p < dtd_start {
            // Tag header: top 3 bits are the tag, bottom 5 the payload length.
            let tag = ext[p] >> 5;
            let len = (ext[p] & 0x1f) as usize;
            let Some(body) = ext.get(p + 1..p + 1 + len) else { break };
            // Tag 7 == "use extended tag", whose first payload byte selects the
            // real block type. 6 == HDR Static Metadata.
            if tag == 7 && body.first() == Some(&6) {
                if let Some(lum) = parse_hdr_static_metadata(&body[1..]) {
                    return Some(lum);
                }
            }
            p += 1 + len;
        }
    }
    None
}

/// The HDR Static Metadata Data Block payload, past its extended-tag byte.
///
/// Layout: `[eotf_bitmap, descriptor_bitmap, max?, frame_avg?, min?]`. Only the
/// first two bytes are mandatory, which is why every luminance field is checked
/// for presence rather than indexed.
fn parse_hdr_static_metadata(payload: &[u8]) -> Option<HdrLuminance> {
    let eotf = *payload.first()?;
    // Bit 2 == SMPTE ST2084 (PQ). A display advertising only traditional gamma
    // and HLG has no PQ peak to report, and claiming one would be a fabrication.
    const EOTF_ST2084_PQ: u8 = 0x04;
    if eotf & EOTF_ST2084_PQ == 0 {
        return None;
    }
    // Luminance fields are optional; without a max there is nothing worth having.
    let max_code = *payload.get(2)?;
    let max_cd_m2 = luminance_from_code(max_code);
    // Frame-average defaults to the peak when absent -- the conservative
    // reading, since assuming a panel can sustain its peak is the safe direction
    // for a value we only log.
    let max_frame_average_cd_m2 = payload.get(3).map_or(max_cd_m2, |c| luminance_from_code(*c));
    let min_0001_cd_m2 = payload
        .get(4)
        .map_or(0, |c| min_luminance_0001(*c, max_cd_m2 as f64));
    Some(HdrLuminance { max_cd_m2, max_frame_average_cd_m2, min_0001_cd_m2 })
}

/// Read a connector's raw EDID blob off its DRM property.
///
/// `None` for a connector with no EDID property or an unreadable blob -- a
/// disconnected or virtual output, typically.
pub(crate) fn read_edid(
    device: &impl ControlDevice,
    connector: connector::Handle,
) -> Option<Vec<u8>> {
    let props = device.get_properties(connector).ok()?;
    let (handles, raw_values) = props.as_props_and_values();
    for (handle, raw) in handles.iter().zip(raw_values.iter()) {
        let Ok(info) = device.get_property(*handle) else { continue };
        if info.name().to_str() != Ok("EDID") {
            continue;
        }
        if let property::Value::Blob(blob) = info.value_type().convert_value(*raw) {
            return device.get_property_blob(blob).ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // A real EDID, from an LG UltraGear on DP-3. edid-decode reports:
    //   Desired content max luminance:             115 (603.666 cd/m^2)
    //   Desired content max frame-average:          79 (276.782 cd/m^2)
    //   Desired content min luminance:               1 (0.000 cd/m^2)
    // Kept verbatim so the parser is pinned against a display that exists rather
    // than against a fixture written to match the parser.
    const LG_ULTRAGEAR_DP3: &str = "00ffffffffffff001e6d9a9ebc37070009220104c5682c78f95da5ad523faf250e5054210900d1c061404540010101010101010101014ed470a0d0a0465030203a0013b44100001a000000fd0c30f091918c010a202020202020000000fc004c4720554c545241474541522b000000ff003430394e544a4a44583032300a028b02032f73230f5707834f0000e305c000e6060501734f01e2006a741a0000030330f000a073014f02f0000000000000e77c70a0d0a0295030203a0013b44100001a9d6770a0d0a0225030203a0013b44100001a00000000000000000000000000000000000000000000000000000000000000000000000000000000000000009b701279030001000cbe280811700da0059078ffbb0f000aa4140e0e0701230000000301509a2202886f0d9f0007801f009f05b20051000700903801086f0d9f0007801f009f0566002b0007006e0101086f0d9f0007801f009f05540022000700936f00087f079f0007801f0037043f001700070000000000000000000000cc90";

    // Also real: a 1280x400 HDMI info strip. Has a CTA-861 extension but no HDR
    // block at all -- the negative case, from hardware rather than imagination.
    const SDR_STRIP_HDMI: &str = "00ffffffffffff0067136666000000001e20010482000078eede50a3544c99260f505400000001010101010101010101010101010101e410908210005050321ee4050ac000000018e410908210005050321ee4050ac000000018e410908210005050321ee4050ac000000018000000fc00595820446973706c61790a2020014102030e4223097f0765030c001000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000075";

    fn edid(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn reads_the_real_panels_luminance_range() {
        let lum = hdr_luminance(&edid(LG_ULTRAGEAR_DP3)).expect("DP-3 advertises HDR");
        // edid-decode says 603.666 and 276.782; we round.
        assert_eq!(lum.max_cd_m2, 604);
        assert_eq!(lum.max_frame_average_cd_m2, 277);
        // 603.666 * (1/255)^2 / 100 = 9.28e-5 cd/m^2 -> 0.93 in 0.0001 units.
        assert_eq!(lum.min_0001_cd_m2, 1);
    }

    // The whole point of the change: what we used to advertise was far above
    // what this panel can do. If the fallback ever equals the real value, this
    // test has stopped proving anything.
    #[test]
    fn the_old_hardcoded_value_really_was_wrong_for_this_panel() {
        let lum = hdr_luminance(&edid(LG_ULTRAGEAR_DP3)).unwrap();
        assert!(
            lum.max_cd_m2 < HdrLuminance::FALLBACK.max_cd_m2,
            "fallback {} should overstate this panel's {}",
            HdrLuminance::FALLBACK.max_cd_m2,
            lum.max_cd_m2
        );
    }

    #[test]
    fn an_sdr_display_reports_nothing() {
        assert_eq!(hdr_luminance(&edid(SDR_STRIP_HDMI)), None);
    }

    #[test]
    fn garbage_and_truncation_are_not_panics() {
        assert_eq!(hdr_luminance(&[]), None);
        assert_eq!(hdr_luminance(&[0u8; 127]), None);
        // Claims 4 extensions, supplies none.
        let mut truncated = [0u8; 128];
        truncated[126] = 4;
        assert_eq!(hdr_luminance(&truncated), None);
        // Full EDID cut off mid-extension.
        let full = edid(LG_ULTRAGEAR_DP3);
        for cut in [129, 200, 255, 300] {
            let _ = hdr_luminance(&full[..cut]);
        }
    }

    // A block that says "traditional gamma and HLG" but not PQ has no PQ peak to
    // report. Claiming one would invent a number the display never gave us.
    #[test]
    fn a_display_without_pq_reports_nothing_even_with_luminance_bytes() {
        // eotf bitmap 0b0000_1010 = traditional gamma (bit 0 clear) + HLG, no PQ.
        let payload = [0b0000_1010u8, 0x01, 115, 79, 1];
        assert_eq!(parse_hdr_static_metadata(&payload), None);
        // Same block with PQ set parses fine, so the bitmap is what decided it.
        let payload = [0b0000_1110u8, 0x01, 115, 79, 1];
        assert!(parse_hdr_static_metadata(&payload).is_some());
    }

    // Every luminance byte past the first two is optional in CTA-861.
    #[test]
    fn optional_luminance_fields_degrade_rather_than_panic() {
        assert_eq!(parse_hdr_static_metadata(&[0x04, 0x01]), None, "no max, nothing to say");
        let only_max = parse_hdr_static_metadata(&[0x04, 0x01, 115]).unwrap();
        assert_eq!(only_max.max_cd_m2, 604);
        assert_eq!(
            only_max.max_frame_average_cd_m2, 604,
            "absent frame-average falls back to the peak"
        );
        assert_eq!(only_max.min_0001_cd_m2, 0);
    }

    #[test]
    fn the_cta_luminance_curve_matches_the_spec() {
        // 50 * 2^(code/32): 0 -> 50, 32 -> 100, 64 -> 200, 96 -> 400.
        assert_eq!(luminance_from_code(0), 50);
        assert_eq!(luminance_from_code(32), 100);
        assert_eq!(luminance_from_code(64), 200);
        assert_eq!(luminance_from_code(96), 400);
    }
}
