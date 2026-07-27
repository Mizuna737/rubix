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
        assert_eq!(bind_count, 17);
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
