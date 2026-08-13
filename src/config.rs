use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use smithay::input::keyboard::{
    xkb::{keysym_from_name, KEYSYM_NO_FLAGS},
    ModifiersState,
};
use smithay::utils::Transform;

use crate::input::NavAction;

/// Built-in fallback, used when no user config exists or one fails to parse.
/// Also serves as the copy-paste reference at `config/default.toml`.
const DEFAULT_CONFIG: &str = include_str!("../config/default.toml");

/// Runtime configuration, resolved from `config.toml` (or the built-in default).
/// Chords have already been parsed into concrete modifier/keysym matches here --
/// the raw string form only exists during deserialization.
pub struct Config {
    pub visible_columns: usize,
    pub outer_gap: u32,
    pub inner_gap: u32,
    pub keybinds: Vec<Keybind>,
    pub animation_duration: Duration,
    /// Commands run once (via `sh -c`) after the compositor is up -- the Rubix
    /// equivalent of AwesomeWM's autorun. Fired from the XWayland-ready hook so
    /// children inherit WAYLAND_DISPLAY and get DISPLAY set (see main.rs).
    pub startup: Vec<String>,
    /// Per-connector output placement, resolved from `[[output]]` entries.
    /// Empty when the user config omits the section (or has no entries) --
    /// the udev backend falls back to auto left-to-right layout in that case.
    pub outputs: Vec<OutputConfig>,
    /// SDR white luminance (nits) fed to the HDR encode shader's
    /// `sdr_white_nits` uniform for `hdr = true` outputs. Live/hot-reloadable;
    /// also adjustable at runtime via the IncreaseSdrWhite/DecreaseSdrWhite
    /// keybinds (see RubixState::sdr_white_nits, the actual live value the
    /// render path reads -- this field only seeds/reseeds it). Always in
    /// [80, 300], clamped here at resolve time.
    pub sdr_white_nits: f32,

    /// Hovering a window gives it keyboard focus. Live/hot-reloadable, and also
    /// flippable at runtime via the ToggleFocusFollowsMouse keybind (see
    /// RubixState::focus_follows_mouse, the live value the input path reads --
    /// this field only seeds/reseeds it).
    pub focus_follows_mouse: bool,
}

/// Resolved placement for one physical output, matched by connector name
/// (e.g. "DP-3", "HDMI-A-1") at connect time in udev.rs.
pub struct OutputConfig {
    pub name: String,
    /// Top-left corner in global compositor space.
    pub position: (i32, i32),
    /// Preferred (width, height); `None` means use the output's own preferred mode.
    pub mode: Option<(i32, i32)>,
    pub primary: bool,
    /// Output transform (rotation/flip). Defaults to `Transform::Normal` when
    /// the config omits `transform` or the string is unrecognized.
    pub transform: Transform,
    /// Opt-in HDR (BT.2020/PQ) for this output. Default false -- omitting
    /// `hdr` (or setting it false) leaves the SDR path byte-for-byte
    /// unchanged. Requires an HDR-capable display; see src/hdr.rs.
    pub hdr: bool,
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
    #[serde(default)]
    input: RawInput,
    // Top-level scalar (must sit before the first [table] header in the TOML
    // file -- see config/default.toml). Live/hot-reloadable: HDR Phase 4's
    // SDR-white-nits slider, adjustable via keybind (IncreaseSdrWhite /
    // DecreaseSdrWhite) or by editing this value and saving. Default mirrors
    // hdr_shaders::SDR_WHITE_NITS (BT.2408 reference SDR white); resolved
    // value is clamped to [80, 300] in `Config::resolve`.
    #[serde(default = "default_sdr_white_nits")]
    sdr_white_nits: f32,
    keybinds: HashMap<String, NavAction>,
    // Optional: a config omitting `startup` parses fine (empty list = run nothing).
    #[serde(default)]
    startup: Vec<String>,
    // Optional: a config omitting `[[output]]` entirely parses fine (empty list =
    // auto left-to-right layout for every connector; see udev.rs). Field name is
    // singular to match the `[[output]]` TOML array-of-tables header exactly (no
    // serde rename); the resolved `Config::outputs` is plural since it's built
    // manually in resolve(), not deserialized directly.
    #[serde(default)]
    output: Vec<RawOutput>,
}

#[derive(Deserialize)]
struct RawOutput {
    name: String,
    position: [i32; 2],
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    primary: bool,
    #[serde(default)]
    transform: Option<String>,
    #[serde(default)]
    hdr: bool,
}

#[derive(Deserialize)]
struct RawLayout {
    visible_columns: usize,
    // Gaps are optional: a config predating them (or a minimal one) still parses,
    // falling back to the same values the layout was hardcoded to before config wiring.
    #[serde(default = "default_outer_gap")]
    outer_gap: u32,
    #[serde(default = "default_inner_gap")]
    inner_gap: u32,
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

// Optional section: a config omitting `[input]` parses fine, taking the
// defaults below.
#[derive(Default, Deserialize)]
struct RawInput {
    // Off by default: click-to-focus is the conventional expectation, and
    // focus-follows-mouse changes what every keystroke does, so it is opt-in.
    #[serde(default)]
    focus_follows_mouse: bool,
}

fn default_duration_ms() -> u64 {
    250
}

fn default_outer_gap() -> u32 {
    20
}

fn default_inner_gap() -> u32 {
    10
}

fn default_sdr_white_nits() -> f32 {
    crate::hdr_shaders::SDR_WHITE_NITS
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

        let outputs = raw
            .output
            .into_iter()
            .map(|o| OutputConfig {
                name: o.name,
                position: (o.position[0], o.position[1]),
                mode: o.mode.as_deref().and_then(parse_mode),
                primary: o.primary,
                transform: o.transform.as_deref().and_then(parse_transform).unwrap_or(Transform::Normal),
                hdr: o.hdr,
            })
            .collect();

        Config {
            visible_columns: raw.layout.visible_columns,
            outer_gap: raw.layout.outer_gap,
            inner_gap: raw.layout.inner_gap,
            keybinds,
            animation_duration: Duration::from_millis(raw.animation.duration_ms),
            startup: raw.startup,
            outputs,
            sdr_white_nits: raw.sdr_white_nits.clamp(80.0, 300.0),
            focus_follows_mouse: raw.input.focus_follows_mouse,
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

/// Parse a mode string like `"1280x400"` into `(width, height)`. Returns `None`
/// (with a warning) on anything that doesn't split cleanly into two ints on
/// 'x' -- an unparseable mode falls back to the output's preferred mode rather
/// than failing config resolution.
fn parse_mode(mode: &str) -> Option<(i32, i32)> {
    let Some((w, h)) = mode.split_once('x') else {
        tracing::warn!("malformed output mode '{mode}'; falling back to preferred mode");
        return None;
    };
    match (w.trim().parse::<i32>(), h.trim().parse::<i32>()) {
        (Ok(w), Ok(h)) => Some((w, h)),
        _ => {
            tracing::warn!("malformed output mode '{mode}'; falling back to preferred mode");
            None
        }
    }
}

/// Parse a `transform` string (e.g. `"270"`, `"flipped-90"`, `"right"`) into a
/// [`Transform`]. Unrecognized strings warn and return `None`, falling back to
/// `Transform::Normal` at the call site rather than failing config resolution.
fn parse_transform(transform: &str) -> Option<Transform> {
    match transform {
        "normal" | "0" => Some(Transform::Normal),
        "90" => Some(Transform::_90),
        "180" => Some(Transform::_180),
        "270" => Some(Transform::_270),
        "flipped" => Some(Transform::Flipped),
        "flipped-90" => Some(Transform::Flipped90),
        "flipped-180" => Some(Transform::Flipped180),
        "flipped-270" => Some(Transform::Flipped270),
        // NOTE: xrandr-style aliases for how Max thinks about rotation direction.
        // This CW/CCW mapping is a best guess and has NOT been hardware-verified;
        // if `left`/`right` come out rotated the wrong way on real hardware, swap
        // these two arms (_90 <-> _270).
        "left" => Some(Transform::_90),
        "right" => Some(Transform::_270),
        _ => {
            tracing::warn!("unrecognized output transform '{transform}'; falling back to normal");
            None
        }
    }
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
