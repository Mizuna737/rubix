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

    // A single file holding today's whole config must resolve to exactly the
    // same `Config` through the new multi-file loader as it did being parsed
    // directly -- the no-op property this phase depends on for a safe
    // restart into the existing single-`config.toml` layout.
    #[test]
    fn single_file_through_the_loader_matches_direct_parse() {
        let dir = std::env::temp_dir().join(format!(
            "rubix-config-noop-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        std::fs::write(dir.join("config.toml"), DEFAULT_CONFIG).expect("write config");

        let files = config_loader::discover_config_files(&dir);
        let (table, _) = config_loader::merge_all(&dir, &files).expect("merge succeeds");
        let raw = config_loader::deserialize_merged(table).expect("deserializes");
        let via_loader = Config::resolve(raw);

        let direct = Config::resolve(toml::from_str(DEFAULT_CONFIG).expect("default parses directly"));

        assert_eq!(via_loader.visible_columns, direct.visible_columns);
        assert_eq!(via_loader.outer_gap, direct.outer_gap);
        assert_eq!(via_loader.inner_gap, direct.inner_gap);
        assert_eq!(via_loader.animation_duration, direct.animation_duration);
        assert_eq!(via_loader.startup, direct.startup);
        assert_eq!(via_loader.sdr_white_nits, direct.sdr_white_nits);
        assert_eq!(via_loader.focus_follows_mouse, direct.focus_follows_mouse);
        assert_eq!(via_loader.outputs.len(), direct.outputs.len());
        assert_eq!(via_loader.wallpaper, direct.wallpaper);
        assert_eq!(via_loader.idle, direct.idle);
        assert_eq!(via_loader.keybinds.len(), direct.keybinds.len());
        assert_eq!(via_loader.decoration.border_width, direct.decoration.border_width);
        assert_eq!(via_loader.decoration.corner_radius, direct.decoration.corner_radius);
        assert_eq!(via_loader.decoration.rules.len(), direct.decoration.rules.len());
        assert_eq!(via_loader.decoration.backdrop_luminance_nits, direct.decoration.backdrop_luminance_nits);
        // Keybinds come from a `HashMap`, so insertion/iteration order is not
        // guaranteed to match between two independent parses of the same
        // text -- compare as a sorted multiset of resolved chords instead.
        let sort_key = |k: &Keybind| (k.logo, k.alt, k.ctrl, k.shift, k.keysym);
        let mut via_sorted: Vec<_> = via_loader.keybinds.iter().map(sort_key).collect();
        let mut direct_sorted: Vec<_> = direct.keybinds.iter().map(sort_key).collect();
        via_sorted.sort();
        direct_sorted.sort();
        assert_eq!(via_sorted, direct_sorted);

        std::fs::remove_dir_all(&dir).ok();
    }

    // ---- wallpaper ----

    // A config predating [wallpaper] must keep working, drawing nothing rather
    // than failing to parse.
    #[test]
    fn config_omitting_wallpaper_section_draws_none() {
        let text = r#"
            [layout]
            visible_columns = 3

            [keybinds]
        "#;
        let raw: RawConfig = toml::from_str(text).expect("config without [wallpaper] still parses");
        let cfg = Config::resolve(raw);
        assert_eq!(cfg.wallpaper.path, None);
        assert_eq!(cfg.wallpaper.mode, crate::wallpaper::WallpaperMode::Fill);
    }

    // The mode strings are the PascalCase Rust variant names, matched by serde
    // with no rename -- the same rule the rest of this config follows.
    #[test]
    fn wallpaper_mode_matches_the_rust_variant_names() {
        for (text, expected) in [
            ("Fill", crate::wallpaper::WallpaperMode::Fill),
            ("Fit", crate::wallpaper::WallpaperMode::Fit),
            ("Stretch", crate::wallpaper::WallpaperMode::Stretch),
            ("Center", crate::wallpaper::WallpaperMode::Center),
        ] {
            let toml_text = format!(
                "[layout]\nvisible_columns = 3\n[keybinds]\n[wallpaper]\nmode = \"{text}\"\n"
            );
            let raw: RawConfig = toml::from_str(&toml_text).expect("mode parses");
            assert_eq!(Config::resolve(raw).wallpaper.mode, expected, "{text}");
        }
    }

    // A wallpaper path is the first config value a user naturally writes with a
    // tilde; leaving it unexpanded produces a literal "~" directory lookup that
    // fails with a confusing message.
    #[test]
    fn wallpaper_paths_expand_a_leading_tilde() {
        let text = r#"
            [layout]
            visible_columns = 3

            [keybinds]

            [wallpaper]
            path = "~/pictures/a.avif"

            [[output]]
            name = "DP-3"
            position = [0, 0]
            wallpaper = "~/pictures/b.avif"
        "#;
        let raw: RawConfig = toml::from_str(text).expect("config parses");
        let cfg = Config::resolve(raw);
        let home = std::env::var("HOME").expect("HOME must be set for this test");
        assert_eq!(
            cfg.wallpaper.path,
            Some(PathBuf::from(&home).join("pictures/a.avif")),
        );
        assert_eq!(
            cfg.outputs[0].wallpaper,
            Some(PathBuf::from(&home).join("pictures/b.avif")),
        );
    }

    // A tilde anywhere but the front is a legal filename character, not a home
    // directory -- expanding it would break a path that was already correct.
    #[test]
    fn only_a_leading_tilde_is_expanded() {
        assert_eq!(expand_tilde("/a/~b.avif".into()), PathBuf::from("/a/~b.avif"));
        assert_eq!(expand_tilde("~notauser/a.avif".into()), PathBuf::from("~notauser/a.avif"));
    }

    // An output that names no wallpaper falls back to the global one; the
    // per-output key exists to override, not to be mandatory.
    #[test]
    fn an_output_may_omit_its_wallpaper() {
        let text = r#"
            [layout]
            visible_columns = 3

            [keybinds]

            [[output]]
            name = "DP-3"
            position = [0, 0]
        "#;
        let raw: RawConfig = toml::from_str(text).expect("config parses");
        assert_eq!(Config::resolve(raw).outputs[0].wallpaper, None);
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
        event::{CreateKind, DataChange, EventKind, MetadataKind, ModifyKind, RemoveKind},
        Event,
    };

    fn event(kind: EventKind, path: &str) -> Event {
        Event::new(kind).add_path(PathBuf::from(path))
    }

    #[test]
    fn reload_fires_on_content_write_to_any_toml_file() {
        let e = event(EventKind::Modify(ModifyKind::Data(DataChange::Any)), "/cfg/nested/other.toml");
        assert!(should_reload(&e));
    }

    #[test]
    fn reload_fires_on_create_of_a_toml_file() {
        let e = event(EventKind::Create(CreateKind::File), "/cfg/config.toml");
        assert!(should_reload(&e));
    }

    // Deletion matters too: a file that vanished mid-session should not keep
    // contributing stale settings to the next reload.
    #[test]
    fn reload_fires_on_deletion_of_a_toml_file() {
        let e = event(EventKind::Remove(RemoveKind::File), "/cfg/config.toml");
        assert!(should_reload(&e));
    }

    // The atime-feedback guard: our own read bumps access time, surfacing as a
    // metadata-only Modify. Reacting to it would loop, so it must be dropped.
    #[test]
    fn reload_ignores_bare_metadata_touch() {
        let e = event(EventKind::Modify(ModifyKind::Metadata(MetadataKind::AccessTime)), "/cfg/config.toml");
        assert!(!should_reload(&e));
    }

    // Only `*.toml` is config -- an unrelated file dropped in the (now
    // recursively watched) directory must not trigger a reload.
    #[test]
    fn reload_ignores_events_for_non_toml_files() {
        let e = event(EventKind::Modify(ModifyKind::Data(DataChange::Any)), "/cfg/notes.md");
        assert!(!should_reload(&e));
    }

    // ---- decoration ----

    #[test]
    fn backdrop_luminance_nits_defaults_to_live_sdr_white_nits() {
        // The fallback is window white, and it must track the *clamped*
        // `sdr_white_nits` rather than the raw key.
        let text = r#"
            sdr_white_nits = 250.0

            [layout]
            visible_columns = 3

            [keybinds]
        "#;
        let raw: RawConfig = toml::from_str(text).expect("config parses");
        let cfg = Config::resolve(raw);
        assert_eq!(cfg.sdr_white_nits, 250.0);
        assert_eq!(cfg.decoration.backdrop_luminance_nits, 250.0);
    }

    #[test]
    fn backdrop_luminance_nits_parses_and_clamps() {
        // The floor sits far below `sdr_white_nits`'s own [80, 300] because a
        // useful backdrop ceiling is expected to be well under window white.
        let deco = decoration_from("backdrop_luminance_nits = 40.0\n");
        assert_eq!(deco.backdrop_luminance_nits, 40.0);
        let low = decoration_from("backdrop_luminance_nits = 1.0\n");
        assert_eq!(low.backdrop_luminance_nits, 10.0);
        let high = decoration_from("backdrop_luminance_nits = 999999.0\n");
        assert_eq!(high.backdrop_luminance_nits, 10000.0);
    }

    fn decoration_from(toml_body: &str) -> DecorationConfig {
        let raw: RawDecoration = toml::from_str(toml_body).expect("decoration section parses");
        resolve_decoration(raw, crate::hdr_shaders::SDR_WHITE_NITS)
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

    // Rules accumulate in file order, so a later rule wins for the fields it
    // names -- the "broad rule, narrow exception" shape.
    #[test]
    fn a_later_matching_rule_overrides_an_earlier_one() {
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
            parse_color("#444444").unwrap()
        );
    }

    #[test]
    fn a_later_rule_leaves_fields_it_does_not_name_alone() {
        let deco = decoration_from(r##"
            [[rule]]
            active_color = "#333333"
            active_opacity = 0.5
            [[rule]]
            app_id = "signal"
            active_opacity = 1.0
        "##);
        let style = deco.style_for(Some("signal"), None, true);
        assert_eq!(style.opacity, 1.0, "the narrow rule pins opacity back");
        assert_eq!(
            style.color,
            parse_color("#333333").unwrap(),
            "and leaves the broad rule's colour in place"
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

    // ---- opacity ----

    #[test]
    fn opacity_defaults_to_fully_opaque() {
        let deco = decoration_from("");
        assert_eq!(deco.active.opacity, 1.0);
        assert_eq!(deco.inactive.opacity, 1.0);
    }

    #[test]
    fn opacity_is_clamped_into_range() {
        let deco = decoration_from("active_opacity = 1.7\ninactive_opacity = -0.4");
        assert_eq!(deco.active.opacity, 1.0);
        assert_eq!(deco.inactive.opacity, 0.0);
    }

    #[test]
    fn a_nan_opacity_falls_back_to_opaque_rather_than_poisoning_the_blend() {
        let deco = decoration_from("active_opacity = nan");
        assert_eq!(deco.active.opacity, 1.0);
    }

    #[test]
    fn focused_and_unfocused_opacity_are_independent() {
        let deco = decoration_from("active_opacity = 1.0\ninactive_opacity = 0.85");
        assert_eq!(deco.style_for(None, None, true).opacity, 1.0);
        assert_eq!(deco.style_for(None, None, false).opacity, 0.85);
    }

    #[test]
    fn a_rule_can_set_opacity_per_class() {
        let deco = decoration_from(r##"
            active_opacity = 1.0
            [[rule]]
            app_id = "alacritty"
            active_color = "#89b4fa"
            active_opacity = 0.9
        "##);
        assert_eq!(deco.style_for(Some("Alacritty"), None, true).opacity, 0.9);
        assert_eq!(deco.style_for(Some("firefox"), None, true).opacity, 1.0);
    }

    // Each field of a rule stands on its own: a rule may set opacity without
    // also having to decide the window's border colour. This is the specific
    // trap the override rework removed.
    #[test]
    fn a_rule_can_set_opacity_without_naming_a_colour() {
        let deco = decoration_from(r##"
            active_opacity = 1.0
            active_color = "#111111"
            [[rule]]
            app_id = "alacritty"
            active_opacity = 0.5
        "##);
        let style = deco.style_for(Some("alacritty"), None, true);
        assert_eq!(style.opacity, 0.5);
        assert_eq!(
            style.color,
            parse_color("#111111").unwrap(),
            "an opacity-only rule must not disturb the colour"
        );
    }

    // The case Max actually wants: dim everything, then exempt video by title.
    #[test]
    fn a_title_rule_can_exempt_one_window_from_a_global_dim() {
        let deco = decoration_from(r##"
            active_opacity = 0.95
            inactive_opacity = 0.85
            [[rule]]
            title = "youtube"
            active_opacity = 1.0
            inactive_opacity = 1.0
        "##);
        assert_eq!(
            deco.style_for(Some("firefox"), Some("YouTube - Home"), false).opacity,
            1.0
        );
        assert_eq!(
            deco.style_for(Some("firefox"), Some("Hacker News"), false).opacity,
            0.85
        );
    }

    #[test]
    fn a_rule_inherits_section_opacity_when_it_sets_only_a_colour() {
        let deco = decoration_from(r##"
            active_opacity = 0.8
            [[rule]]
            app_id = "alacritty"
            active_color = "#89b4fa"
        "##);
        assert_eq!(deco.style_for(Some("alacritty"), None, true).opacity, 0.8);
    }

    // ---- backdrop (frosted glass) ----

    #[test]
    fn backdrop_knobs_default_to_off_and_radius_defaults_to_32() {
        let deco = decoration_from("");
        assert!(!deco.active.backdrop_tonemap);
        assert!(!deco.inactive.backdrop_tonemap);
        assert!(!deco.active.backdrop_blur);
        assert!(!deco.inactive.backdrop_blur);
        assert_eq!(deco.backdrop_blur_radius, 32);
    }

    #[test]
    fn backdrop_knobs_parse_and_focus_states_are_independent() {
        let deco = decoration_from(
            "active_backdrop_tonemap = true\ninactive_backdrop_blur = true\nbackdrop_blur_radius = 48",
        );
        assert!(deco.active.backdrop_tonemap);
        assert!(!deco.inactive.backdrop_tonemap);
        assert!(!deco.active.backdrop_blur);
        assert!(deco.inactive.backdrop_blur);
        assert_eq!(deco.backdrop_blur_radius, 48);
    }

    #[test]
    fn backdrop_blur_radius_is_global_not_per_rule() {
        // There is deliberately no `active_backdrop_blur_radius` /
        // `[[decoration.rule]]`-level radius key -- only the section-level one.
        let deco = decoration_from(
            r##"
            backdrop_blur_radius = 64
            [[rule]]
            app_id = "obsidian"
            active_backdrop_blur = true
            "##,
        );
        assert_eq!(deco.backdrop_blur_radius, 64, "radius stays a single global value");
        assert!(deco.style_for(Some("obsidian"), None, true).backdrop_blur);
    }

    #[test]
    fn a_rule_can_enable_backdrop_tonemap_and_blur_independently() {
        let deco = decoration_from(
            r##"
            [[rule]]
            app_id = "obsidian"
            active_backdrop_tonemap = true
            "##,
        );
        let style = deco.style_for(Some("obsidian"), None, true);
        assert!(style.backdrop_tonemap);
        assert!(!style.backdrop_blur, "blur was never asked for by this rule");
    }

    // The same trap opacity-only rules avoid: setting one backdrop field must
    // not disturb the other, nor anything unrelated the rule didn't name.
    #[test]
    fn a_rule_setting_only_backdrop_blur_does_not_disturb_other_fields() {
        let deco = decoration_from(
            r##"
            active_color = "#111111"
            active_opacity = 0.8
            [[rule]]
            app_id = "obsidian"
            active_backdrop_blur = true
            "##,
        );
        let style = deco.style_for(Some("obsidian"), None, true);
        assert!(style.backdrop_blur);
        assert!(!style.backdrop_tonemap);
        assert_eq!(style.color, parse_color("#111111").unwrap());
        assert_eq!(style.opacity, 0.8);
    }

    #[test]
    fn a_later_rule_can_turn_backdrop_tonemap_back_off() {
        let deco = decoration_from(
            r##"
            [[rule]]
            title = "glass"
            active_backdrop_tonemap = true
            [[rule]]
            title = "glass"
            active_backdrop_tonemap = false
            "##,
        );
        assert!(!deco.style_for(None, Some("glass pane"), true).backdrop_tonemap);
    }

    #[test]
    fn style_override_apply_sets_backdrop_fields_independently() {
        let mut style = WindowStyle {
            color: parse_color("#111111").unwrap(),
            luminance_nits: None,
            glow_margin: 0,
            glow_falloff: 2.0,
            opacity: 1.0,
            backdrop_tonemap: false,
            backdrop_blur: false,
        };
        let before = style.clone();
        let over = StyleOverride { backdrop_blur: Some(true), ..StyleOverride::default() };
        over.apply(&mut style);
        assert!(style.backdrop_blur, "the named field must change");
        assert!(!style.backdrop_tonemap, "an unset field must not change");
        assert_eq!(style.color, before.color);
        assert_eq!(style.opacity, before.opacity);
        assert_eq!(style.glow_margin, before.glow_margin);
    }

    // ---- diagnostics ----

    #[test]
    fn config_omitting_diagnostics_section_defaults_to_osd() {
        let text = r#"
            [layout]
            visible_columns = 3

            [keybinds]
            "Alt+h" = "RotateColumnsLeft"
        "#;
        let raw: RawConfig = toml::from_str(text).expect("config without [diagnostics] parses");
        let cfg = Config::resolve(raw);
        assert_eq!(cfg.diagnostics.config_errors, ConfigErrorSink::Osd);
        assert!(cfg.diagnostics.wants_osd());
        assert!(!cfg.diagnostics.wants_ipc());
    }

    #[test]
    fn diagnostics_section_selects_the_sink() {
        for (value, expected) in [
            ("Osd", ConfigErrorSink::Osd),
            ("Ipc", ConfigErrorSink::Ipc),
            ("Both", ConfigErrorSink::Both),
            ("Silent", ConfigErrorSink::Silent),
        ] {
            let text = format!(
                r#"
                [diagnostics]
                config_errors = "{value}"

                [layout]
                visible_columns = 3

                [keybinds]
                "Alt+h" = "RotateColumnsLeft"
                "#
            );
            let raw: RawConfig = toml::from_str(&text).expect("config with [diagnostics] parses");
            let cfg = Config::resolve(raw);
            assert_eq!(cfg.diagnostics.config_errors, expected, "sink {value}");
        }
    }

    #[test]
    fn both_sink_wants_each_channel_and_silent_wants_neither() {
        assert!(ConfigErrorSink::Both.osd() && ConfigErrorSink::Both.ipc());
        assert!(!ConfigErrorSink::Silent.osd() && !ConfigErrorSink::Silent.ipc());
        assert!(ConfigErrorSink::Osd.osd() && !ConfigErrorSink::Osd.ipc());
        assert!(ConfigErrorSink::Ipc.ipc() && !ConfigErrorSink::Ipc.osd());
    }

    // A typo'd chord is the motivating case: it drops the bind, and used to do so
    // with nothing but a log line. The diagnostic has to actually be collected.
    #[test]
    fn a_dropped_bind_is_collected_as_a_diagnostic() {
        let _ = take_config_diagnostics();
        assert!(parse_chord("Alt+nosuchkey", NavAction::RotateColumnsLeft).is_none());
        let problems = take_config_diagnostics();
        assert_eq!(problems.len(), 1, "the dropped bind must be reported");
        assert!(
            problems[0].contains("nosuchkey") && problems[0].contains("Alt+nosuchkey"),
            "diagnostic should name the offending key and chord: {:?}",
            problems[0]
        );
    }

    #[test]
    fn taking_diagnostics_drains_them() {
        let _ = take_config_diagnostics();
        note_config_problem("first".to_string());
        note_config_problem("second".to_string());
        assert_eq!(take_config_diagnostics(), vec!["first", "second"]);
        assert!(
            take_config_diagnostics().is_empty(),
            "a second drain must not repeat the same problems"
        );
    }

    #[test]
    fn a_clean_config_produces_no_diagnostics() {
        let _ = take_config_diagnostics();
        let raw: RawConfig = toml::from_str(DEFAULT_CONFIG).expect("default config parses");
        let _ = Config::resolve(raw);
        assert!(
            take_config_diagnostics().is_empty(),
            "the shipped default config must not warn about itself"
        );
    }
