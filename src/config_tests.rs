    use super::*;

    // The built-in default must always parse and resolve fully. This guards the
    // TOML variant strings against drift from the NavAction enum: a rename that
    // desyncs the two would be silently dropped by resolve()'s filter_map, so we
    // assert every bind survives rather than trusting the count implicitly.
    #[test]
    fn default_config_parses_and_resolves_every_bind() {
        let raw: RawConfig = toml::from_str(DEFAULT_CONFIG).expect("default config parses");
        let bind_count = raw.keybinds.len();
        let cfg = Config::resolve(raw);
        assert_eq!(cfg.visible_columns, 3);
        assert_eq!(cfg.keybinds.len(), bind_count, "a keybind was dropped during resolution");
        assert_eq!(bind_count, 26);
    }

    // ---- input ----

    #[test]
    fn config_omitting_input_section_defaults_focus_follows_mouse_off() {
        let text = r#"
            [layout]
            visible_columns = 3

            [keybinds]
            "Alt+h" = "RotateColumnsLeft"
        "#;
        let raw: RawConfig = toml::from_str(text).expect("config without [input] still parses");
        let cfg = Config::resolve(raw);
        assert!(!cfg.focus_follows_mouse, "click-to-focus is the default");
    }

    #[test]
    fn input_section_enables_focus_follows_mouse() {
        let text = r#"
            [layout]
            visible_columns = 3

            [input]
            focus_follows_mouse = true

            [keybinds]
            "Alt+h" = "RotateColumnsLeft"
        "#;
        let raw: RawConfig = toml::from_str(text).expect("config with [input] parses");
        let cfg = Config::resolve(raw);
        assert!(cfg.focus_follows_mouse);
    }

    // ---- animation duration ----

    #[test]
    fn default_config_resolves_animation_duration_to_250ms() {
        let raw: RawConfig = toml::from_str(DEFAULT_CONFIG).expect("default config parses");
        let cfg = Config::resolve(raw);
        assert_eq!(cfg.animation_duration, std::time::Duration::from_millis(250));
    }

    #[test]
    fn config_omitting_animation_section_defaults_to_250ms() {
        let text = r#"
            [layout]
            visible_columns = 3

            [keybinds]
            "Alt+h" = "RotateColumnsLeft"
        "#;
        let raw: RawConfig = toml::from_str(text).expect("config without [animation] still parses");
        let cfg = Config::resolve(raw);
        assert_eq!(cfg.animation_duration, std::time::Duration::from_millis(250));
    }

    #[test]
    fn chord_parses_modifiers_and_keysym() {
        let kb = parse_chord("Alt+Return", NavAction::MoveToNewColumn).unwrap();
        assert!(kb.alt && !kb.logo && !kb.ctrl && !kb.shift);
        assert_eq!(kb.keysym, keysym_from_name("Return", KEYSYM_NO_FLAGS).raw());
    }

    #[test]
    fn unknown_key_drops_the_bind() {
        assert!(parse_chord("Alt+Nonsense", NavAction::MoveToNewColumn).is_none());
    }

    // ---- output config ----

    #[test]
    fn two_output_entries_resolve_with_position_and_mode() {
        let text = r#"
            [layout]
            visible_columns = 3

            [keybinds]
            "Alt+h" = "RotateColumnsLeft"

            [[output]]
            name = "DP-3"
            position = [0, 0]
            mode = "1920x1080"
            primary = true

            [[output]]
            name = "HDMI-A-1"
            position = [1920, 0]
            mode = "1280x400"
        "#;
        let raw: RawConfig = toml::from_str(text).expect("config with [[output]] parses");
        let cfg = Config::resolve(raw);
        assert_eq!(cfg.outputs.len(), 2);

        let dp3 = cfg.outputs.iter().find(|o| o.name == "DP-3").unwrap();
        assert_eq!(dp3.position, (0, 0));
        assert_eq!(dp3.mode, Some((1920, 1080)));
        assert!(dp3.primary);

        let hdmi = cfg.outputs.iter().find(|o| o.name == "HDMI-A-1").unwrap();
        assert_eq!(hdmi.position, (1920, 0));
        assert_eq!(hdmi.mode, Some((1280, 400)));
        assert!(!hdmi.primary);
    }

    #[test]
    fn config_omitting_output_section_resolves_to_empty_vec() {
        let text = r#"
            [layout]
            visible_columns = 3

            [keybinds]
            "Alt+h" = "RotateColumnsLeft"
        "#;
        let raw: RawConfig = toml::from_str(text).expect("config without [[output]] still parses");
        let cfg = Config::resolve(raw);
        assert!(cfg.outputs.is_empty());
    }

    #[test]
    fn malformed_output_mode_is_dropped_without_failing_parse() {
        let text = r#"
            [layout]
            visible_columns = 3

            [keybinds]
            "Alt+h" = "RotateColumnsLeft"

            [[output]]
            name = "DP-3"
            position = [0, 0]
            mode = "not-a-mode"
        "#;
        let raw: RawConfig = toml::from_str(text).expect("config still parses despite bad mode string");
        let cfg = Config::resolve(raw);
        assert_eq!(cfg.outputs.len(), 1);
        assert_eq!(cfg.outputs[0].mode, None);
    }

    #[test]
    fn transform_string_270_resolves_to_270() {
        let text = r#"
            [layout]
            visible_columns = 3

            [keybinds]
            "Alt+h" = "RotateColumnsLeft"

            [[output]]
            name = "HDMI-A-1"
            position = [0, 0]
            transform = "270"
        "#;
        let raw: RawConfig = toml::from_str(text).expect("config with transform parses");
        let cfg = Config::resolve(raw);
        assert_eq!(cfg.outputs[0].transform, Transform::_270);
    }

    #[test]
    fn transform_aliases_left_and_right_map_to_90_and_270() {
        let text = r#"
            [layout]
            visible_columns = 3

            [keybinds]
            "Alt+h" = "RotateColumnsLeft"

            [[output]]
            name = "DP-3"
            position = [0, 0]
            transform = "left"

            [[output]]
            name = "HDMI-A-1"
            position = [1920, 0]
            transform = "right"
        "#;
        let raw: RawConfig = toml::from_str(text).expect("config with transform aliases parses");
        let cfg = Config::resolve(raw);
        let dp3 = cfg.outputs.iter().find(|o| o.name == "DP-3").unwrap();
        assert_eq!(dp3.transform, Transform::_90);
        let hdmi = cfg.outputs.iter().find(|o| o.name == "HDMI-A-1").unwrap();
        assert_eq!(hdmi.transform, Transform::_270);
    }

    #[test]
    fn omitted_transform_defaults_to_normal() {
        let text = r#"
            [layout]
            visible_columns = 3

            [keybinds]
            "Alt+h" = "RotateColumnsLeft"

            [[output]]
            name = "DP-3"
            position = [0, 0]
        "#;
        let raw: RawConfig = toml::from_str(text).expect("config without transform still parses");
        let cfg = Config::resolve(raw);
        assert_eq!(cfg.outputs[0].transform, Transform::Normal);
    }

    #[test]
    fn garbage_transform_falls_back_to_normal_without_failing_parse() {
        let text = r#"
            [layout]
            visible_columns = 3

            [keybinds]
            "Alt+h" = "RotateColumnsLeft"

            [[output]]
            name = "DP-3"
            position = [0, 0]
            transform = "sideways"
        "#;
        let raw: RawConfig = toml::from_str(text).expect("config still parses despite bad transform string");
        let cfg = Config::resolve(raw);
        assert_eq!(cfg.outputs[0].transform, Transform::Normal);
    }

    // ---- hdr ----

    #[test]
    fn output_with_hdr_true_resolves_to_hdr_true() {
        let text = r#"
            [layout]
            visible_columns = 3

            [keybinds]
            "Alt+h" = "RotateColumnsLeft"

            [[output]]
            name = "DP-3"
            position = [0, 0]
            hdr = true
        "#;
        let raw: RawConfig = toml::from_str(text).expect("config with hdr parses");
        let cfg = Config::resolve(raw);
        assert!(cfg.outputs[0].hdr);
    }

    #[test]
    fn output_omitting_hdr_defaults_to_false() {
        let text = r#"
            [layout]
            visible_columns = 3

            [keybinds]
            "Alt+h" = "RotateColumnsLeft"

            [[output]]
            name = "DP-3"
            position = [0, 0]
        "#;
        let raw: RawConfig = toml::from_str(text).expect("config without hdr still parses");
        let cfg = Config::resolve(raw);
        assert!(!cfg.outputs[0].hdr);
    }

    // ---- hot-reload event filtering ----

    use calloop_notify::notify::{
        event::{CreateKind, DataChange, EventKind, MetadataKind, ModifyKind},
        Event,
    };
    use std::ffi::OsStr;

    fn event(kind: EventKind, path: &str) -> Event {
        Event::new(kind).add_path(PathBuf::from(path))
    }

    #[test]
    fn reload_fires_on_content_write_to_the_config() {
        let e = event(EventKind::Modify(ModifyKind::Data(DataChange::Any)), "/cfg/config.toml");
        assert!(should_reload(&e, OsStr::new("config.toml")));
    }

    #[test]
    fn reload_fires_on_create_of_the_config() {
        let e = event(EventKind::Create(CreateKind::File), "/cfg/config.toml");
        assert!(should_reload(&e, OsStr::new("config.toml")));
    }

    // The atime-feedback guard: our own read bumps access time, surfacing as a
    // metadata-only Modify. Reacting to it would loop, so it must be dropped.
    #[test]
    fn reload_ignores_bare_metadata_touch() {
        let e = event(EventKind::Modify(ModifyKind::Metadata(MetadataKind::AccessTime)), "/cfg/config.toml");
        assert!(!should_reload(&e, OsStr::new("config.toml")));
    }

    #[test]
    fn reload_ignores_events_for_other_files() {
        let e = event(EventKind::Modify(ModifyKind::Data(DataChange::Any)), "/cfg/other.toml");
        assert!(!should_reload(&e, OsStr::new("config.toml")));
    }

    // ---- decoration ----

    fn decoration_from(toml_body: &str) -> DecorationConfig {
        let raw: RawDecoration = toml::from_str(toml_body).expect("decoration section parses");
        resolve_decoration(raw)
    }

    #[test]
    fn config_omitting_decoration_section_gets_the_built_in_border_defaults() {
        let raw: RawConfig = toml::from_str(DEFAULT_CONFIG).expect("default config parses");
        let cfg = Config::resolve(raw);
        assert_eq!(cfg.decoration.border_width, 2);
        assert!(cfg.decoration.active.color != cfg.decoration.inactive.color);
    }

    #[test]
    fn hex_colors_parse_in_all_three_lengths() {
        let long = parse_color("#8040c0").expect("6-digit parses");
        assert!((long.r() - 0x80 as f32 / 255.0).abs() < 1e-6);
        assert!((long.b() - 0xc0 as f32 / 255.0).abs() < 1e-6);
        assert_eq!(long.a(), 1.0, "6 digits means fully opaque");

        // Each digit doubles: #84c is #8844cc, not a truncation of #8040c0.
        assert_eq!(parse_color("#84c"), parse_color("#8844cc"));

        let with_alpha = parse_color("#8040c080").expect("8-digit parses");
        assert!(with_alpha.a() < 1.0, "the 8th digit pair is alpha");
    }

    #[test]
    fn the_leading_hash_is_optional() {
        assert_eq!(parse_color("89b4fa"), parse_color("#89b4fa"));
    }

    #[test]
    fn malformed_colors_are_rejected_rather_than_silently_accepted() {
        assert!(parse_color("#12345").is_none(), "5 digits is not a color");
        assert!(parse_color("#gggggg").is_none(), "non-hex digits");
        assert!(parse_color("").is_none());
    }

    #[test]
    fn an_unparseable_color_falls_back_instead_of_failing_the_whole_config() {
        let deco = decoration_from(r##"active_color = "not-a-color""##);
        assert_eq!(deco.active.color, parse_color("#89b4fa").unwrap());
    }

    // Zero means "no opinion", not "make the border black" -- see resolve_luminance.
    #[test]
    fn non_positive_luminance_is_read_as_no_opinion() {
        let deco = decoration_from("active_luminance_nits = 0.0\ninactive_luminance_nits = -5.0");
        assert_eq!(deco.active.luminance_nits, None);
        assert_eq!(deco.inactive.luminance_nits, None);
    }

    #[test]
    fn a_window_matching_no_rule_takes_the_section_defaults() {
        let deco = decoration_from(r##"
            active_color = "#111111"
            inactive_color = "#222222"
            [[rule]]
            app_id = "signal"
            active_color = "#333333"
        "##);
        let style = deco.style_for(Some("firefox"), None, true);
        assert_eq!(style.color, parse_color("#111111").unwrap());
    }

    #[test]
    fn app_id_matching_is_case_insensitive_and_substring() {
        let deco = decoration_from(r##"
            [[rule]]
            app_id = "signal"
            active_color = "#333333"
        "##);
        let style = deco.style_for(Some("org.SIGNAL.Desktop"), None, true);
        assert_eq!(style.color, parse_color("#333333").unwrap());
    }

    #[test]
    fn a_rule_setting_only_active_does_not_take_over_the_inactive_style() {
        let deco = decoration_from(r##"
            inactive_color = "#222222"
            [[rule]]
            app_id = "signal"
            active_color = "#333333"
        "##);
        let inactive = deco.style_for(Some("signal"), None, false);
        assert_eq!(
            inactive.color,
            parse_color("#222222").unwrap(),
            "an active-only rule must fall through for the unfocused case"
        );
    }

    #[test]
    fn the_first_matching_rule_wins() {
        let deco = decoration_from(r##"
            [[rule]]
            app_id = "signal"
            active_color = "#333333"
            [[rule]]
            app_id = "signal"
            active_color = "#444444"
        "##);
        assert_eq!(
            deco.style_for(Some("signal"), None, true).color,
            parse_color("#333333").unwrap()
        );
    }

    #[test]
    fn a_rule_with_both_matchers_requires_both_to_match() {
        let deco = decoration_from(r##"
            active_color = "#111111"
            [[rule]]
            app_id = "firefox"
            title = "youtube"
            active_color = "#333333"
        "##);
        assert_eq!(
            deco.style_for(Some("firefox"), Some("YouTube - Home"), true).color,
            parse_color("#333333").unwrap()
        );
        assert_eq!(
            deco.style_for(Some("firefox"), Some("Hacker News"), true).color,
            parse_color("#111111").unwrap(),
            "title must also match"
        );
    }

    #[test]
    fn a_rule_with_no_matchers_acts_as_a_catch_all() {
        let deco = decoration_from(r##"
            active_color = "#111111"
            [[rule]]
            active_color = "#333333"
        "##);
        assert_eq!(
            deco.style_for(None, None, true).color,
            parse_color("#333333").unwrap()
        );
    }

    #[test]
    fn a_rule_matcher_does_not_match_a_window_with_no_app_id() {
        let deco = decoration_from(r##"
            active_color = "#111111"
            [[rule]]
            app_id = "signal"
            active_color = "#333333"
        "##);
        assert_eq!(
            deco.style_for(None, None, true).color,
            parse_color("#111111").unwrap()
        );
    }

    #[test]
    fn per_rule_luminance_is_carried_through_to_the_resolved_style() {
        let deco = decoration_from(r##"
            [[rule]]
            app_id = "signal"
            active_color = "#333333"
            active_luminance_nits = 600.0
        "##);
        assert_eq!(deco.style_for(Some("signal"), None, true).luminance_nits, Some(600.0));
    }
