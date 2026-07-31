   use super::*;

    #[test]
    fn default_hdr_color_state_has_expected_fields() {
        let state = default_hdr_color_state();
        assert_eq!(state.colorspace, Colorspace::Bt2020Rgb);
        let meta = state.hdr_metadata.expect("hdr_metadata should be Some");
        assert_eq!(meta.eotf, Eotf::SmpteSt2084);
        assert_eq!(state.max_bpc, Some(10));

        // Primaries are BT.2020
        assert_eq!(meta.display_primaries[0], CtaCoordinate::BT2020_RED);
        assert_eq!(meta.display_primaries[1], CtaCoordinate::BT2020_GREEN);
        assert_eq!(meta.display_primaries[2], CtaCoordinate::BT2020_BLUE);

        // White point is D65
        assert_eq!(meta.white_point, CtaCoordinate::D65_WHITE);
    }
