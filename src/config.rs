use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use smithay::input::keyboard::{
    xkb::{keysym_from_name, KEYSYM_NO_FLAGS},
    ModifiersState,
};

use crate::input::NavAction;

/// Built-in fallback, used when no user config exists or one fails to parse.
/// Also serves as the copy-paste reference at `config/default.toml`.
const DEFAULT_CONFIG: &str = include_str!("../config/default.toml");

/// Runtime configuration, resolved from `config.toml` (or the built-in default).
/// Chords have already been parsed into concrete modifier/keysym matches here --
/// the raw string form only exists during deserialization.
pub struct Config {
    pub visible_columns: usize,
    pub keybinds: Vec<Keybind>,
    pub animation_duration: Duration,
}

/// A resolved chord: the exact modifier set and keysym to match, plus its action.
pub struct Keybind {
    logo: bool,
    alt: bool,
    ctrl: bool,
    shift: bool,
    keysym: u32,
    pub action: NavAction,
}

impl Keybind {
    /// True when the live modifier state and resolved keysym match this bind.
    /// Modifiers must match *exactly*, so `Alt+h` is not fired by `Super+Alt+h`.
    pub fn matches(&self, mods: &ModifiersState, sym: u32) -> bool {
        self.logo == mods.logo
            && self.alt == mods.alt
            && self.ctrl == mods.ctrl
            && self.shift == mods.shift
            && self.keysym == sym
    }
}

// ---- deserialization (raw string form) ----

// No serde renames anywhere: config keys mirror the Rust identifiers exactly
// (snake_case fields, PascalCase NavAction variants). The less translation
// between code and config, the fewer places they can silently drift.
#[derive(Deserialize)]
struct RawConfig {
    layout: RawLayout,
    #[serde(default)]
    animation: RawAnimation,
    keybinds: HashMap<String, NavAction>,
}

#[derive(Deserialize)]
struct RawLayout {
    visible_columns: usize,
}

// Optional section: a config omitting `[animation]` entirely still parses,
// falling back to `default_duration_ms` below.
#[derive(Deserialize)]
struct RawAnimation {
    #[serde(default = "default_duration_ms")]
    duration_ms: u64,
}

impl Default for RawAnimation {
    fn default() -> Self {
        RawAnimation { duration_ms: default_duration_ms() }
    }
}

fn default_duration_ms() -> u64 {
    250
}

impl Config {
    /// Load and resolve the config: user file if present and valid, else the
    /// built-in default. Never panics on a missing or malformed user file.
    pub fn load() -> Self {
        Self::resolve(Self::read_raw())
    }

    fn read_raw() -> RawConfig {
        let text = config_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_else(|| DEFAULT_CONFIG.to_string());

        toml::from_str(&text).unwrap_or_else(|e| {
            tracing::warn!("failed to parse config ({e}); using built-in defaults");
            toml::from_str(DEFAULT_CONFIG).expect("built-in default config must parse")
        })
    }

    fn resolve(raw: RawConfig) -> Self {
        let keybinds = raw
            .keybinds
            .into_iter()
            .filter_map(|(chord, action)| parse_chord(&chord, action))
            .collect();

        Config {
            visible_columns: raw.layout.visible_columns,
            keybinds,
            animation_duration: Duration::from_millis(raw.animation.duration_ms),
        }
    }

    /// Re-read and resolve the *user* config from disk for hot-reload. Returns
    /// `None` if the file is missing or fails to parse -- the caller keeps its
    /// current config (keep-last-good), so a broken edit never disturbs the
    /// running session. Distinct from [`load`](Self::load), which substitutes the
    /// built-in default at startup: on reload we deliberately do *not* fall back
    /// to default, preserving the last set of working binds.
    pub fn reload() -> Option<Config> {
        let text = std::fs::read_to_string(config_path()?).ok()?;
        let raw: RawConfig = toml::from_str(&text)
            .map_err(|e| tracing::warn!("config reload failed to parse ({e}); keeping current config"))
            .ok()?;
        Some(Config::resolve(raw))
    }
}

/// `$XDG_CONFIG_HOME/rubix/config.toml`, falling back to `~/.config/rubix/config.toml`.
pub fn config_path() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("rubix/config.toml"));
        }
    }
    std::env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".config/rubix/config.toml"))
}

/// Directory + filename to hand the file watcher. We watch the *parent dir*
/// (not the file) so a save survives an editor's atomic rename -- a direct file
/// watch goes deaf once the inode is swapped. The path is canonicalized so a
/// stow symlink is followed to the real file under the dotfiles repo (watching
/// the symlink's own dir would miss writes to the target). Returns `None` when
/// there's no user config and no dir to watch -- the compiled-in default can't
/// be hot-reloaded, so there is simply nothing to watch.
pub fn config_watch_target() -> Option<(PathBuf, std::ffi::OsString)> {
    let path = config_path()?;
    // File present: canonicalize it, watch the real inode's parent dir.
    if let Ok(real) = path.canonicalize() {
        return Some((real.parent()?.to_path_buf(), real.file_name()?.to_os_string()));
    }
    // File absent: watch the (canonicalized) parent dir so a later create fires.
    let dir = path.parent()?.canonicalize().ok()?;
    Some((dir, path.file_name()?.to_os_string()))
}

/// True when a filesystem event warrants a config reload: it names the config
/// file and is a content or create change. Bare metadata touches are filtered
/// out -- our own read bumps the file's access time, and reacting to that would
/// feed back into an endless reload loop (self-limiting under `relatime`, but
/// cheap to rule out regardless).
pub fn should_reload(event: &calloop_notify::notify::Event, file_name: &std::ffi::OsStr) -> bool {
    use calloop_notify::notify::event::{EventKind, ModifyKind};

    let touches = event.paths.iter().any(|p| p.file_name() == Some(file_name));
    let is_write = match &event.kind {
        EventKind::Create(_) => true,
        EventKind::Modify(kind) => !matches!(kind, ModifyKind::Metadata(_)),
        _ => false,
    };
    touches && is_write
}

/// Parse a chord like `"Alt+Return"` into a resolved [`Keybind`]. Modifier tokens
/// are case-insensitive; the final token is an xkb keysym name. An unknown key
/// name drops the bind (with a warning) rather than aborting startup.
fn parse_chord(chord: &str, action: NavAction) -> Option<Keybind> {
    let (mut logo, mut alt, mut ctrl, mut shift) = (false, false, false, false);
    let mut keysym = None;

    for token in chord.split('+').map(str::trim) {
        match token.to_ascii_lowercase().as_str() {
            "super" | "logo" | "mod4" | "meta" => logo = true,
            "alt" | "mod1" => alt = true,
            "ctrl" | "control" => ctrl = true,
            "shift" => shift = true,
            _ => {
                let sym = keysym_from_name(token, KEYSYM_NO_FLAGS).raw();
                if sym == 0 {
                    tracing::warn!("unknown key '{token}' in chord '{chord}'; ignoring bind");
                    return None;
                }
                keysym = Some(sym);
            }
        }
    }

    Some(Keybind {
        logo,
        alt,
        ctrl,
        shift,
        keysym: keysym?,
        action,
    })
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
