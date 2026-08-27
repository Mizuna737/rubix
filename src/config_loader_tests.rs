use super::*;
use std::fs;

/// A throwaway directory under the system temp dir, cleaned up on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let mut dir = std::env::temp_dir();
        dir.push(format!("rubix-config-loader-test-{}-{}", std::process::id(), unique_suffix()));
        fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, rel: &str, contents: &str) {
        let full = self.0.join(rel);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).expect("create parent dir");
        }
        fs::write(full, contents).expect("write config file");
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn unique_suffix() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

// ---- discovery ----

#[test]
fn discovers_nested_toml_files_in_sorted_order() {
    let dir = TempDir::new();
    dir.write("b.toml", "");
    dir.write("a/inner.toml", "");
    dir.write("a.toml", "");

    let found = discover_config_files(dir.path());
    let names: Vec<String> = found.iter().map(|p| p.to_string_lossy().to_string()).collect();

    // Bytewise on the relative path: "a.toml" < "a/inner.toml" < "b.toml"
    // because '.' (0x2e) sorts before '/' (0x2f).
    assert_eq!(names, vec!["a.toml", "a/inner.toml", "b.toml"]);
}

#[test]
fn ignores_non_toml_files() {
    let dir = TempDir::new();
    dir.write("keep.toml", "");
    dir.write("ignore.txt", "");
    dir.write("README.md", "");

    let found = discover_config_files(dir.path());
    assert_eq!(found, vec![PathBuf::from("keep.toml")]);
}

#[cfg(unix)]
#[test]
fn follows_a_symlinked_directory() {
    let dir = TempDir::new();
    dir.write("real/nested.toml", "");
    std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("linked"))
        .expect("create symlink");

    let found = discover_config_files(dir.path());
    let names: Vec<String> = found.iter().map(|p| p.to_string_lossy().to_string()).collect();
    assert!(names.contains(&"real/nested.toml".to_string()));
    assert!(names.contains(&"linked/nested.toml".to_string()));
}

#[cfg(unix)]
#[test]
fn terminates_on_a_symlink_cycle() {
    let dir = TempDir::new();
    fs::create_dir_all(dir.path().join("a")).unwrap();
    dir.write("a/real.toml", "");
    // a/loop -> the temp dir itself, so walking it re-enters `a`, `a/loop`, ...
    std::os::unix::fs::symlink(dir.path(), dir.path().join("a/loop")).expect("create symlink");

    // Must terminate at all; a broken cycle guard hangs or stack-overflows.
    let found = discover_config_files(dir.path());
    assert!(found.iter().any(|p| p.to_string_lossy() == "a/real.toml"));
}

// ---- merge ----

#[test]
fn tables_merge_recursively() {
    let dir = TempDir::new();
    dir.write("a.toml", "[layout]\nvisible_columns = 3\n");
    dir.write("b.toml", "[layout]\nouter_gap = 5\n");

    let files = discover_config_files(dir.path());
    let (table, _) = merge_all(dir.path(), &files).expect("merge succeeds");
    let layout = table.get("layout").unwrap().as_table().unwrap();
    assert_eq!(layout.get("visible_columns").unwrap().as_integer(), Some(3));
    assert_eq!(layout.get("outer_gap").unwrap().as_integer(), Some(5));
}

#[test]
fn arrays_of_tables_concatenate_in_sorted_file_order() {
    let dir = TempDir::new();
    dir.write(
        "a.toml",
        "[[decoration.rule]]\napp_id = \"first\"\n",
    );
    dir.write(
        "b.toml",
        "[[decoration.rule]]\napp_id = \"second\"\n",
    );

    let files = discover_config_files(dir.path());
    assert_eq!(files, vec![PathBuf::from("a.toml"), PathBuf::from("b.toml")]);

    let (table, _) = merge_all(dir.path(), &files).expect("merge succeeds");
    let rules = table.get("decoration").unwrap().as_table().unwrap().get("rule").unwrap().as_array().unwrap();
    assert_eq!(rules.len(), 2);
    assert_eq!(rules[0].get("app_id").unwrap().as_str(), Some("first"));
    assert_eq!(rules[1].get("app_id").unwrap().as_str(), Some("second"));
}

#[test]
fn scalar_conflict_errors_naming_both_files() {
    let dir = TempDir::new();
    dir.write("a.toml", "[layout]\nvisible_columns = 3\n");
    dir.write("b.toml", "[layout]\nvisible_columns = 4\n");

    let files = discover_config_files(dir.path());
    let err = merge_all(dir.path(), &files).expect_err("conflicting scalar must error");
    let message = err.to_string();
    assert!(message.contains("layout.visible_columns"), "{message}");
    assert!(message.contains("a.toml"), "{message}");
    assert!(message.contains("b.toml"), "{message}");
}

// ---- unknown-key reporting ----

#[test]
fn unknown_key_is_reported_with_its_file() {
    let mut table = toml::Table::new();
    let mut decoration = toml::Table::new();
    decoration.insert("active_backdrop_tonemap".to_string(), toml::Value::Boolean(true));
    // The actual typo that prompted this feature.
    decoration.insert("active_background_tonemap".to_string(), toml::Value::Boolean(true));
    table.insert("decoration".to_string(), toml::Value::Table(decoration));

    let mut prov = Provenance::new();
    prov.insert("decoration.active_background_tonemap".to_string(), PathBuf::from("typo.toml"));

    let problems = check_unknown_keys(&table, &prov);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains("decoration.active_background_tonemap"), "{problems:?}");
    assert!(problems[0].contains("typo.toml"), "{problems:?}");
}

#[test]
fn keybinds_contents_are_never_reported_as_unknown() {
    let mut table = toml::Table::new();
    let mut keybinds = toml::Table::new();
    keybinds.insert("Alt+h".to_string(), toml::Value::String("RotateColumnsLeft".to_string()));
    keybinds.insert("Super+Return".to_string(), toml::Value::String("Spawn".to_string()));
    table.insert("keybinds".to_string(), toml::Value::Table(keybinds));

    let prov = Provenance::new();
    let problems = check_unknown_keys(&table, &prov);
    assert!(problems.is_empty(), "{problems:?}");
}

#[test]
fn a_known_key_never_gets_reported() {
    let mut table = toml::Table::new();
    let mut layout = toml::Table::new();
    layout.insert("visible_columns".to_string(), toml::Value::Integer(3));
    table.insert("layout".to_string(), toml::Value::Table(layout));

    let prov = Provenance::new();
    assert!(check_unknown_keys(&table, &prov).is_empty());
}

/// Field names declared by a `Raw*` struct in `config.rs`.
///
/// Text-scraped rather than derived, because there is no reflection to derive
/// it from: the raw structs only implement `Deserialize`, and adding
/// `Serialize` purely to enumerate them would mean annotating every `Option`
/// field to keep the TOML serializer happy. Scraping the source is uglier but
/// it is complete, and it costs nothing at runtime.
fn raw_struct_fields(src: &str, name: &str) -> Vec<String> {
    let head = format!("struct {name} {{");
    let start = src.find(&head).unwrap_or_else(|| panic!("no `{head}` in config.rs")) + head.len();
    let body = &src[start..];
    let end = body.find("\n}").expect("unterminated struct");
    body[..end]
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("//"))
        .filter_map(|l| l.split(':').next())
        .map(|l| l.trim_start_matches("pub(crate)").trim_start_matches("pub").trim())
        .filter(|l| {
            !l.is_empty() && l.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        })
        .map(str::to_string)
        .collect()
}

/// The schema lists stand in for `deny_unknown_fields` and are maintained by
/// hand, so they drift silently. When they do, the setting still works -- serde
/// reads it regardless -- and the only symptom is the compositor reporting
/// "unknown config key" about its own valid config on every load. That shipped:
/// the refraction keys were added to `RawDecoration` and to nothing else, and
/// surfaced as a config error on every save of a file that was working fine.
#[test]
fn the_schema_matches_the_raw_structs() {
    let src = include_str!("config.rs");
    let pairs: &[(&str, &[(&str, Schema)])] = &[
        ("RawDecoration", DECORATION_SCHEMA),
        ("RawBorderRule", RULE_SCHEMA),
        ("RawWallpaper", WALLPAPER_SCHEMA),
        ("RawOutput", OUTPUT_SCHEMA),
        ("RawLayout", LAYOUT_SCHEMA),
        ("RawAnimation", ANIMATION_SCHEMA),
        ("RawInput", INPUT_SCHEMA),
        ("RawIdle", IDLE_SCHEMA),
        ("RawTheme", THEME_SCHEMA),
        ("RawBar", BAR_SCHEMA),
    ];

    let mut problems = Vec::new();
    for (struct_name, schema) in pairs {
        let fields = raw_struct_fields(src, struct_name);
        assert!(!fields.is_empty(), "scraped no fields from {struct_name}");
        let names: Vec<&str> = schema.iter().map(|(n, _)| *n).collect();
        for f in &fields {
            if !names.contains(&f.as_str()) {
                problems.push(format!("{struct_name}.{f} is missing from the schema"));
            }
        }
        for n in &names {
            if !fields.iter().any(|f| f == n) {
                problems.push(format!("schema lists '{n}', which {struct_name} does not have"));
            }
        }
    }
    assert!(problems.is_empty(), "config schema drift:\n  {}", problems.join("\n  "));
}
