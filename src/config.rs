use std::collections::HashMap;
use std::path::PathBuf;

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
    keybinds: HashMap<String, NavAction>,
}

#[derive(Deserialize)]
struct RawLayout {
    visible_columns: usize,
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
        }
    }
}

/// `$XDG_CONFIG_HOME/rubix/config.toml`, falling back to `~/.config/rubix/config.toml`.
fn config_path() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("rubix/config.toml"));
        }
    }
    std::env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".config/rubix/config.toml"))
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
mod tests {
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
        assert_eq!(bind_count, 8);
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
}
