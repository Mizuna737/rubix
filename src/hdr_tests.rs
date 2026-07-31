    use super::*;

    // Tests assume a little-endian host (x86_64/aarch64, what Rubix actually
    // runs on) so `to_le_bytes` doubles as the "native" comparison.

    #[test]
    fn blob_length_matches_kernel_struct_size() {
        let blob = build_hdr_metadata_blob(HdrMetadata::default());
        assert_eq!(blob.len(), HDR_OUTPUT_METADATA_SIZE);
        assert_eq!(blob.len(), 32);
    }

    #[test]
    fn outer_metadata_type_selects_hdmi_type1_at_offset_0() {
        let blob = build_hdr_metadata_blob(HdrMetadata::default());
        assert_eq!(&blob[0..4], &0u32.to_le_bytes());
    }

    #[test]
    fn eotf_byte_lands_at_offset_4() {
        let params = HdrMetadata { eotf: EOTF_ST2084_PQ, ..HdrMetadata::default() };
        let blob = build_hdr_metadata_blob(params);
        assert_eq!(blob[4], EOTF_ST2084_PQ);
        assert_eq!(blob[4], 2);
    }

    #[test]
    fn inner_metadata_type_byte_lands_at_offset_5() {
        let blob = build_hdr_metadata_blob(HdrMetadata::default());
        assert_eq!(blob[5], METADATA_TYPE_STATIC_1);
        assert_eq!(blob[5], 1);
    }

    #[test]
    fn first_display_primary_lands_at_offset_6() {
        let params = HdrMetadata {
            display_primaries: [
                ChromaticityCoord { x: 0x1234, y: 0x5678 },
                ChromaticityCoord { x: 0, y: 0 },
                ChromaticityCoord { x: 0, y: 0 },
            ],
            ..HdrMetadata::default()
        };
        let blob = build_hdr_metadata_blob(params);
        assert_eq!(&blob[6..8], &0x1234u16.to_le_bytes());
        assert_eq!(&blob[8..10], &0x5678u16.to_le_bytes());
    }

    #[test]
    fn bt2020_red_primary_value_is_correct() {
        // 0.708 / 0.00002 = 35400
        let params = HdrMetadata::default();
        assert_eq!(params.display_primaries[0], ChromaticityCoord { x: 35400, y: 14600 });
        let blob = build_hdr_metadata_blob(params);
        assert_eq!(&blob[6..8], &35400u16.to_le_bytes());
        assert_eq!(&blob[8..10], &14600u16.to_le_bytes());
    }

    #[test]
    fn white_point_lands_at_offset_18() {
        let params = HdrMetadata { white_point: ChromaticityCoord { x: 0x1111, y: 0x2222 }, ..HdrMetadata::default() };
        let blob = build_hdr_metadata_blob(params);
        assert_eq!(&blob[18..20], &0x1111u16.to_le_bytes());
        assert_eq!(&blob[20..22], &0x2222u16.to_le_bytes());
    }

    #[test]
    fn max_display_mastering_luminance_lands_at_offset_22() {
        let params = HdrMetadata { max_display_mastering_luminance: 1000, ..HdrMetadata::default() };
        let blob = build_hdr_metadata_blob(params);
        assert_eq!(&blob[22..24], &1000u16.to_le_bytes());
    }

    #[test]
    fn min_display_mastering_luminance_lands_at_offset_24() {
        let params = HdrMetadata { min_display_mastering_luminance: 1, ..HdrMetadata::default() };
        let blob = build_hdr_metadata_blob(params);
        assert_eq!(&blob[24..26], &1u16.to_le_bytes());
    }

    #[test]
    fn max_cll_and_max_fall_land_at_offsets_26_and_28() {
        let params = HdrMetadata { max_cll: 500, max_fall: 200, ..HdrMetadata::default() };
        let blob = build_hdr_metadata_blob(params);
        assert_eq!(&blob[26..28], &500u16.to_le_bytes());
        assert_eq!(&blob[28..30], &200u16.to_le_bytes());
    }

    #[test]
    fn trailing_two_bytes_are_zero_padding() {
        let blob = build_hdr_metadata_blob(HdrMetadata::default());
        assert_eq!(&blob[30..32], &[0u8, 0u8]);
    }

    #[test]
    fn default_metadata_uses_pq_eotf_and_static_type_1() {
        let params = HdrMetadata::default();
        assert_eq!(params.eotf, EOTF_ST2084_PQ);
        assert_eq!(params.metadata_type, METADATA_TYPE_STATIC_1);
        assert_eq!(params.max_cll, 0);
        assert_eq!(params.max_fall, 0);
    }
