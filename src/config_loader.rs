//! Multi-file TOML config discovery and merge.
//!
//! Rubix's config directory (`~/.config/rubix`, itself typically a stow
//! symlink into the dotfiles repo) is walked recursively for config files,
//! which are parsed and deep-merged at the `toml::Table` level before being
//! deserialized into `RawConfig` exactly once. See `config.rs`'s `load` and
//! `reload` for how this plugs in.
//!
//! Deserializing once (rather than per-file, then merging structs) is
//! deliberate: it keeps `RawConfig`'s `#[serde(default)]` attributes and
//! every test that already exercises it authoritative, instead of requiring
//! every field to become `Option` and hand-writing struct-level merge logic.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::RawConfig;

/// Extensions the discovery walk picks up. A slice (not a single constant)
/// so a future extension (e.g. a `.lua` scripting-layer config) can be added
/// here without redesigning the walker itself.
const CONFIG_EXTENSIONS: &[&str] = &["toml"];

pub(super) fn has_config_extension(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| CONFIG_EXTENSIONS.contains(&e))
}

/// Recursively find every config file under `root`, returned as paths
/// relative to `root`, sorted bytewise by that relative path.
///
/// Bytewise, not `Path`'s own `Ord` and not locale-collated: the order is
/// semantically load-bearing (see [`merge_all`]'s array-of-tables
/// concatenation), so it must be deterministic and locale-independent.
///
/// Symlinks are followed -- `~/.config/rubix` is itself typically a stow
/// symlink into the dotfiles repo, so a walker that refuses symlinks would
/// find nothing at all -- with cycle detection via the canonicalised real
/// paths of the *current descent's ancestors* (not every directory visited
/// anywhere in the tree: two sibling symlinks that happen to point at the
/// same real directory are not a cycle, and both must still be walked).
pub(super) fn discover_config_files(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut ancestors = Vec::new();
    if let Ok(real_root) = root.canonicalize() {
        ancestors.push(real_root);
    }
    walk(root, root, &mut found, &mut ancestors);
    found.sort_by(|a, b| path_bytes(a).cmp(path_bytes(b)));
    found
}

fn path_bytes(path: &Path) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes()
}

fn walk(root: &Path, dir: &Path, found: &mut Vec<PathBuf>, ancestors: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        // `metadata` (not `symlink_metadata`) follows symlinks, so a
        // symlinked file or directory is treated as whatever it points to.
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        if meta.is_dir() {
            let Ok(real) = path.canonicalize() else { continue };
            if ancestors.contains(&real) {
                continue; // this directory is its own ancestor via a symlink -- cycle
            }
            ancestors.push(real);
            walk(root, &path, found, ancestors);
            ancestors.pop();
        } else if meta.is_file() && has_config_extension(&path) && let Ok(rel) = path.strip_prefix(root) {
            found.push(rel.to_path_buf());
        }
    }
}

/// Key path (dotted, with `[i]` segments for array-of-tables entries) -> the
/// file that supplied it. Lets both the conflict error and the unknown-key
/// report name an actual file instead of just a key.
pub(super) type Provenance = HashMap<String, PathBuf>;

/// Everything that can go wrong loading/merging the config tree, each naming
/// the offending file(s) so the caller can report exactly what to fix.
#[derive(Debug)]
pub(super) enum LoadError {
    NoFiles,
    Io { file: PathBuf, error: std::io::Error },
    Parse { file: PathBuf, error: toml::de::Error },
    Conflict { key: String, first: PathBuf, second: PathBuf },
    Deserialize(toml::de::Error),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::NoFiles => write!(f, "no config files found"),
            LoadError::Io { file, error } => write!(f, "failed to read {} ({error})", file.display()),
            LoadError::Parse { file, error } => write!(f, "failed to parse {} ({error})", file.display()),
            LoadError::Conflict { key, first, second } => write!(
                f,
                "config key '{key}' is set in both {} and {}",
                first.display(),
                second.display()
            ),
            LoadError::Deserialize(error) => write!(f, "merged config does not match schema ({error})"),
        }
    }
}

/// Parse every file in `files` (relative to `root`, in the order given -- the
/// caller is responsible for having sorted it, see [`discover_config_files`])
/// and deep-merge them into one `toml::Table`, alongside a [`Provenance`] map
/// recording which file supplied each key path.
///
/// Merge semantics, in order of precedence:
/// - tables merge recursively;
/// - arrays of tables (`[[decoration.rule]]`, `[[output]]`) concatenate,
///   earlier-sorted files first -- load-bearing, since rule/output order is
///   documented as applying in file order;
/// - anything else two files both set (a scalar, or the same key as
///   incompatible types) is a hard error naming both files. Silent
///   last-wins is deliberately not supported.
pub(super) fn merge_all(root: &Path, files: &[PathBuf]) -> Result<(toml::Table, Provenance), LoadError> {
    if files.is_empty() {
        return Err(LoadError::NoFiles);
    }
    let mut acc = toml::Table::new();
    let mut prov = Provenance::new();
    for rel in files {
        let full = root.join(rel);
        let text = std::fs::read_to_string(&full).map_err(|e| LoadError::Io { file: rel.clone(), error: e })?;
        let table: toml::Table =
            toml::from_str(&text).map_err(|e| LoadError::Parse { file: rel.clone(), error: e })?;
        merge_into(&mut acc, &mut prov, table, rel, &mut Vec::new())?;
    }
    Ok((acc, prov))
}

/// Merge already-parsed tables, labelled by the name they should be reported
/// under. Used for the built-in defaults, which are embedded rather than read
/// from disk -- concatenating their text instead would be wrong, since a bare
/// top-level key following another file's `[section]` header silently parses
/// as a member of that section.
pub(super) fn merge_parsed(
    parts: &[(&str, toml::Table)],
) -> Result<(toml::Table, Provenance), LoadError> {
    let mut acc = toml::Table::new();
    let mut prov = Provenance::new();
    for (name, table) in parts {
        let rel = PathBuf::from(name);
        merge_into(&mut acc, &mut prov, table.clone(), &rel, &mut Vec::new())?;
    }
    Ok((acc, prov))
}

fn merge_into(
    acc: &mut toml::Table,
    prov: &mut Provenance,
    incoming: toml::Table,
    file: &Path,
    path: &mut Vec<String>,
) -> Result<(), LoadError> {
    for (k, v) in incoming {
        path.push(k.clone());
        let key_path = path.join(".");
        match acc.remove(&k) {
            None => {
                record_provenance(&v, prov, path, file);
                acc.insert(k, v);
            }
            Some(toml::Value::Table(mut existing_table)) => {
                let Some(incoming_table) = as_table(v) else {
                    let first = prov.get(&key_path).cloned().unwrap_or_else(|| file.to_path_buf());
                    path.pop();
                    return Err(LoadError::Conflict { key: key_path, first, second: file.to_path_buf() });
                };
                merge_into(&mut existing_table, prov, incoming_table, file, path)?;
                acc.insert(k, toml::Value::Table(existing_table));
            }
            Some(toml::Value::Array(mut existing_arr)) if is_table_array_compatible(&existing_arr) => {
                match v {
                    toml::Value::Array(incoming_arr) if is_table_array_compatible(&incoming_arr) => {
                        let start = existing_arr.len();
                        for (i, item) in incoming_arr.into_iter().enumerate() {
                            let mut ipath = path.clone();
                            ipath.push(format!("[{}]", start + i));
                            record_provenance(&item, prov, &ipath, file);
                            existing_arr.push(item);
                        }
                        acc.insert(k, toml::Value::Array(existing_arr));
                    }
                    _ => {
                        let first = prov.get(&key_path).cloned().unwrap_or_else(|| file.to_path_buf());
                        path.pop();
                        return Err(LoadError::Conflict { key: key_path, first, second: file.to_path_buf() });
                    }
                }
            }
            Some(_existing) => {
                let first = prov.get(&key_path).cloned().unwrap_or_else(|| file.to_path_buf());
                path.pop();
                return Err(LoadError::Conflict { key: key_path, first, second: file.to_path_buf() });
            }
        }
        path.pop();
    }
    Ok(())
}

fn as_table(v: toml::Value) -> Option<toml::Table> {
    match v {
        toml::Value::Table(t) => Some(t),
        _ => None,
    }
}

/// Empty arrays count as compatible (vacuously) so an explicit `x = []` in
/// one file doesn't block a later file's `[[x]] ...` entries from
/// concatenating onto it.
fn is_table_array_compatible(arr: &[toml::Value]) -> bool {
    arr.iter().all(|v| matches!(v, toml::Value::Table(_)))
}

fn record_provenance(value: &toml::Value, prov: &mut Provenance, path: &[String], file: &Path) {
    prov.insert(path.join("."), file.to_path_buf());
    match value {
        toml::Value::Table(t) => {
            for (k, v) in t {
                let mut p = path.to_vec();
                p.push(k.clone());
                record_provenance(v, prov, &p, file);
            }
        }
        toml::Value::Array(arr) => {
            for (i, v) in arr.iter().enumerate() {
                if matches!(v, toml::Value::Table(_)) {
                    let mut p = path.to_vec();
                    p.push(format!("[{i}]"));
                    record_provenance(v, prov, &p, file);
                }
            }
        }
        _ => {}
    }
}

/// Deserialize the merged table into `RawConfig` exactly once, so every
/// `#[serde(default)]` and existing test on `RawConfig` stays authoritative.
pub(super) fn deserialize_merged(table: toml::Table) -> Result<RawConfig, LoadError> {
    RawConfig::deserialize(toml::Value::Table(table)).map_err(LoadError::Deserialize)
}

// ---- unknown-key reporting ----

/// A minimal shadow of `RawConfig`'s shape, hand-kept in sync with it, used
/// only to name unrecognised keys (and the file that set them) without
/// making `RawConfig` itself do double duty as a schema description.
enum Schema {
    Table(&'static [(&'static str, Schema)]),
    ArrayOfTables(&'static [(&'static str, Schema)]),
    /// `[keybinds]`: a user-defined map (chord -> action), not a fixed set
    /// of keys -- its contents must never be reported as unknown.
    FreeForm,
    Leaf,
}

// These lists stand in for `#[serde(deny_unknown_fields)]`, which is
// deliberately absent (see `deserialize_merged`), and they are hand-maintained.
// Adding a field to a `Raw*` struct in `config.rs` without adding it here does
// not break the setting -- serde still reads it, and it works -- it just makes
// the compositor report "unknown config key" on every single load. A config
// that works while complaining about itself is a confusing thing to hand
// someone, and it shipped exactly that way once. `the_schema_matches_the_raw_
// structs` in the tests below now fails instead.
const RULE_SCHEMA: &[(&str, Schema)] = &[
    ("app_id", Schema::Leaf),
    ("title", Schema::Leaf),
    ("active_color", Schema::Leaf),
    ("inactive_color", Schema::Leaf),
    ("active_luminance_nits", Schema::Leaf),
    ("inactive_luminance_nits", Schema::Leaf),
    ("active_glow_margin", Schema::Leaf),
    ("inactive_glow_margin", Schema::Leaf),
    ("glow_falloff", Schema::Leaf),
    ("active_opacity", Schema::Leaf),
    ("inactive_opacity", Schema::Leaf),
    ("active_backdrop_tonemap", Schema::Leaf),
    ("inactive_backdrop_tonemap", Schema::Leaf),
    ("active_backdrop_blur", Schema::Leaf),
    ("inactive_backdrop_blur", Schema::Leaf),
    ("active_refract", Schema::Leaf),
    ("inactive_refract", Schema::Leaf),
    ("active_effect", Schema::Leaf),
    ("inactive_effect", Schema::Leaf),
    ("effect_speed", Schema::Leaf),
    ("active_effect_luminance_nits", Schema::Leaf),
    ("inactive_effect_luminance_nits", Schema::Leaf),
    ("effect_depth", Schema::Leaf),
    ("effect_fps", Schema::Leaf),
];

const OUTPUT_SCHEMA: &[(&str, Schema)] = &[
    ("name", Schema::Leaf),
    ("position", Schema::Leaf),
    ("mode", Schema::Leaf),
    ("primary", Schema::Leaf),
    ("transform", Schema::Leaf),
    ("hdr", Schema::Leaf),
    ("wallpaper", Schema::Leaf),
];

const DECORATION_SCHEMA: &[(&str, Schema)] = &[
    ("border_width", Schema::Leaf),
    ("corner_radius", Schema::Leaf),
    ("active_color", Schema::Leaf),
    ("inactive_color", Schema::Leaf),
    ("active_luminance_nits", Schema::Leaf),
    ("inactive_luminance_nits", Schema::Leaf),
    ("active_glow_margin", Schema::Leaf),
    ("inactive_glow_margin", Schema::Leaf),
    ("glow_falloff", Schema::Leaf),
    ("active_opacity", Schema::Leaf),
    ("inactive_opacity", Schema::Leaf),
    ("obscured_opacity", Schema::Leaf),
    ("obscured_threshold", Schema::Leaf),
    ("active_backdrop_tonemap", Schema::Leaf),
    ("inactive_backdrop_tonemap", Schema::Leaf),
    ("active_backdrop_blur", Schema::Leaf),
    ("inactive_backdrop_blur", Schema::Leaf),
    ("backdrop_blur_radius", Schema::Leaf),
    ("backdrop_luminance_nits", Schema::Leaf),
    ("active_refract", Schema::Leaf),
    ("inactive_refract", Schema::Leaf),
    ("refract_strength", Schema::Leaf),
    ("refract_facet_size", Schema::Leaf),
    ("refract_dispersion", Schema::Leaf),
    ("active_effect", Schema::Leaf),
    ("inactive_effect", Schema::Leaf),
    ("effect_speed", Schema::Leaf),
    ("active_effect_luminance_nits", Schema::Leaf),
    ("inactive_effect_luminance_nits", Schema::Leaf),
    ("effect_depth", Schema::Leaf),
    ("effect_fps", Schema::Leaf),
    ("rule", Schema::ArrayOfTables(RULE_SCHEMA)),
];

const WALLPAPER_SCHEMA: &[(&str, Schema)] = &[
    ("path", Schema::Leaf),
    ("mode", Schema::Leaf),
    ("luminance_scale", Schema::Leaf),
    ("interval_seconds", Schema::Leaf),
    ("sdr_reference_nits", Schema::Leaf),
];

const IDLE_SCHEMA: &[(&str, Schema)] = &[("enabled", Schema::Leaf), ("screen_off_seconds", Schema::Leaf)];
const LAYOUT_SCHEMA: &[(&str, Schema)] =
    &[("visible_columns", Schema::Leaf), ("outer_gap", Schema::Leaf), ("inner_gap", Schema::Leaf)];
const ANIMATION_SCHEMA: &[(&str, Schema)] = &[("duration_ms", Schema::Leaf)];
const INPUT_SCHEMA: &[(&str, Schema)] =
    &[("focus_follows_mouse", Schema::Leaf), ("mouse_follows_focus", Schema::Leaf)];
const DIAGNOSTICS_SCHEMA: &[(&str, Schema)] = &[("config_errors", Schema::Leaf)];
const THEME_SCHEMA: &[(&str, Schema)] = &[
    ("enable", Schema::Leaf),
    ("output_path", Schema::Leaf),
    ("on_change", Schema::Leaf),
    ("target_lc", Schema::Leaf),
    ("opacity", Schema::Leaf),
    ("backdrop_cap_nits", Schema::Leaf),
    ("backdrop_blurred", Schema::Leaf),
    ("apply_to_borders", Schema::Leaf),
];

const BAR_SCHEMA: &[(&str, Schema)] = &[
    ("enabled", Schema::Leaf),
    ("position", Schema::Leaf),
    ("height", Schema::Leaf),
    ("font_size", Schema::Leaf),
    ("label", Schema::Leaf),
];

const ROOT_SCHEMA: &[(&str, Schema)] = &[
    ("layout", Schema::Table(LAYOUT_SCHEMA)),
    ("animation", Schema::Table(ANIMATION_SCHEMA)),
    ("input", Schema::Table(INPUT_SCHEMA)),
    ("diagnostics", Schema::Table(DIAGNOSTICS_SCHEMA)),
    ("decoration", Schema::Table(DECORATION_SCHEMA)),
    ("sdr_white_nits", Schema::Leaf),
    ("keybinds", Schema::FreeForm),
    ("startup", Schema::Leaf),
    ("output", Schema::ArrayOfTables(OUTPUT_SCHEMA)),
    ("wallpaper", Schema::Table(WALLPAPER_SCHEMA)),
    ("idle", Schema::Table(IDLE_SCHEMA)),
    ("theme", Schema::Table(THEME_SCHEMA)),
    ("bar", Schema::Table(BAR_SCHEMA)),
];

/// Report every key in `table` that `RawConfig` does not recognise, naming
/// the key's full dotted path and the file `prov` says supplied it.
///
/// Never fatal: an unknown key is reported here and then simply dropped by
/// `deserialize_merged` (via `#[serde(deny_unknown_fields)]`'s *absence* --
/// serde silently ignores it), the same as it silently did before this
/// existed. The whole point is turning that silence into a report.
pub(super) fn check_unknown_keys(table: &toml::Table, prov: &Provenance) -> Vec<String> {
    let mut out = Vec::new();
    check_table(table, ROOT_SCHEMA, &mut Vec::new(), prov, &mut out);
    out
}

fn check_table(
    table: &toml::Table,
    schema: &'static [(&'static str, Schema)],
    path: &mut Vec<String>,
    prov: &Provenance,
    out: &mut Vec<String>,
) {
    for (k, v) in table {
        path.push(k.clone());
        match schema.iter().find(|(name, _)| *name == k) {
            None => {
                let key_path = path.join(".");
                let file = prov
                    .get(&key_path)
                    .map(|f| f.display().to_string())
                    .unwrap_or_else(|| "<unknown file>".to_string());
                out.push(format!("unknown config key '{key_path}' in {file}"));
            }
            Some((_, Schema::Table(sub))) => {
                if let toml::Value::Table(t) = v {
                    check_table(t, sub, path, prov, out);
                }
            }
            Some((_, Schema::ArrayOfTables(sub))) => {
                if let toml::Value::Array(arr) = v {
                    for (i, item) in arr.iter().enumerate() {
                        if let toml::Value::Table(t) = item {
                            path.push(format!("[{i}]"));
                            check_table(t, sub, path, prov, out);
                            path.pop();
                        }
                    }
                }
            }
            Some((_, Schema::FreeForm)) | Some((_, Schema::Leaf)) => {}
        }
        path.pop();
    }
}

#[cfg(test)]
#[path = "config_loader_tests.rs"]
mod tests;
