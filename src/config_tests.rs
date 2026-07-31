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
        assert_eq!(bind_count, 22);
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
