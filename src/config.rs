use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use smithay::input::keyboard::{
    xkb::{keysym_from_name, KEYSYM_NO_FLAGS},
    ModifiersState,
};
use smithay::backend::renderer::Color32F;
use smithay::utils::Transform;

use crate::input::NavAction;

#[path = "config_loader.rs"]
mod config_loader;

/// Built-in fallback, used when no user config exists or one fails to parse.
/// Also serves as the copy-paste reference at `config/default/`.
///
/// Split across files the same way a user's config is, and merged through the
/// real loader rather than concatenated: a bare top-level key (`startup`,
/// `sdr_white_nits`) appearing after another file's `[section]` header would
/// silently parse as a member of that section. Merging also means every test
/// that touches the defaults exercises the merge path.
const DEFAULT_PARTS: [(&str, &str); 4] = [
    ("decoration.toml", include_str!("../config/default/decoration.toml")),
    ("input.toml", include_str!("../config/default/input.toml")),
    ("layout.toml", include_str!("../config/default/layout.toml")),
    ("startup.toml", include_str!("../config/default/startup.toml")),
];

/// The built-in defaults as one merged table. Panics on failure: these files
/// are compiled in, so a problem here is a build-time mistake, not user input.
fn default_config_table() -> toml::Table {
    let parsed: Vec<(&str, toml::Table)> = DEFAULT_PARTS
        .iter()
        .map(|(name, text)| {
            (*name, toml::from_str(text).unwrap_or_else(|e| panic!("built-in {name} must parse: {e}")))
        })
        .collect();
    config_loader::merge_parsed(&parsed)
        .unwrap_or_else(|e| panic!("built-in defaults must merge: {e}"))
        .0
}

/// The built-in defaults, resolved.
fn default_raw_config() -> RawConfig {
    config_loader::deserialize_merged(default_config_table())
        .unwrap_or_else(|e| panic!("built-in defaults must deserialize: {e}"))
}

thread_local! {
    /// Problems noticed while parsing the config, drained by the caller after a
    /// `load`/`reload` so they can be surfaced to the user rather than only logged.
    ///
    /// A thread-local rather than a threaded-through `&mut Vec` because the parse
    /// helpers (`parse_chord`, `parse_mode`, `parse_color_or`, ...) are free
    /// functions called from deep inside `resolve`, and every one of them is a
    /// place where a typo silently drops a setting. Both `load` and `reload` run
    /// entirely on the compositor thread, so there is no cross-thread interleaving
    /// to worry about.
    static DIAGNOSTICS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// Record a config problem: logged exactly as before, *and* kept for the user.
///
/// Every caller is a spot where the config asked for something we could not honor
/// and we fell back instead -- which is precisely the class of failure that used
/// to vanish into the log.
pub(crate) fn note_config_problem(message: String) {
    tracing::warn!("{message}");
    DIAGNOSTICS.with(|d| d.borrow_mut().push(message));
}

/// Take everything noted since the last drain. Call once after `load`/`reload`.
pub(crate) fn take_config_diagnostics() -> Vec<String> {
    DIAGNOSTICS.with(|d| std::mem::take(&mut *d.borrow_mut()))
}

/// Where config problems are surfaced, beyond always being logged.
///
/// Defaults to `Osd` because a status bar is not guaranteed to exist, let alone to
/// have somewhere to put a notification -- whereas a desktop notification is the
/// one channel a fresh install can reasonably assume.
#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ConfigErrorSink {
    /// `notify-send`, if it and a notification daemon are present.
    #[default]
    Osd,
    /// A `config_error` message pushed to every IPC subscriber (see ipc.rs).
    Ipc,
    Both,
    /// Log only -- the pre-existing behavior.
    Silent,
}

impl ConfigErrorSink {
    fn osd(self) -> bool {
        matches!(self, ConfigErrorSink::Osd | ConfigErrorSink::Both)
    }
    fn ipc(self) -> bool {
        matches!(self, ConfigErrorSink::Ipc | ConfigErrorSink::Both)
    }
}

/// Resolved `[diagnostics]` section.
#[derive(Clone, Copy, Debug, Default)]
pub struct DiagnosticsConfig {
    pub config_errors: ConfigErrorSink,
}

impl DiagnosticsConfig {
    pub fn wants_osd(&self) -> bool {
        self.config_errors.osd()
    }
    pub fn wants_ipc(&self) -> bool {
        self.config_errors.ipc()
    }
}

// Optional section: a config omitting `[diagnostics]` parses fine and gets `Osd`.
#[derive(Default, Deserialize)]
struct RawDiagnostics {
    #[serde(default)]
    config_errors: ConfigErrorSink,
}

/// Runtime configuration, resolved from `config.toml` (or the built-in default).
/// Chords have already been parsed into concrete modifier/keysym matches here --
/// the raw string form only exists during deserialization.
pub struct Config {
    pub visible_columns: usize,
    /// How config problems reach the user. Live/hot-reloadable, and deliberately
    /// swapped *before* the diagnostics from that same reload are reported, so
    /// changing the sink takes effect on the edit that changes it.
    pub diagnostics: DiagnosticsConfig,
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
    /// Desktop wallpaper. Live/hot-reloadable, and also settable at runtime
    /// over IPC (`set_wallpaper`); see src/wallpaper.rs.
    pub wallpaper: WallpaperConfig,
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

    /// Border appearance. Live/hot-reloadable like the rest of the config;
    /// the render path reads it fresh every frame.
    pub decoration: DecorationConfig,

    /// Self-contained idle screen-off timer -- Rubix's own DPMS-equivalent,
    /// independent of any external daemon (gestureControl, swayidle, ...).
    /// Live/hot-reloadable: `RubixState::reload_config` re-arms the idle
    /// timer against the new `screen_off_seconds` immediately.
    pub idle: IdleConfig,
}

/// Resolved `[idle]`. See `config/default/` for the on-disk defaults.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IdleConfig {
    /// Master switch. `false` disables the timer outright, independent of
    /// `screen_off_seconds`.
    pub enabled: bool,
    /// Seconds of no keyboard/pointer activity before Rubix powers every
    /// output off. `0` disables the timer regardless of `enabled` -- treated
    /// as "no timeout" rather than "blank instantly", matching how `0` reads
    /// as "off" for `border_width`/`corner_radius` above.
    pub screen_off_seconds: u64,
}

/// Resolved wallpaper settings. An absent `path` means no wallpaper: outputs
/// clear to black, which is what every config predating this section did.
#[derive(Debug, Clone, PartialEq)]
pub struct WallpaperConfig {
    pub path: Option<PathBuf>,
    pub mode: crate::wallpaper::WallpaperMode,
    /// Linear-light gain applied to the image at decode time. 1.0 leaves it
    /// untouched; 0.5 halves its luminance. Exists because an image graded far
    /// above a panel's peak reads as uniformly too bright rather than as more
    /// dynamic range. Clamped to [0.05, 4.0] in `resolve`.
    pub luminance_scale: f32,
    /// Seconds each image is shown when `path` names a directory. Ignored for a
    /// single file. Minimum 1.
    pub interval_seconds: u64,
    /// What luminance (cd/m²) in the source image counts as white when the
    /// wallpaper is tone-mapped onto an SDR output. Measured against the file's
    /// own grading, before `luminance_scale` is applied, so the HDR and SDR
    /// renderings can be tuned independently. Only the wallpaper uses this;
    /// ordinary HDR windows keep `sdr_white_nits`. Clamped to [80, 10000].
    pub sdr_reference_nits: f32,
}

impl Default for WallpaperConfig {
    fn default() -> Self {
        WallpaperConfig {
            path: None,
            mode: crate::wallpaper::WallpaperMode::default(),
            luminance_scale: 1.0,
            interval_seconds: 300,
            sdr_reference_nits: crate::hdr_shaders::SDR_WHITE_NITS,
        }
    }
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
    /// Wallpaper for this output specifically, overriding `[wallpaper] path`.
    /// `None` falls back to the global one; see src/wallpaper.rs.
    pub wallpaper: Option<PathBuf>,
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
    // Optional section: a config omitting `[diagnostics]` surfaces config problems
    // as desktop notifications (see `ConfigErrorSink`).
    #[serde(default)]
    diagnostics: RawDiagnostics,
    // Optional section: a config omitting `[decoration]` gets the built-in
    // border defaults (2px, Catppuccin blue/surface1, no HDR luminance).
    #[serde(default)]
    decoration: RawDecoration,
    // Top-level scalar (must sit before the first [table] header in the TOML
    // file -- see config/default/). Live/hot-reloadable: HDR Phase 4's
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
    // Optional section: a config omitting [wallpaper] draws no wallpaper at all
    // (the desktop clears to black), which is what every pre-wallpaper config
    // already did.
    #[serde(default)]
    wallpaper: RawWallpaper,
    // Optional section: a config omitting [idle] gets the defaults below
    // (enabled, 600s) -- so a fresh install blanks its OLED panel out of the
    // box with no separate setup.
    #[serde(default)]
    idle: RawIdle,
}

#[derive(Deserialize, Default)]
struct RawWallpaper {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    mode: crate::wallpaper::WallpaperMode,
    #[serde(default = "default_luminance_scale")]
    luminance_scale: f32,
    #[serde(default = "default_interval_seconds")]
    interval_seconds: u64,
    #[serde(default = "default_sdr_reference_nits")]
    sdr_reference_nits: f32,
}

fn default_sdr_reference_nits() -> f32 {
    crate::hdr_shaders::SDR_WHITE_NITS
}

fn default_interval_seconds() -> u64 {
    300
}

fn default_luminance_scale() -> f32 {
    1.0
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
    #[serde(default)]
    wallpaper: Option<String>,
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

// Optional section: a config omitting `[idle]` parses fine, taking the
// defaults below (enabled, 600s) -- see `IdleConfig`'s doc comment.
#[derive(Deserialize)]
struct RawIdle {
    #[serde(default = "default_idle_enabled")]
    enabled: bool,
    #[serde(default = "default_screen_off_seconds")]
    screen_off_seconds: u64,
}

impl Default for RawIdle {
    fn default() -> Self {
        RawIdle { enabled: default_idle_enabled(), screen_off_seconds: default_screen_off_seconds() }
    }
}

fn default_idle_enabled() -> bool {
    true
}

fn default_screen_off_seconds() -> u64 {
    600
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
    /// Load and resolve the config: every `*.toml` file under the user's
    /// config directory, deep-merged (see `config_loader`), if present and
    /// valid, else the built-in default. Never panics on a missing or
    /// malformed user config -- the compositor must still come up.
    pub fn load() -> Self {
        if let Some(dir) = config_dir() {
            let files = config_loader::discover_config_files(&dir);
            match config_loader::merge_all(&dir, &files) {
                Ok((table, prov)) => {
                    for msg in config_loader::check_unknown_keys(&table, &prov) {
                        note_config_problem(msg);
                    }
                    match config_loader::deserialize_merged(table) {
                        Ok(raw) => return Self::resolve(raw),
                        Err(e) => note_config_problem(format!("{e}; using built-in defaults")),
                    }
                }
                Err(e) => note_config_problem(format!("{e}; using built-in defaults")),
            }
        } else {
            note_config_problem("no config directory found; using built-in defaults".to_string());
        }
        Self::resolve(default_raw_config())
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
                wallpaper: o.wallpaper.map(expand_tilde),
            })
            .collect();

        // Clamped once and reused: `decoration`'s backdrop ceiling defaults to
        // window white, and must track the *clamped* value rather than the raw
        // one so an out-of-range `sdr_white_nits` cannot drag the backdrop
        // reference somewhere the display never goes.
        let sdr_white_nits = raw.sdr_white_nits.clamp(80.0, 300.0);
        Config {
            visible_columns: raw.layout.visible_columns,
            diagnostics: DiagnosticsConfig { config_errors: raw.diagnostics.config_errors },
            outer_gap: raw.layout.outer_gap,
            inner_gap: raw.layout.inner_gap,
            keybinds,
            animation_duration: Duration::from_millis(raw.animation.duration_ms),
            startup: raw.startup,
            outputs,
            wallpaper: WallpaperConfig {
                path: raw.wallpaper.path.map(expand_tilde),
                mode: raw.wallpaper.mode,
                // Clamped rather than rejected: this is a value users tune by
                // nudging it, and a typo'd 40 should darken to the floor, not
                // decode a black desktop or refuse to load the config.
                luminance_scale: raw.wallpaper.luminance_scale.clamp(0.05, 4.0),
                // Floored at 1: a zero interval would re-decode continuously.
                interval_seconds: raw.wallpaper.interval_seconds.max(1),
                sdr_reference_nits: raw.wallpaper.sdr_reference_nits.clamp(80.0, 10000.0),
            },
            sdr_white_nits,
            focus_follows_mouse: raw.input.focus_follows_mouse,
            decoration: resolve_decoration(raw.decoration, sdr_white_nits),
            idle: IdleConfig {
                enabled: raw.idle.enabled,
                screen_off_seconds: raw.idle.screen_off_seconds,
            },
        }
    }

    /// Re-read and re-merge every `*.toml` file under the user's config
    /// directory for hot-reload. Returns `None` if there's nothing to load or
    /// the merge/parse fails -- the caller keeps its current config
    /// (keep-last-good), so a broken edit never disturbs the running session.
    /// Distinct from [`load`](Self::load), which substitutes the built-in
    /// default at startup: on reload we deliberately do *not* fall back to
    /// default, preserving the last set of working binds.
    pub fn reload() -> Option<Config> {
        let dir = config_dir()?;
        let files = config_loader::discover_config_files(&dir);
        let (table, prov) = config_loader::merge_all(&dir, &files)
            .map_err(|e| note_config_problem(format!("config reload failed ({e}); keeping current config")))
            .ok()?;
        for msg in config_loader::check_unknown_keys(&table, &prov) {
            note_config_problem(msg);
        }
        let raw = config_loader::deserialize_merged(table)
            .map_err(|e| note_config_problem(format!("config reload failed ({e}); keeping current config")))
            .ok()?;
        Some(Config::resolve(raw))
    }
}

/// `$XDG_CONFIG_HOME/rubix`, falling back to `~/.config/rubix`. The root
/// directory recursively walked for `*.toml` config files.
pub fn config_dir() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("rubix"));
        }
    }
    std::env::var("HOME").ok().map(|home| PathBuf::from(home).join(".config/rubix"))
}

/// Directory to hand the file watcher: the canonicalized config root, so a
/// stow symlink (e.g. `~/.config/rubix` -> the dotfiles repo) is followed to
/// the real directory -- watching the symlink's own parent would miss writes
/// to the target tree. Watched recursively by the caller, since config files
/// can live in nested subdirectories. Returns `None` when the config
/// directory doesn't exist at all -- the compiled-in default can't be
/// hot-reloaded, so there is simply nothing to watch.
pub fn config_watch_target() -> Option<PathBuf> {
    config_dir()?.canonicalize().ok()
}

/// True when a filesystem event warrants a config reload: it touches a
/// `*.toml` file and is a content, create, or delete change. Bare metadata
/// touches are filtered out -- our own read bumps the file's access time, and
/// reacting to that would feed back into an endless reload loop
/// (self-limiting under `relatime`, but cheap to rule out regardless).
pub fn should_reload(event: &calloop_notify::notify::Event) -> bool {
    use calloop_notify::notify::event::{EventKind, ModifyKind};

    let touches_config = event.paths.iter().any(|p| config_loader::has_config_extension(p));
    let is_write = match &event.kind {
        EventKind::Create(_) | EventKind::Remove(_) => true,
        EventKind::Modify(kind) => !matches!(kind, ModifyKind::Metadata(_)),
        _ => false,
    };
    touches_config && is_write
}

/// Parse a mode string like `"1280x400"` into `(width, height)`. Returns `None`
/// (with a warning) on anything that doesn't split cleanly into two ints on
/// 'x' -- an unparseable mode falls back to the output's preferred mode rather
/// than failing config resolution.
fn parse_mode(mode: &str) -> Option<(i32, i32)> {
    let Some((w, h)) = mode.split_once('x') else {
        note_config_problem(format!("malformed output mode '{mode}'; falling back to preferred mode"));
        return None;
    };
    match (w.trim().parse::<i32>(), h.trim().parse::<i32>()) {
        (Ok(w), Ok(h)) => Some((w, h)),
        _ => {
            note_config_problem(format!("malformed output mode '{mode}'; falling back to preferred mode"));
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
            note_config_problem(format!("unrecognized output transform '{transform}'; falling back to normal"));
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
                    note_config_problem(format!("unknown key '{token}' in chord '{chord}'; ignoring bind"));
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


// ---- decoration (HDR Phase 5b) ----

/// How one window is dressed in one focus state: its border's appearance and
/// its own opacity.
///
/// Named for the window rather than the border because it no longer describes
/// only the border -- `opacity` applies to the window's own surface. Both are
/// selected by the same rules (class, title, focus), so they share a struct
/// rather than duplicating the matching.
///
/// `luminance_nits: None` means "no opinion" -- the border sits at SDR white
/// like every other SDR element, which is also what happens on any non-HDR
/// output regardless of what this says.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowStyle {
    pub color: Color32F,
    pub luminance_nits: Option<f32>,
    /// How far the glow reaches beyond the ring, in logical pixels. `0`
    /// (the default) draws a plain ring with no glow at all.
    pub glow_margin: u32,
    /// Shapes how quickly the glow gives out. `1.0` is linear; higher values
    /// keep the glow tight to the border, lower values spread it.
    pub glow_falloff: f32,
    /// The window's own opacity, `0.0`-`1.0`. `1.0` (the default) leaves the
    /// window untouched and preserves every optimization an opaque window
    /// gets.
    pub opacity: f32,
    /// Cap the backdrop quad's highlights at `sdr_reference_nits` instead of
    /// letting the raw HDR wallpaper show through. `false` (the default)
    /// preserves existing behaviour.
    pub backdrop_tonemap: bool,
    /// Sample the pre-blurred backdrop buffer instead of the sharp wallpaper.
    /// `false` (the default) preserves existing behaviour.
    pub backdrop_blur: bool,
    /// Refract the backdrop quad through a faceted crystal-like field instead
    /// of showing the wallpaper flat. `false` (the default) preserves existing
    /// behaviour.
    pub refract: bool,
}

impl Default for DecorationConfig {
    /// The resolved defaults, for callers (tests, mostly) that want a
    /// baseline `DecorationConfig` without going through TOML parsing.
    fn default() -> Self {
        resolve_decoration(RawDecoration::default(), default_sdr_white_nits())
    }
}

/// Resolved `[decoration]`. `active`/`inactive` are the fallbacks; `rules` are
/// consulted first, in config order.
pub struct DecorationConfig {
    /// Logical pixels, drawn outside the client rect. `0` disables borders
    /// entirely and short-circuits all decoration work in the render path.
    pub border_width: u32,
    /// Corner radius in logical pixels. `0` (the default) disables rounding
    /// entirely and keeps the batched, un-attributed element path every
    /// backend used before rounding existed.
    pub corner_radius: u32,
    pub active: WindowStyle,
    pub inactive: WindowStyle,
    pub rules: Vec<BorderRule>,
    /// Opacity for a window covered by others. `1.0` (the default) disables
    /// the whole occlusion check, including its per-frame geometry work.
    pub obscured_opacity: f32,
    /// How covered a window must be, 0.0-1.0, before it counts as obscured.
    pub obscured_threshold: f32,
    /// Blur radius for the backdrop, in logical pixels. Global rather than
    /// per-rule: each distinct radius needs its own precomputed buffer, and a
    /// 4K 16-bit RGBA wallpaper is already ~66 MB. `0` disables blur
    /// entirely and skips the blur work at decode time.
    pub backdrop_blur_radius: u32,
    /// Ceiling, in nits, for the backdrop quad's highlights. Separate from
    /// `sdr_reference_nits`, which normalises the *whole* wallpaper for an SDR
    /// output and is deliberately large; this one answers a different
    /// question -- "how bright may what shows through a window get" -- and its
    /// natural answer is window white. Defaults to the live `sdr_white_nits`.
    ///
    /// The roll-off knee sits at `0.8x` this value, so a setting above the
    /// wallpaper's actual peak luminance makes the cap an exact no-op.
    /// Clamped to `[10.0, 10000.0]` -- the floor is far below
    /// `sdr_white_nits`'s own `[80, 300]` because useful values here are
    /// expected to sit well *under* window white.
    pub backdrop_luminance_nits: f32,
    /// Peak ray offset for the crystal-facet backdrop refraction, in logical
    /// pixels. Scaled by the output scale before it reaches the shader.
    /// Clamped to `[0.0, 200.0]`.
    pub refract_strength: f32,
    /// Facet cell size for the backdrop refraction, in logical pixels.
    /// Scaled by the output scale before it reaches the shader. Clamped to
    /// `[4.0, 2000.0]`.
    pub refract_facet_size: f32,
    /// Per-channel dispersion spread, `0.0`-`1.0`, before the seam
    /// amplification the shader applies. Clamped to `[0.0, 1.0]`.
    pub refract_dispersion: f32,
}

/// A conditional override. Both matchers are case-insensitive substring tests,
/// and a rule with neither matcher set matches every window (useful as a
/// catch-all placed last). When both are set, both must match.
pub struct BorderRule {
    pub app_id: Option<String>,
    pub title: Option<String>,
    pub active: StyleOverride,
    pub inactive: StyleOverride,
}

/// The fields one rule sets, each independently optional.
///
/// Every field is separate so a rule can change exactly one thing -- setting
/// only `active_opacity` should dim a window without also deciding its border
/// colour. An earlier design required a rule to name a colour before any of its
/// other fields applied, which meant an opacity-only rule silently did nothing.
///
/// `luminance_nits` is doubly optional on purpose: the outer layer is "did the
/// rule mention it", the inner is "did it ask for no opinion" (a zero or
/// negative value). Collapsing them would make `= 0` indistinguishable from
/// omitting the key.
#[derive(Default)]
pub struct StyleOverride {
    pub color: Option<Color32F>,
    pub luminance_nits: Option<Option<f32>>,
    pub glow_margin: Option<u32>,
    pub glow_falloff: Option<f32>,
    pub opacity: Option<f32>,
    pub backdrop_tonemap: Option<bool>,
    pub backdrop_blur: Option<bool>,
    pub refract: Option<bool>,
}

impl StyleOverride {
    fn apply(&self, style: &mut WindowStyle) {
        if let Some(color) = self.color {
            style.color = color;
        }
        if let Some(nits) = self.luminance_nits {
            style.luminance_nits = nits;
        }
        if let Some(glow) = self.glow_margin {
            style.glow_margin = glow;
        }
        if let Some(falloff) = self.glow_falloff {
            style.glow_falloff = falloff;
        }
        if let Some(opacity) = self.opacity {
            style.opacity = opacity;
        }
        if let Some(tonemap) = self.backdrop_tonemap {
            style.backdrop_tonemap = tonemap;
        }
        if let Some(blur) = self.backdrop_blur {
            style.backdrop_blur = blur;
        }
        if let Some(refract) = self.refract {
            style.refract = refract;
        }
    }
}

impl BorderRule {
    fn matches(&self, app_id: Option<&str>, title: Option<&str>) -> bool {
        fn contains_ignore_case(haystack: Option<&str>, needle: &str) -> bool {
            haystack.is_some_and(|h| h.to_lowercase().contains(&needle.to_lowercase()))
        }
        // A matcher that is set but does not match rejects the rule; a matcher
        // that is unset is simply not a constraint.
        self.app_id.as_ref().is_none_or(|n| contains_ignore_case(app_id, n))
            && self.title.as_ref().is_none_or(|n| contains_ignore_case(title, n))
    }
}

impl DecorationConfig {
    /// The style for one window: the section defaults, with every matching
    /// rule layered over them in file order.
    ///
    /// Rules accumulate rather than the first match winning, and each rule
    /// touches only the fields it names. That makes a broad rule plus a narrow
    /// exception work the obvious way -- dim every window, then a later rule
    /// pins one class back to fully opaque without having to restate its
    /// colours.
    pub fn style_for(&self, app_id: Option<&str>, title: Option<&str>, focused: bool) -> WindowStyle {
        let mut style = if focused { self.active.clone() } else { self.inactive.clone() };
        for rule in &self.rules {
            if rule.matches(app_id, title) {
                let over = if focused { &rule.active } else { &rule.inactive };
                over.apply(&mut style);
            }
        }
        style
    }
}

#[derive(Deserialize)]
struct RawDecoration {
    #[serde(default = "default_border_width")]
    border_width: u32,
    #[serde(default)]
    corner_radius: u32,
    #[serde(default = "default_active_color")]
    active_color: String,
    #[serde(default = "default_inactive_color")]
    inactive_color: String,
    #[serde(default)]
    active_luminance_nits: Option<f32>,
    #[serde(default)]
    inactive_luminance_nits: Option<f32>,
    #[serde(default)]
    active_glow_margin: u32,
    #[serde(default)]
    inactive_glow_margin: u32,
    #[serde(default = "default_glow_falloff")]
    glow_falloff: f32,
    #[serde(default = "default_opacity")]
    active_opacity: f32,
    #[serde(default = "default_opacity")]
    inactive_opacity: f32,
    #[serde(default = "default_opacity")]
    obscured_opacity: f32,
    #[serde(default = "default_obscured_threshold")]
    obscured_threshold: f32,
    #[serde(default)]
    active_backdrop_tonemap: bool,
    #[serde(default)]
    inactive_backdrop_tonemap: bool,
    #[serde(default)]
    active_backdrop_blur: bool,
    #[serde(default)]
    inactive_backdrop_blur: bool,
    #[serde(default = "default_backdrop_blur_radius")]
    backdrop_blur_radius: u32,
    // `None` rather than a default fn: the fallback is the live
    // `sdr_white_nits`, which lives on a different struct and is not knowable
    // here. Resolved in `resolve_decoration`.
    #[serde(default)]
    backdrop_luminance_nits: Option<f32>,
    #[serde(default)]
    active_refract: bool,
    #[serde(default)]
    inactive_refract: bool,
    #[serde(default = "default_refract_strength")]
    refract_strength: f32,
    #[serde(default = "default_refract_facet_size")]
    refract_facet_size: f32,
    #[serde(default = "default_refract_dispersion")]
    refract_dispersion: f32,
    // Singular to match the `[[decoration.rule]]` array-of-tables header
    // exactly, same convention as `[[output]]`.
    #[serde(default)]
    rule: Vec<RawBorderRule>,
}

impl Default for RawDecoration {
    fn default() -> Self {
        RawDecoration {
            border_width: default_border_width(),
            corner_radius: 0,
            active_color: default_active_color(),
            inactive_color: default_inactive_color(),
            active_luminance_nits: None,
            inactive_luminance_nits: None,
            active_glow_margin: 0,
            inactive_glow_margin: 0,
            glow_falloff: default_glow_falloff(),
            active_opacity: default_opacity(),
            inactive_opacity: default_opacity(),
            obscured_opacity: default_opacity(),
            obscured_threshold: default_obscured_threshold(),
            active_backdrop_tonemap: false,
            inactive_backdrop_tonemap: false,
            active_backdrop_blur: false,
            inactive_backdrop_blur: false,
            backdrop_blur_radius: default_backdrop_blur_radius(),
            backdrop_luminance_nits: None,
            active_refract: false,
            inactive_refract: false,
            refract_strength: default_refract_strength(),
            refract_facet_size: default_refract_facet_size(),
            refract_dispersion: default_refract_dispersion(),
            rule: Vec::new(),
        }
    }
}

#[derive(Deserialize)]
struct RawBorderRule {
    #[serde(default)]
    app_id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    active_color: Option<String>,
    #[serde(default)]
    inactive_color: Option<String>,
    #[serde(default)]
    active_luminance_nits: Option<f32>,
    #[serde(default)]
    inactive_luminance_nits: Option<f32>,
    #[serde(default)]
    active_glow_margin: Option<u32>,
    #[serde(default)]
    inactive_glow_margin: Option<u32>,
    #[serde(default)]
    glow_falloff: Option<f32>,
    #[serde(default)]
    active_opacity: Option<f32>,
    #[serde(default)]
    inactive_opacity: Option<f32>,
    #[serde(default)]
    active_backdrop_tonemap: Option<bool>,
    #[serde(default)]
    inactive_backdrop_tonemap: Option<bool>,
    #[serde(default)]
    active_backdrop_blur: Option<bool>,
    #[serde(default)]
    inactive_backdrop_blur: Option<bool>,
    #[serde(default)]
    active_refract: Option<bool>,
    #[serde(default)]
    inactive_refract: Option<bool>,
}

fn default_border_width() -> u32 {
    2
}

/// Slightly super-linear, so the glow stays close to the border rather than
/// washing across the gap. Subtlety in HDR comes from headroom, not spread.
fn default_glow_falloff() -> f32 {
    2.0
}

fn default_opacity() -> f32 {
    1.0
}

fn default_backdrop_blur_radius() -> u32 {
    32
}

/// Peak ray offset for the crystal-facet backdrop refraction, in logical
/// pixels. Subtle enough to read as glass, not as a funhouse mirror.
fn default_refract_strength() -> f32 {
    12.0
}

/// Facet cell size for the crystal-facet backdrop refraction, in logical
/// pixels. Coarse enough that individual facets are visible on an ordinary
/// window rather than dissolving into noise.
fn default_refract_facet_size() -> f32 {
    90.0
}

/// Per-channel dispersion spread for the crystal-facet backdrop refraction,
/// before the seam amplification the shader applies.
fn default_refract_dispersion() -> f32 {
    0.35
}

/// Nearly-covered rather than entirely covered, so a window peeking out by a
/// few pixels behind a maximized neighbour still fades.
fn default_obscured_threshold() -> f32 {
    0.9
}

fn default_active_color() -> String {
    "#89b4fa".to_string()
}

fn default_inactive_color() -> String {
    "#45475a".to_string()
}

/// Parse `#RGB`, `#RRGGBB`, or `#RRGGBBAA` (the leading `#` optional) into a
/// straight-alpha sRGB color. Returns `None` on anything else so the caller
/// can warn and fall back rather than taking a malformed color as authoritative.
///
/// Note the components stay sRGB-*encoded* here. Linearization happens in the
/// render path, where it can be paired with the luminance scaling -- doing it
/// at parse time would make `WindowStyle::color` mean something different from
/// what the user typed.
pub(crate) fn parse_color(text: &str) -> Option<Color32F> {
    let hex = text.strip_prefix('#').unwrap_or(text);
    let component = |i: usize, len: usize| -> Option<f32> {
        let slice = &hex.get(i * len..(i + 1) * len)?;
        let value = u8::from_str_radix(&slice.repeat(2 / len), 16).ok()?;
        Some(value as f32 / 255.0)
    };
    match hex.len() {
        3 => Some(Color32F::new(component(0, 1)?, component(1, 1)?, component(2, 1)?, 1.0)),
        6 => Some(Color32F::new(component(0, 2)?, component(1, 2)?, component(2, 2)?, 1.0)),
        8 => Some(Color32F::new(
            component(0, 2)?,
            component(1, 2)?,
            component(2, 2)?,
            component(3, 2)?,
        )),
        _ => None,
    }
}

/// Luminance is only meaningful as a positive absolute value. A zero or
/// negative entry is read as "no opinion" rather than as a request to make the
/// border black, which is almost certainly not what someone typing `0` meant.
/// Expand a leading `~` to `$HOME`.
///
/// Wallpaper paths are the first config value a user naturally writes with a
/// tilde -- every other path in this config is compositor-managed. An
/// unexpandable `~` is left as-is so the eventual "no such file" names the path
/// the user actually typed.
fn expand_tilde(path: String) -> PathBuf {
    let Some(rest) = path.strip_prefix("~/") else {
        return PathBuf::from(path);
    };
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(rest),
        None => PathBuf::from(path),
    }
}

fn resolve_luminance(nits: Option<f32>) -> Option<f32> {
    nits.filter(|n| *n > 0.0)
}

/// Opacity outside 0..=1 is meaningless rather than interesting: below zero is
/// the same as zero, above one is the same as one, and a NaN would poison the
/// blend. Clamped here so the render path never has to think about it.
fn resolve_opacity(value: f32) -> f32 {
    if value.is_nan() { 1.0 } else { value.clamp(0.0, 1.0) }
}

fn resolve_color(text: &str, fallback: Color32F) -> Color32F {
    match parse_color(text) {
        Some(color) => color,
        None => {
            note_config_problem(format!("unparseable border color {text:?}; falling back"));
            fallback
        }
    }
}

fn resolve_decoration(raw: RawDecoration, sdr_white_nits: f32) -> DecorationConfig {
    let active_fallback = parse_color(&default_active_color()).expect("built-in color parses");
    let inactive_fallback = parse_color(&default_inactive_color()).expect("built-in color parses");

    let rules = raw
        .rule
        .into_iter()
        .map(|r| {
            let color = |text: Option<String>, fallback: Color32F| {
                text.map(|c| resolve_color(&c, fallback))
            };
            BorderRule {
                active: StyleOverride {
                    color: color(r.active_color, active_fallback),
                    luminance_nits: r.active_luminance_nits.map(|n| resolve_luminance(Some(n))),
                    glow_margin: r.active_glow_margin,
                    glow_falloff: r.glow_falloff,
                    opacity: r.active_opacity.map(resolve_opacity),
                    backdrop_tonemap: r.active_backdrop_tonemap,
                    backdrop_blur: r.active_backdrop_blur,
                    refract: r.active_refract,
                },
                inactive: StyleOverride {
                    color: color(r.inactive_color, inactive_fallback),
                    luminance_nits: r.inactive_luminance_nits.map(|n| resolve_luminance(Some(n))),
                    glow_margin: r.inactive_glow_margin,
                    glow_falloff: r.glow_falloff,
                    opacity: r.inactive_opacity.map(resolve_opacity),
                    backdrop_tonemap: r.inactive_backdrop_tonemap,
                    backdrop_blur: r.inactive_backdrop_blur,
                    refract: r.inactive_refract,
                },
                app_id: r.app_id,
                title: r.title,
            }
        })
        .collect();

    DecorationConfig {
        border_width: raw.border_width,
        corner_radius: raw.corner_radius,
        active: WindowStyle {
            color: resolve_color(&raw.active_color, active_fallback),
            luminance_nits: resolve_luminance(raw.active_luminance_nits),
            glow_margin: raw.active_glow_margin,
            glow_falloff: raw.glow_falloff,
            opacity: resolve_opacity(raw.active_opacity),
            backdrop_tonemap: raw.active_backdrop_tonemap,
            backdrop_blur: raw.active_backdrop_blur,
            refract: raw.active_refract,
        },
        inactive: WindowStyle {
            color: resolve_color(&raw.inactive_color, inactive_fallback),
            luminance_nits: resolve_luminance(raw.inactive_luminance_nits),
            glow_margin: raw.inactive_glow_margin,
            glow_falloff: raw.glow_falloff,
            opacity: resolve_opacity(raw.inactive_opacity),
            backdrop_tonemap: raw.inactive_backdrop_tonemap,
            backdrop_blur: raw.inactive_backdrop_blur,
            refract: raw.inactive_refract,
        },
        rules,
        obscured_opacity: resolve_opacity(raw.obscured_opacity),
        obscured_threshold: resolve_opacity(raw.obscured_threshold),
        backdrop_blur_radius: raw.backdrop_blur_radius,
        // Falls back to window white: what shows through a window should not
        // out-shine the window itself.
        backdrop_luminance_nits: raw
            .backdrop_luminance_nits
            .unwrap_or(sdr_white_nits)
            .clamp(10.0, 10000.0),
        refract_strength: raw.refract_strength.clamp(0.0, 200.0),
        refract_facet_size: raw.refract_facet_size.clamp(4.0, 2000.0),
        refract_dispersion: raw.refract_dispersion.clamp(0.0, 1.0),
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
