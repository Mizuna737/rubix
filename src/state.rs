use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    sync::Arc,
    time::Instant,
};

use smithay::{
    desktop::{layer_map_for_output, LayerSurface, PopupManager, Space, Window, WindowSurface, WindowSurfaceType},
    input::{
        pointer::CursorImageStatus,
        Seat, SeatState,
    },
    output::Output,
    reexports::{
        calloop::{generic::Generic, EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction},
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_protocols_wlr::output_power_management::v1::server::zwlr_output_power_v1::ZwlrOutputPowerV1,
        wayland_server::{
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
            Display, DisplayHandle,
        },
    },
    utils::{Logical, Point, Rectangle},
    wayland::{
        color::management::ColorManagementState,
        presentation::PresentationState,
        compositor::{CompositorClientState, CompositorState},
        dmabuf::{DmabufGlobal, DmabufState},
        output::OutputManagerState,
        pointer_constraints::{PointerConstraintsState, with_pointer_constraint},
        seat::WaylandFocus,
        relative_pointer::RelativePointerManagerState,
        selection::{data_device::DataDeviceState, wlr_data_control::DataControlState},
        shell::kde::decoration::KdeDecorationState,
        shell::wlr_layer::{Layer as WlrLayer, WlrLayerShellState},
        shell::xdg::decoration::XdgDecorationState,
        shell::xdg::XdgShellState,
        shm::ShmState,
        socket::ListeningSocketSource,
        viewporter::ViewporterState,
        xwayland_shell::XWaylandShellState,
    },
    xwayland::X11Wm,
};

use crate::{
    config::Config,
    input::NavAction,
    model::{
        geometry::Rect,
        grid::Workspace,
        tiling::SplitDirection,
    },
};

// Stashed in an Output's user-data map at bind time (`bind_output_monitor`) so
// any code holding an `Output` can recover which model `Monitor` it drives,
// without a name-based lookup back through config.
#[derive(Clone, Copy)]
pub(crate) struct MonitorId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Pos {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum TweenKind { Enter, Leave, Move }

// The exiting "ghost" trajectory for a wrapping rotate Move: a second copy of
// the same surface, drawn only for the duration of the tween, sliding off the
// near edge while the Space-mapped copy slides in from the far edge.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct GhostTrack {
    from: Pos,
    to: Pos,
}

// The scale channel of a tween, used only by Reveal. `None` means "draw at
// native size", which is every tween the slide transitions produce -- keeping
// it optional is what lets Scroll/Rotate stay on the cheap Space-mapped path
// while Reveal alone takes the render-time rescale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ScaleTrack {
    from: f32,
    to: f32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Tween {
    kind: TweenKind,
    from: Pos,
    to: Pos,
    start: Instant,
    ghost: Option<GhostTrack>,
    scale: Option<ScaleTrack>,
}

// The kind of spatial-nav transition that just happened. Carries the slide
// axis/sign, which cannot be recovered from a set diff.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Transition {
    Scroll { down: bool },
    Rotate,
    // A reveal swap: the group traded into the active slot grows from nothing
    // while the displaced group shrinks away, both in place. Nothing slides,
    // because the two groups are not adjacent -- the displaced one is going to
    // a column that is off-screen by definition, so there is no edge to travel
    // toward that would read as motion rather than a glitch.
    Reveal,
}

/// How much space the maximized window takes, if any.
///
/// Compositor-only state: unlike fullscreen this involves no client protocol
/// negotiation, no connector or scanout involvement. The window keeps its grid
/// slot -- only the rect `apply_layout` hands it changes -- and it releases
/// itself as soon as focus moves elsewhere.
///
/// `ToggleMaximize` cycles `Group` -> `Monitor` -> `None`. `Group` covers only
/// the window's own split tree, hiding its tiling siblings while the rest of
/// the grid stays visible; `Monitor` takes the whole work area. A window alone
/// in its group skips `Group`, where its tile already *is* the group rect and
/// the first press would look dead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaximizeState {
    None,
    Group(u32),
    Monitor(u32),
}

impl MaximizeState {
    /// The window this applies to, if any. The common question -- every caller
    /// outside `apply_layout` cares *whether* a window is blown up, not by how
    /// much.
    pub(crate) fn window(self) -> Option<u32> {
        match self {
            MaximizeState::None => None,
            MaximizeState::Group(id) | MaximizeState::Monitor(id) => Some(id),
        }
    }
}

// The client's DnD icon surface, tracked while a drag is in progress.
// `offset` accumulates the surface's buffer deltas as the client re-attaches
// (handlers/compositor.rs `commit`) so the icon stays put under the cursor
// instead of drifting -- same trick the cursor hotspot uses for the same
// reason.
pub struct DndIcon {
    pub surface: WlSurface,
    pub offset: Point<i32, Logical>,
}

pub struct RubixState {
    pub start_time: std::time::Instant,
    pub socket_name: OsString,
    pub display_handle: DisplayHandle,

    pub space: Space<Window>,
    pub loop_signal: LoopSignal,

    // Backend-neutral VT-switch request. The keyboard filter sets this when it
    // sees an XF86Switch_VT chord; the udev input source consumes it and calls
    // `session.change_vt` (the session lives in the backend, not here). winit
    // never reads it. `Option` so a single chord fires exactly one switch.
    pub pending_vt: Option<i32>,

    // Chords resolved (and modifier-gated -- see input.rs::process_input_event)
    // since the last drain, for main.rs's run-loop callback to hand off to
    // `ipc::broadcast_chord`. Same hand-off shape as `pending_vt`: the input
    // filter doesn't hold the IPC `ClientRegistry`, so it stashes here instead
    // of threading the registry into the input path. A `Vec` (not `Option`,
    // unlike `pending_vt`) because more than one gated chord can land between
    // two dispatch-cycle wake-ups.
    pub pending_chords: Vec<(String, Option<NavAction>)>,

    // User configuration (keybinds + layout), resolved at startup.
    pub config: Config,

    // Live SDR-white-nits value the HDR encode pass reads each frame
    // (render_surface_hdr, threaded through render_surface). Seeded from
    // `config.sdr_white_nits` in `new` and re-seeded in `reload_config`, but
    // also adjustable independently at runtime via the IncreaseSdrWhite /
    // DecreaseSdrWhite keybinds (input.rs dispatch_nav) without touching the
    // config struct. Always in [80, 300].
    pub sdr_white_nits: f32,

    // Rubix model + translation registry.
    // `workspace` is the pure tiling model, one Monitor per bound output;
    // `windows` maps its synthetic u32 ids to live Smithay handles; `next_id`
    // mints those ids (starts at 1 -- 0 is TilingNode::remove_window's
    // transient placeholder, never real).
    pub workspace: Workspace,
    pub windows: HashMap<u32, Window>,
    pub next_id: u32,

    // Wayland toplevels that have been created (initial configure sent) but
    // never committed a buffer yet -- e.g. a headless clipboard reader that
    // creates a toplevel just to read the selection and exits before mapping.
    // Kept OUT of `windows` so the model/focus/IPC never see them; promoted to
    // `windows` on first buffer commit (see xdg_shell::handle_commit).
    pub(crate) unmapped: HashMap<u32, Window>,

    // Set by any mutation that changes cube state (nav dispatch, window
    // map/unmap). The run-loop callback in main.rs checks-and-clears this once
    // per dispatch cycle to coalesce a burst of mutations into a single IPC
    // subscriber push (see ipc.rs).
    pub ipc_dirty: bool,

    // Config problems waiting to go out to IPC subscribers. Drained by the
    // run-loop callback in main.rs alongside the snapshot broadcast, because the
    // client registry lives there rather than on the state. Only ever populated
    // when the resolved sink includes Ipc.
    pub pending_config_errors: Vec<String>,
    /// Decoded wallpapers and their per-output assignment. Populated from
    /// config at startup and on reload, and mutable at runtime over IPC.
    /// See src/wallpaper.rs.
    pub wallpaper: crate::wallpaper::WallpaperManager,

    /// Env vars set on every process spawned from here on, carrying the
    /// latest solved theme (`RUBIX_THEME`/`RUBIX_BACKGROUND`/
    /// `RUBIX_FOREGROUND`/`RUBIX_ACCENT`). Empty when `[theme] enable` is
    /// false. Only affects processes started AFTER a theme change -- an
    /// already-running app needs the file or the IPC event instead, since its
    /// environment cannot be rewritten from outside it. See
    /// `apply_theme_update` and the `NavAction::Spawn`/startup spawn sites.
    pub theme_env: Vec<(String, String)>,

    /// The most recently solved theme, kept whole. `theme_env` and
    /// `theme_border_colors` are derivatives of this and are computed once
    /// at solve time; this is the source they came from, for consumers that
    /// need a role we did not pre-derive -- compositor-drawn chrome asking
    /// for `foreground` or `surface` in abs10k. `None` until the first
    /// wallpaper is themed, and whenever `[theme] enable` is false.
    pub(crate) theme: Option<crate::theme::Theme>,

    /// Border colours from the last solved theme, as `(focused, unfocused)`.
    /// `None` when theming is off, `[theme] apply_to_borders` is false, or no
    /// wallpaper has been themed yet -- in all of which cases the configured
    /// `active_color` / `inactive_color` stand.
    ///
    /// Two roles rather than one shade of a single colour: the focused border
    /// uses `glow`, which is picked to contrast with the wallpaper, while the
    /// unfocused one uses `border`, which is on-hue and muted so it recedes.
    /// Focus stays legible through that chroma gap and the glow margin, so
    /// theming the unfocused ring costs nothing.
    ///
    /// Stored as display sRGB because that is what `WindowStyle::color` is; the
    /// HDR luminance scaling happens later in `decoration::resolved_color`.
    pub(crate) theme_border_colors:
        Option<(smithay::backend::renderer::Color32F, smithay::backend::renderer::Color32F)>,

    /// Rasterizer for compositor-drawn text. `RefCell` because the render
    /// path composes from `&RubixState` while the rasterizer must mutate its
    /// own cache -- the alternative is widening every compose signature to
    /// `&mut` for one cache insert. Single-threaded: the compositor's event
    /// loop is the only borrower, and the only borrow site is the bar.
    pub(crate) text: std::cell::RefCell<crate::text::TextRenderer>,

    // wlr-foreign-toplevel-management: bound managers plus what each window's
    // handles were last told, so `foreign_toplevel::refresh` can send deltas.
    pub(crate) foreign_toplevel: crate::foreign_toplevel::ForeignToplevelState,

    // Last tiling area we laid out into. When a layer surface (bar) changes its
    // exclusive zone, the reserved area shifts; comparing against this lets the
    // layer-commit path reflow existing windows exactly once per change instead
    // of every frame the bar repaints.
    pub reserved_bounds: Option<Rect>,

    animations: HashMap<u32, Tween>,
    pub(crate) pending_transition: Option<Transition>,
    // The exiting-ghost render positions for the in-flight frame, rebuilt fresh
    // by `step_animations` each call. Consumed by the backends right after, to
    // inject a second draw of the wrapping surface. Not Space state.
    pub(crate) active_ghosts: Vec<(u32, Pos)>,
    // Windows mid-Reveal: (id, top-left position, current scale). Rendered
    // outside the Space, wrapped in RescaleRenderElement -- the Space renders
    // in one region-wide call that cannot scale individual elements, so a
    // scaling window has to be unmapped and drawn by hand, the same way
    // rotation ghosts are.
    pub(crate) active_scales: Vec<(u32, Pos, f32)>,

    // Windows currently in fullscreen state (bypass normal tiling).
    pub(crate) fullscreen_windows: HashSet<u32>,

    // X11 windows last reported to their client as iconified. Only what the
    // client was told -- `sync_x11_iconic` diffs against it so an unchanged
    // layout pass sends no property writes.
    pub(crate) iconified: HashSet<u32>,

    // An X11 window that was focused at map time before its `wl_surface` had
    // been associated, so the focus never resolved. `surface_associated`
    // consumes this to complete the handover once the surface exists.
    pub(crate) pending_x11_focus: Option<u32>,

    // How far the focused window is currently blown up, if at all. See
    // [`MaximizeState`].
    pub(crate) maximized: MaximizeState,

    // Track the last configured geometry and fullscreen state for each window
    // to avoid redundant configure resends when layout is recomputed but geometry
    // hasn't changed (prevents flicker during exclusive fullscreen scanout).
    last_configured: HashMap<u32, (Rect, bool)>,

    // Smithay State
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    // Server-side decoration negotiation (see handlers/decoration.rs). Both
    // protocols are advertised because toolkits are split between them; both
    // answer `ServerSide` unconditionally. `xdg_decoration_state` is never
    // read after construction -- the global lives as long as the state does,
    // and the handler works off the toplevel, not the state -- but dropping it
    // would destroy the global.
    #[allow(dead_code)]
    pub xdg_decoration_state: XdgDecorationState,
    pub kde_decoration_state: KdeDecorationState,
    pub layer_shell_state: WlrLayerShellState,
    pub xwayland_shell_state: XWaylandShellState,
    // None until XWaylandEvent::Ready fires and X11Wm::start_wm succeeds.
    pub xwm: Option<X11Wm>,
    // Display number (e.g. `1` for `:1`), stored for logging/env once XWayland is ready.
    pub xdisplay: Option<u32>,
    // Gates `data.config.startup` in the `Ready` handler to a single run.
    // XWayland can now respawn after crashing (see XwmHandler::disconnected in
    // xwayland.rs), so `Ready` firing a second time must NOT relaunch Max's
    // entire startup set on top of his already-running session.
    pub(crate) xwayland_started_once: bool,
    // Crash-loop guard for XWayland respawn: an immediate, repeated exit (bad
    // config, missing binary) must not spin the event loop respawning it
    // forever. Reset to 0 on the next successful `Ready`.
    pub(crate) xwayland_respawn_attempts: u32,
    pub(crate) xwayland_last_exit: Option<std::time::Instant>,
    pub shm_state: ShmState,
    pub viewporter_state: ViewporterState,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<RubixState>,
    pub data_device_state: DataDeviceState,
    pub data_control_state: DataControlState,
    pub popups: PopupManager,
    pub dmabuf_state: DmabufState,
    // Created lazily once the udev backend knows the primary GPU's render
    // formats (in `device_added`); stays None on winit.
    pub dmabuf_global: Option<DmabufGlobal>,
    // Handle back into the udev backend's renderer, so `DmabufHandler::dmabuf_imported`
    // (which fires on `RubixState`) can reach `GpuManager::single_renderer` to
    // validate an imported dmabuf. None on winit.
    pub(crate) udev_handle: Option<std::rc::Rc<std::cell::RefCell<crate::udev::UdevData>>>,

    // Pointer constraints: allows clients to lock or confine the pointer.
    // Used by games for mouselook/aim functionality.
    pub pointer_constraints_state: PointerConstraintsState,
    // Relative pointer: allows clients to receive raw pointer deltas when locked.
    pub relative_pointer_manager_state: RelativePointerManagerState,

    pub seat: Seat<Self>,

    // Single source of truth for the software cursor's logical position, kept
    // in sync by BOTH input paths (relative + absolute) in `input.rs`. The
    // cursor render element (src/cursor.rs) reads this each frame.
    pub pointer_location: Point<f64, Logical>,
    // The client-requested cursor image (named/surface/hidden), set by the
    // `SeatHandler::cursor_image` callback in handlers/mod.rs.
    pub cursor_status: CursorImageStatus,
    // The drag icon surface for an in-progress DnD session, set by
    // `WaylandDndGrabHandler::dnd_requested` (handlers/mod.rs) and cleared on
    // `dropped`/`cancelled` (handlers/xwayland.rs). `None` outside a drag --
    // the overwhelmingly common case, so the render path must stay
    // allocation-free when this is `None`.
    pub dnd_icon: Option<DndIcon>,
    // Surface-local position a pointer-locking client says it is drawing its own
    // cursor at. Recorded while the lock is active so the real pointer can be
    // warped there when the lock ends (handlers/pointer_constraints.rs).
    pub(crate) cursor_position_hint: Option<(WlSurface, Point<f64, Logical>)>,
    // Live focus-follows-mouse flag. Seeded from `config.focus_follows_mouse`
    // and re-seeded by `reload_config`, but flippable on its own via the
    // ToggleFocusFollowsMouse keybind -- same seed/live split as sdr_white_nits.
    pub(crate) focus_follows_mouse: bool,

    // wlr-screencopy captures awaiting the next presented frame. Pushed by the
    // frame `copy` handler (screencopy.rs), drained by each backend's render
    // path via `screencopy::fulfill_pending` right after it presents.
    pub(crate) pending_screencopy: Vec<crate::screencopy::PendingScreencopy>,

    // Names of outputs currently running the HDR pipeline, mirrored from
    // `SurfaceData::hdr`.
    //
    // Exists because `ColorManagementHandler::description_for_output` runs
    // during protocol dispatch and must not block on the udev RefCell -- it used
    // to `try_borrow` and report SDR on failure, which silently handed clients
    // an sRGB description for an HDR output. gamescope read that as
    // uMaxLum/uRefLum = 80 (sRGB's reference white), concluded there was no
    // headroom, and refused to expose HDR to the game. A cache cannot fail that
    // way: it is either current or it is a bug with a visible cause.
    pub(crate) hdr_outputs: HashSet<String>,

    /// What each connected display says its luminance range is, from its EDID's
    /// CTA-861 HDR Static Metadata Block (see `crate::edid`).
    ///
    /// Separate from `hdr_outputs` on purpose, and not a sync hazard: whether an
    /// output is *running* HDR is dynamic (`toggle_hdr` flips it), while what the
    /// panel is *capable* of is fixed for as long as it stays plugged in. Keyed
    /// by output name, populated on connect and dropped on disconnect. A missing
    /// entry means the display said nothing usable, and callers fall back to
    /// `HdrLuminance::FALLBACK`.
    pub(crate) hdr_luminance: HashMap<String, crate::edid::HdrLuminance>,

    // wp_presentation: lets clients learn when a frame actually hit the
    // display, which is what VK_KHR_present_wait / present_id are built on and
    // what video players use for frame pacing. Feedback is collected per frame
    // in `udev::render_surface` and fired from the DRM page-flip handler
    // (`udev::frame_finish`) -- advertising the global without that would hang
    // any client that waits on a `presented` event.
    //
    // Never read: holding it IS its purpose. `PresentationState` owns the
    // global's `GlobalId`, so dropping this field would tear `wp_presentation`
    // back down mid-session.
    #[allow(dead_code)]
    pub(crate) presentation_state: PresentationState,

    // HDR Phase 1b: wp_color_management_v1 state (advertised TFs/primaries,
    // known image-description identities). See `color_management::init`.
    pub(crate) color_management_state: ColorManagementState,

    // Loop handle stashed so `ColorManagementHandler::schedule_image_description_info`
    // can defer `wp_image_description_info_v1`'s events to an idle callback
    // (required -- see that impl's doc comment). Cloned from `event_loop.handle()`
    // at construction; calloop's `LoopHandle` is itself a cheap `Rc`-backed clone.
    pub(crate) loop_handle: LoopHandle<'static, RubixState>,

    // ---- Idle timer / idle-inhibit / ext-idle-notify (Phase 2) ----
    //
    // `last_activity` is the single clock all three consult: `notify_activity`
    // (called from `input.rs::process_input_event` for every keyboard/pointer
    // event, screen on or off) stamps it, `idle_notifier_state.notify_activity`
    // pings ext-idle-notify listeners off the same call, and `rearm_idle_timer`
    // computes the next timeout's delay from it -- one signal feeding every
    // consumer, not a second parallel clock.
    pub(crate) last_activity: std::time::Instant,
    // The pending idle-timeout calloop timer, if one is armed. `None` while
    // disarmed (screen already off, idle disabled/`screen_off_seconds == 0`,
    // or an inhibitor is held) -- see `rearm_idle_timer`.
    pub(crate) idle_timer: Option<smithay::reexports::calloop::RegistrationToken>,
    /// Coalesces a burst of filesystem events into one reload. See
    /// `schedule_config_reload`.
    pub(crate) config_reload_timer: Option<smithay::reexports::calloop::RegistrationToken>,
    /// Fires when the next slideshow image is due. `None` when the wallpaper is
    /// a single file, so a static desktop arms no timer at all.
    pub(crate) wallpaper_timer: Option<smithay::reexports::calloop::RegistrationToken>,
    // Surfaces currently holding a live `zwp_idle_inhibitor_v1`. A `HashSet`
    // rather than a count: a client destroying its *wl_surface* without ever
    // sending the inhibitor's `destroy` request (crash, or just sloppy
    // teardown order) must still release cleanly -- see
    // `CompositorHandler::destroyed` in handlers/compositor.rs, which purges
    // by surface identity.
    pub(crate) idle_inhibitors: HashSet<WlSurface>,
    // Mirrors `!idle_inhibitors.is_empty()`, kept as its own field (rather
    // than recomputed) so `sync_idle_inhibited` can short-circuit on no
    // change instead of always re-pushing to `idle_notifier_state`.
    pub(crate) idle_inhibited: bool,
    // Never read after construction, same reasoning as `presentation_state`
    // above: holding it IS its purpose (owns the `zwp_idle_inhibit_manager_v1`
    // global's `GlobalId` -- dropping this would tear the global down).
    #[allow(dead_code)]
    pub(crate) idle_inhibit_manager_state: smithay::wayland::idle_inhibit::IdleInhibitManagerState,
    pub(crate) idle_notifier_state: smithay::wayland::idle_notify::IdleNotifierState<RubixState>,

    // ---- Screen power / wlr-output-power-management-v1 (Phase 3) ----
    //
    // Authoritative power state moved to per-output (`udev::SurfaceData::
    // power_off`) in Phase 3 -- a single `screen_off: bool` here could not
    // represent "DP-3 off, HDMI-A-1 on". `any_output_off`/`all_outputs_off`
    // below derive their answer by reading every surface fresh each call, so
    // there is no second copy that could drift from the per-output truth.
    // Semantics (deliberately asymmetric, see `input.rs`/`rearm_idle_timer`):
    //   - wake-on-input turns ON every output that is off, regardless of why.
    //   - the idle timer disarms only once EVERY output is off.
    /// Bound `zwlr_output_power_v1` control objects, one per output at most
    /// (see output_power.rs's module doc for the one-object-per-output rule
    /// and its crash-safety guarantee).
    pub(crate) output_power: HashMap<String, ZwlrOutputPowerV1>,
}

impl RubixState {
    pub fn new(event_loop: &mut EventLoop<'static, Self>, display: Display<Self>, config: Config) -> Self {
        let start_time = std::time::Instant::now();

        let dh = display.handle();
        let loop_handle = event_loop.handle();

        let compositor_state = CompositorState::new::<Self>(&dh);
        let viewporter_state = ViewporterState::new::<Self>(&dh);
        // CLOCK_MONOTONIC: matches the clock DRM reports page-flip timestamps
        // against, so the times we hand clients need no conversion.
        let presentation_state =
            PresentationState::new::<RubixState>(&dh, <Monotonic as ClockSource>::ID as u32);
        let color_management_state = crate::color_management::init(&dh);
        let idle_inhibit_manager_state =
            smithay::wayland::idle_inhibit::IdleInhibitManagerState::new::<Self>(&dh);
        let idle_notifier_state =
            smithay::wayland::idle_notify::IdleNotifierState::<Self>::new(&dh, loop_handle.clone());
        crate::output_power::init(&dh);
        let pointer_constraints_state = PointerConstraintsState::new::<Self>(&dh);
        let relative_pointer_manager_state = RelativePointerManagerState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&dh);
        let kde_decoration_state =
            KdeDecorationState::new::<Self>(&dh, crate::handlers::RUBIX_KDE_DEFAULT_MODE);
        let layer_shell_state = WlrLayerShellState::new::<Self>(&dh);
        let xwayland_shell_state = XWaylandShellState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let mut seat_state = SeatState::new();
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        // wlr-data-control: lets headless clients (wl-paste, clipse, etc.) read/write
        // the clipboard without creating a real toplevel. No primary-selection support.
        let data_control_state = DataControlState::new::<Self, _>(&dh, None, |_| true);
        let popups = PopupManager::default();

        // A seat is a group of keyboards, pointer and touch devices.
        // A seat typically has a pointer and maintains a keyboard focus and a pointer focus.
        let mut seat: Seat<Self> = seat_state.new_wl_seat(&dh, "winit");

        // Notify clients that we have a keyboard, for the sake of the example we assume that keyboard is always present.
        // You may want to track keyboard hot-plug in real compositor.
        seat.add_keyboard(Default::default(), 200, 25).unwrap();

        // Notify clients that we have a pointer (mouse)
        // Here we assume that there is always pointer plugged in
        seat.add_pointer();

        // A space represents a two-dimensional plane. Windows and Outputs can be mapped onto it.
        //
        // Windows get a position and stacking order through mapping.
        // Outputs become views of a part of the Space and can be rendered via Space::render_output.
        let space = Space::default();

        let socket_name = Self::init_wayland_listener(display, event_loop);

        // Get the loop signal, used to stop the event loop
        let loop_signal = event_loop.get_signal();

        // No output is mapped into `space` yet at construction time (the
        // winit/udev backends map their output right after `RubixState::new`
        // returns), so this always takes the `(0.0, 0.0)` branch today. The
        // `space.outputs()` lookup is kept anyway so this stays correct if
        // that ordering ever changes.
        let pointer_location = space
            .outputs()
            .next()
            .and_then(|o| space.output_geometry(o))
            .map(|geo| {
                Point::<f64, Logical>::from((
                    geo.loc.x as f64 + geo.size.w as f64 / 2.0,
                    geo.loc.y as f64 + geo.size.h as f64 / 2.0,
                ))
            })
            .unwrap_or_else(|| (0.0, 0.0).into());

        // Monitors are created lazily, one per bound output, in
        // `bind_output_monitor` once each backend's output-connect path maps
        // an Output into `space` (see udev.rs/winit.rs). Empty at construction.
        let workspace = Workspace::new();

        let sdr_white_nits = config.sdr_white_nits.clamp(80.0, 300.0);
        let focus_follows_mouse = config.focus_follows_mouse;

        let mut state = Self {
            start_time,
            display_handle: dh,

            space,
            loop_signal,
            pending_vt: None,
            pending_chords: Vec::new(),
            socket_name,

            config,
            sdr_white_nits,

            workspace,
            windows: HashMap::new(),
            next_id: 1,
            unmapped: HashMap::new(),
            ipc_dirty: false,
            pending_config_errors: Vec::new(),
            wallpaper: crate::wallpaper::WallpaperManager::default(),
            theme_env: Vec::new(),
            theme: None,
            theme_border_colors: None,
            text: std::cell::RefCell::new(crate::text::TextRenderer::new()),
            wallpaper_timer: None,
            config_reload_timer: None,
            foreign_toplevel: Default::default(),
            reserved_bounds: None,
            animations: HashMap::new(),
            pending_transition: None,
            active_ghosts: Vec::new(),
            active_scales: Vec::new(),
            fullscreen_windows: HashSet::new(),
            iconified: HashSet::new(),
            pending_x11_focus: None,
            maximized: MaximizeState::None,
            last_configured: HashMap::new(),

            compositor_state,
            viewporter_state,
            xdg_shell_state,
            xdg_decoration_state,
            kde_decoration_state,
            layer_shell_state,
            xwayland_shell_state,
            xwm: None,
            xdisplay: None,
            xwayland_started_once: false,
            xwayland_respawn_attempts: 0,
            xwayland_last_exit: None,
            shm_state,
            output_manager_state,
            seat_state,
            data_device_state,
            data_control_state,
            popups,
            dmabuf_state: DmabufState::new(),
            dmabuf_global: None,
            udev_handle: None,
            pointer_constraints_state,
            relative_pointer_manager_state,
            seat,

            pointer_location,
            cursor_status: CursorImageStatus::default_named(),
            dnd_icon: None,
            cursor_position_hint: None,
            focus_follows_mouse,
            pending_screencopy: Vec::new(),

            hdr_outputs: HashSet::new(),
            hdr_luminance: HashMap::new(),
            presentation_state,
            color_management_state,
            loop_handle,

            last_activity: start_time,
            idle_timer: None,
            idle_inhibitors: HashSet::new(),
            idle_inhibited: false,
            idle_inhibit_manager_state,
            idle_notifier_state,
            output_power: HashMap::new(),
        };
        // Arm the idle timer from a cold start too -- a session nobody has
        // touched yet should still blank at `screen_off_seconds`, not only
        // after the first real activity re-arms it.
        state.rearm_idle_timer();
        state
    }

    /// Force a repaint so a queued screencopy capture is serviced. The udev
    /// backend renders on demand (VBlank/timer) and would otherwise stay idle
    /// until something else dirties the screen, leaving the client (e.g. grim)
    /// blocked forever; winit repaints continuously and needs no nudge.
    pub(crate) fn nudge_render(&self) {
        if let Some(udev) = &self.udev_handle {
            crate::udev::nudge_all_renders(udev);
        }
    }

    /// Live A/B toggle of HDR on every HDR-capable output; see
    /// `crate::udev::toggle_hdr`. Notifies bound `wp_color_management_output_v1`
    /// objects for the toggled outputs afterward, so HDR-aware clients
    /// (browsers) re-query `description_for_output` and flip HDR detection
    /// without a page reload. `udev::toggle_hdr` returns the toggled outputs
    /// rather than us re-borrowing `udev` here -- avoids a second borrow
    /// alongside `self.color_management_state`.
    pub(crate) fn toggle_hdr(&mut self) {
        // Clone the `Rc` (not a borrow of `self`) first so the subsequent
        // `&mut self.color_management_state` below doesn't conflict with a
        // live `&self.udev_handle` borrow.
        let Some(udev) = self.udev_handle.clone() else {
            return;
        };
        // The new per-output state comes back with the outputs so `hdr_outputs`
        // stays in step without a second udev borrow. That cache is what
        // `description_for_output` reads, so letting it drift would hand clients
        // the wrong description for the rest of the session.
        for (output, is_hdr) in crate::udev::toggle_hdr(&udev) {
            if is_hdr {
                self.hdr_outputs.insert(output.name());
            } else {
                self.hdr_outputs.remove(&output.name());
            }
            self.color_management_state.output_description_changed(&output);
        }
    }

    /// True if `name` is a known udev output that is currently powered off.
    /// `false` (not "unknown") for a name that doesn't match anything --
    /// callers that need to distinguish "off" from "no such output" (there
    /// are none today) would need a different signature. Always `false` on
    /// winit (no real CRTCs to power down).
    pub(crate) fn output_is_off(&self, name: &str) -> bool {
        self.output_power_status().into_iter().any(|(n, off)| n == name && off)
    }

    /// True if AT LEAST ONE known output is currently powered off. Drives
    /// wake-on-input (`input.rs`): per the Phase 3 policy, input wakes EVERY
    /// off output regardless of why it was off, so the wake check only needs
    /// to know "is there anything to wake at all".
    pub(crate) fn any_output_off(&self) -> bool {
        any_off(&self.output_power_status())
    }

    /// True only if EVERY known output is currently powered off. Drives the
    /// idle timer's disarm guard (`idle_timer_delay`'s `screen_off` param) --
    /// per the Phase 3 policy, the timer keeps counting down as long as any
    /// output is still lit, since its job (blank everything) isn't done yet.
    /// `false` -- not vacuously `true` -- when there are no known outputs at
    /// all (nothing has connected yet, or winit): there is nothing to call
    /// "blanked" in that case.
    pub(crate) fn all_outputs_off(&self) -> bool {
        all_off(&self.output_power_status())
    }

    /// Every known output's current power state, `(name, is_off)`. Backs the
    /// three predicates above and `ipc::Request::ScreenStatus`. On winit
    /// (`udev_handle` is `None`) this lists every mapped output as always on
    /// -- there are no real CRTCs to power down there, matching Phase 1/2's
    /// blanket no-op semantics for that backend.
    pub(crate) fn output_power_status(&self) -> Vec<(String, bool)> {
        let Some(udev) = &self.udev_handle else {
            return self.space.outputs().map(|o| (o.name(), false)).collect();
        };
        crate::udev::output_power_status(udev)
    }

    /// DPMS-equivalent screen power for one output (`output_name = Some(name)`)
    /// or every output (`output_name = None` -- the idle timer's and the CLI's
    /// "all" case), driven by `ipc::Request::SetScreenPower`, `input.rs`'s wake
    /// hook, and `output_power.rs`'s own `set_mode` handler. No-op on winit --
    /// `udev_handle` is `None` there, so there are no real CRTCs to power down.
    /// See `crate::udev::set_screen_power` for the actual DRM atomic-commit
    /// work; per-output truth lives entirely there (`SurfaceData::power_off`),
    /// never duplicated here.
    ///
    /// Returns the outputs that actually transitioned (matches
    /// `udev::set_screen_power`'s return) -- empty means every targeted
    /// output already matched `on`, in which case this deliberately skips
    /// re-arming the idle timer and emitting a `mode` event: a no-op request
    /// must produce no observable side effect.
    pub(crate) fn set_screen_power(&mut self, on: bool, output_name: Option<&str>) -> Vec<(Output, bool)> {
        let Some(udev) = self.udev_handle.clone() else {
            return Vec::new();
        };
        let changed = crate::udev::set_screen_power(&udev, on, output_name);
        if changed.is_empty() {
            return changed;
        }

        if on {
            // Any power-on -- input wake, an explicit IPC/CLI call, or a
            // client's own `set_mode(on)` -- restarts the idle countdown.
            // Without this, waking a stale-idle session could have the timer
            // fire again on the very next tick.
            self.last_activity = std::time::Instant::now();
        }
        // Re-arm unconditionally, not just on wake: turning an output off
        // might now mean every output is off (see `idle_timer_delay`'s
        // `all_outputs_off` guard), in which case this cancels the timer;
        // otherwise it's a cheap recompute against the (possibly still
        // ticking) elapsed time, same as the idle-inhibit-release path.
        self.rearm_idle_timer();

        // wlr-output-power: tell every bound client about every transition,
        // regardless of what caused it -- see output_power.rs's module doc
        // for why this has to be unconditional here rather than left to
        // individual call sites to remember.
        crate::output_power::notify_power_changed(self, &changed);
        changed
    }

    /// Single activity signal for the idle subsystem: called from
    /// `input.rs::process_input_event` for every keyboard/pointer event
    /// (screen on or off). Stamps `last_activity`, pings ext-idle-notify
    /// listeners, and re-arms the idle timeout -- the one clock phase-1's
    /// wake hook, the idle timer, and ext-idle-notify all read.
    pub(crate) fn notify_activity(&mut self) {
        self.last_activity = std::time::Instant::now();
        let seat = self.seat.clone();
        self.idle_notifier_state.notify_activity(&seat);
        self.rearm_idle_timer();
    }

    /// (Re)arm the idle-timeout calloop timer against the current config and
    /// `last_activity`, first dropping whatever was previously armed. Refuses
    /// to arm (leaves `idle_timer` as `None`) while the screen is already
    /// off, idle is disabled or timed out to `0`, or an inhibitor is held --
    /// `idle_timer_delay` holds that policy as a pure function so it's
    /// unit-testable without a real calloop loop (see state_tests.rs).
    ///
    /// Computing the delay from *elapsed* time since `last_activity`, rather
    /// than always arming the full timeout, is what makes config hot-reload
    /// correct: `reload_config` calls this after swapping in a new
    /// `screen_off_seconds`, and if the session has already been idle longer
    /// than the new (shorter) timeout, this arms a near-immediate fire
    /// instead of waiting out a full fresh timeout.
    pub(crate) fn rearm_idle_timer(&mut self) {
        if let Some(token) = self.idle_timer.take() {
            self.loop_handle.remove(token);
        }
        let Some(delay) = idle_timer_delay(
            self.all_outputs_off(),
            self.idle_inhibited,
            &self.config.idle,
            self.last_activity.elapsed(),
        ) else {
            return;
        };
        let loop_handle = self.loop_handle.clone();
        let timer = if delay.is_zero() {
            smithay::reexports::calloop::timer::Timer::immediate()
        } else {
            smithay::reexports::calloop::timer::Timer::from_duration(delay)
        };
        let token = loop_handle.insert_source(timer, move |_, _, data| {
            // Guarded again here (not just at arm time): an inhibitor taken
            // out, or the screen already turned off some other way, between
            // arming and firing must not blank it a second time / redundantly.
            if !data.all_outputs_off() && !data.idle_inhibited {
                tracing::info!(
                    "idle timeout ({}s) reached; powering every output off",
                    data.config.idle.screen_off_seconds,
                );
                data.set_screen_power(false, None);
            }
            data.idle_timer = None;
            smithay::reexports::calloop::timer::TimeoutAction::Drop
        });
        self.idle_timer = token.ok();
    }

    /// Reload the config a short while after filesystem activity stops.
    ///
    /// A single save is not a single event. Editors truncate before writing, so
    /// the first inotify event routinely arrives while the file is zero bytes
    /// -- which parses as "TOML parse error at line 1, column 1" and, now that
    /// config problems are surfaced on screen, produced a notification on every
    /// save. Several more events follow as the write completes, each of which
    /// used to trigger its own full reload (and, with a slideshow configured,
    /// its own synchronous image decode).
    ///
    /// Re-arming on each event means the reload happens once, after the writing
    /// has stopped.
    pub(crate) fn schedule_config_reload(&mut self) {
        if let Some(token) = self.config_reload_timer.take() {
            self.loop_handle.remove(token);
        }
        let timer = smithay::reexports::calloop::timer::Timer::from_duration(
            std::time::Duration::from_millis(250),
        );
        let token = self.loop_handle.insert_source(timer, move |_, _, data| {
            data.config_reload_timer = None;
            data.reload_config();
            smithay::reexports::calloop::timer::TimeoutAction::Drop
        });
        self.config_reload_timer = token.ok();
    }

    /// Arm (or disarm) the slideshow timer to match the wallpaper's schedule.
    ///
    /// Deliberately not driven off the render loop: a static desktop produces
    /// no frames, so a slideshow hung off rendering would never advance on an
    /// idle screen -- which is exactly when it is most visible.
    pub(crate) fn rearm_wallpaper_timer(&mut self) {
        if let Some(token) = self.wallpaper_timer.take() {
            self.loop_handle.remove(token);
        }
        let Some(next_at) = self.wallpaper.next_slideshow_at() else {
            return;
        };
        let delay = next_at.saturating_duration_since(Instant::now());
        let timer = if delay.is_zero() {
            smithay::reexports::calloop::timer::Timer::immediate()
        } else {
            smithay::reexports::calloop::timer::Timer::from_duration(delay)
        };
        let token = self.loop_handle.insert_source(timer, move |_, _, data| {
            if data.wallpaper.advance_slideshow(Instant::now()) {
                // Nothing else marks the output damaged: the wallpaper is not a
                // client surface, so no commit arrives to trigger a repaint.
                data.nudge_render();
            }
            data.wallpaper_timer = None;
            // Re-armed rather than repeating on a fixed interval: a swap that
            // had to wait on a slow decode reschedules itself sooner, and a
            // config reload can change the interval underneath us.
            data.rearm_wallpaper_timer();
            smithay::reexports::calloop::timer::TimeoutAction::Drop
        });
        self.wallpaper_timer = token.ok();
    }

    /// Recompute `idle_inhibited` from `idle_inhibitors` and, on change,
    /// propagate it to `idle_notifier_state` (so ext-idle-notify clients --
    /// swayidle, hypridle -- observe the same inhibit state Rubix itself
    /// acts on) and either cancel or restart the idle timer.
    pub(crate) fn sync_idle_inhibited(&mut self) {
        let inhibited = !self.idle_inhibitors.is_empty();
        if inhibited == self.idle_inhibited {
            return;
        }
        self.idle_inhibited = inhibited;
        self.idle_notifier_state.set_is_inhibited(inhibited);
        if inhibited {
            if let Some(token) = self.idle_timer.take() {
                self.loop_handle.remove(token);
            }
        } else {
            // Inhibitor released: count idle time from now, not from
            // whatever `last_activity` was before the inhibitor was taken --
            // otherwise releasing a long-held inhibitor could blank the
            // screen instantly.
            self.last_activity = std::time::Instant::now();
            self.rearm_idle_timer();
        }
    }

    fn init_wayland_listener(
        display: Display<RubixState>,
        event_loop: &mut EventLoop<RubixState>,
    ) -> OsString {
        // Creates a new listening socket, automatically choosing the next available `wayland` socket name.
        let listening_socket = ListeningSocketSource::new_auto().unwrap();

        // Get the name of the listening socket.
        // Clients will connect to this socket.
        let socket_name = listening_socket.socket_name().to_os_string();

        let loop_handle = event_loop.handle();

        loop_handle
            .insert_source(listening_socket, move |client_stream, _, state| {
                // Inside the callback, you should insert the client into the display.
                //
                // You may also associate some data with the client when inserting the client.
                state
                    .display_handle
                    .insert_client(client_stream, Arc::new(ClientState::default()))
                    .unwrap();
            })
            .expect("Failed to init the wayland event source.");

        // You also need to add the display itself to the event loop, so that client events will be processed by wayland-server.
        loop_handle
            .insert_source(
                Generic::new(display, Interest::READ, Mode::Level),
                |_, display, state| {
                    // Safety: we don't drop the display
                    unsafe {
                        display.get_mut().dispatch_clients(state).unwrap();
                    }
                    Ok(PostAction::Continue)
                },
            )
            .unwrap();

        socket_name
    }

    /// The output whose geometry contains this global point, if any. `None`
    /// when there are no outputs, or the point falls in a dead zone between
    /// heads (a gap left by non-adjacent placement in config).
    pub(crate) fn output_at(&self, point: Point<f64, Logical>) -> Option<Output> {
        self.space
            .outputs()
            .find(|o| {
                self.space
                    .output_geometry(o)
                    .map(|geo| geo.to_f64().contains(point))
                    .unwrap_or(false)
            })
            .cloned()
    }

    /// Resolve the surface (and its global position) under the pointer, honouring
    /// the layer-shell stacking order: overlay/top layer surfaces sit *above* the
    /// tiled windows, bottom/background *below* -- the same z-order the render
    /// paths build. Without the layer hit-test the space's toplevels were the only
    /// thing the pointer could ever land on, so layer clients (mako notifications,
    /// the bar) never received pointer enter/button events and couldn't be clicked.
    /// `(app_id, title)` for a window, whichever shell it came from.
    ///
    /// Wayland reads xdg-toplevel surface state; X11 maps `class` -> app_id and
    /// `title` -> title, which is the convention every taskbar expects. Empty
    /// X11 strings become `None` rather than `Some("")` so consumers can treat a
    /// missing identity uniformly. Shared by the IPC snapshot and the
    /// foreign-toplevel list so a window is named the same way everywhere.
    pub(crate) fn window_identity(&self, id: u32) -> (Option<String>, Option<String>) {
        let Some(window) = self.windows.get(&id) else {
            return (None, None);
        };
        match window.underlying_surface() {
            WindowSurface::Wayland(toplevel) => {
                smithay::wayland::compositor::with_states(toplevel.wl_surface(), |states| {
                    let attrs = states
                        .data_map
                        .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                        .map(|d| d.lock().unwrap());
                    match attrs {
                        Some(attrs) => (attrs.app_id.clone(), attrs.title.clone()),
                        None => (None, None),
                    }
                })
            }
            WindowSurface::X11(x11) => {
                let non_empty = |s: String| (!s.is_empty()).then_some(s);
                (non_empty(x11.class()), non_empty(x11.title()))
            }
        }
    }

    /// Global top-left of the mapped window backing `surface`, for turning
    /// surface-local client coordinates back into compositor space.
    pub(crate) fn window_location(&self, surface: &WlSurface) -> Option<Point<f64, Logical>> {
        let window = self
            .space
            .elements()
            .find(|window| window.wl_surface().is_some_and(|s| s.as_ref() == surface))?;
        self.space.element_location(window).map(|loc| loc.to_f64())
    }

    /// Hold a proposed pointer position inside the output layout.
    ///
    /// Any point landing on some output is accepted as-is, so the cursor crosses
    /// freely between adjacent heads; otherwise it is clamped per-axis to the
    /// output it is currently on, stopping it at that head's edge where there is
    /// no neighbour.
    pub(crate) fn clamp_to_outputs(&self, proposed: Point<f64, Logical>) -> Point<f64, Logical> {
        if self.output_at(proposed).is_some() {
            return proposed;
        }
        let current = self
            .output_at(self.pointer_location)
            .or_else(|| self.space.outputs().next().cloned());
        let Some(current) = current else { return self.pointer_location };
        let Some(geo) = self.space.output_geometry(&current) else { return self.pointer_location };

        let mut clamped = proposed;
        clamped.x = clamped.x.clamp(geo.loc.x as f64, (geo.loc.x + geo.size.w) as f64);
        clamped.y = clamped.y.clamp(geo.loc.y as f64, (geo.loc.y + geo.size.h) as f64);
        clamped
    }

    pub fn surface_under(&self, pos: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
        // Prefer the output whose geometry actually contains the pointer; fall
        // back to the first known output for a point in a dead zone between
        // heads (gaps from non-adjacent placement), so behaviour never
        // regresses versus the single-output case.
        let output = self.output_at(pos).unwrap_or(self.space.outputs().next()?.clone());
        let output = &output;
        let output_loc = self.space.output_geometry(output).map(|g| g.loc).unwrap_or_default();
        let layers = layer_map_for_output(output);
        // layer_geometry / layer_under work in output-local coords; shift the global
        // pointer position into that space, and shift results back out.
        let local = pos - output_loc.to_f64();

        let hit_layer = |layer: &LayerSurface| -> Option<(WlSurface, Point<f64, Logical>)> {
            let base = layers.layer_geometry(layer)?.loc.to_f64() + output_loc.to_f64();
            layer
                .surface_under(pos - base, WindowSurfaceType::ALL)
                .map(|(s, p)| (s, p.to_f64() + base))
        };

        // Above the tiled windows.
        if let Some(layer) = layers
            .layer_under(WlrLayer::Overlay, local)
            .or_else(|| layers.layer_under(WlrLayer::Top, local))
        {
            if let Some(hit) = hit_layer(layer) {
                return Some(hit);
            }
        }

        // The tiled windows themselves.
        if let Some((window, location)) = self.space.element_under(pos) {
            if let Some(hit) = window
                .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                .map(|(s, p)| (s, (p + location).to_f64()))
            {
                return Some(hit);
            }
        }

        // Below the tiled windows.
        if let Some(layer) = layers
            .layer_under(WlrLayer::Bottom, local)
            .or_else(|| layers.layer_under(WlrLayer::Background, local))
        {
            if let Some(hit) = hit_layer(layer) {
                return Some(hit);
            }
        }

        None
    }

    /// Bind a freshly-mapped Output to a model Monitor: idempotently create/get
    /// the Monitor whose id matches the output's `[[output]]` config entry (by
    /// name), stash that id in the Output's user-data so later lookups
    /// (`output_bounds_for`, remove-on-disconnect) can go the other way, and
    /// make it the active monitor if none is active yet (first head connected
    /// wins). Unconfigured outputs (e.g. winit's nested "winit" name, or a
    /// hotplugged head with no matching entry) get an id appended above the
    /// configured range rather than colliding with a configured id.
    pub(crate) fn bind_output_monitor(&mut self, output: &Output) {
        let name = output.name();
        let id = self
            .config
            .outputs
            .iter()
            .position(|o| o.name == name)
            .map(|i| i as u32)
            .unwrap_or_else(|| {
                let base = self.config.outputs.len() as u32;
                base + self
                    .workspace
                    .monitors
                    .iter()
                    .filter(|m| m.id >= base)
                    .count() as u32
            });
        self.workspace.ensure_monitor(id, self.config.visible_columns);
        output.user_data().insert_if_missing(|| MonitorId(id));
        // The configured primary output claims active focus even if another head
        // bound first (connectors can enumerate in any order); otherwise the first
        // output to bind seeds it.
        let is_primary = self
            .config
            .outputs
            .iter()
            .find(|o| o.name == name)
            .map(|o| o.primary)
            .unwrap_or(false);
        if is_primary || self.workspace.active_monitor().is_none() {
            self.set_active_monitor(id);
        }
    }

    /// Make `id` the active monitor and tell the bar about it.
    ///
    /// Wraps `Workspace::set_active_monitor` purely to pair it with `ipc_dirty`:
    /// which head is active is part of what the status bar renders, and
    /// `Workspace` cannot set the flag itself (it has no reach into
    /// `RubixState`). Every caller was setting it by hand, which worked but put
    /// the burden on each new call site to remember. Go through this one.
    pub(crate) fn set_active_monitor(&mut self, id: u32) {
        if self.workspace.active_monitor_id() == id {
            return;
        }
        self.workspace.set_active_monitor(id);
        self.ipc_dirty = true;
    }

    /// Solve inputs for the theme, or `None` when `[theme] enable` is false --
    /// the single gate that also stops `WallpaperManager` from extracting a
    /// palette at all (see `WallpaperManager::set_theme_params`).
    pub(crate) fn theme_params(&self) -> Option<crate::theme::ThemeParams> {
        self.config.theme.enable.then(|| self.config.theme.solve_params(&self.config.decoration))
    }

    /// The last solved theme, or `None` when theming is disabled or no
    /// wallpaper has been themed yet. Callers must have a fallback: a
    /// compositor that has not solved a theme still has to draw.
    pub(crate) fn theme(&self) -> Option<&crate::theme::Theme> {
        self.theme.as_ref()
    }

    /// Emit a freshly solved theme to every configured surface: the JSON file
    /// (atomically), IPC subscribers, and the environment newly spawned
    /// children inherit.
    ///
    /// Called once per theme change -- the caller drains
    /// `WallpaperManager::take_theme_update`, so this never runs on every
    /// frame or every wallpaper-lifecycle tick, only when the solved theme
    /// actually changed.
    pub(crate) fn apply_theme_update(
        &mut self,
        wallpaper_path: &std::path::Path,
        theme: &crate::theme::Theme,
        registry: Option<&crate::ipc::ClientRegistry>,
    ) {
        let sdr_white_nits = self.config.theme.sdr_white_nits;
        let json = theme_json(wallpaper_path, theme, sdr_white_nits);

        if let Err(e) = write_theme_file(&self.config.theme.output_path, &json) {
            tracing::warn!("theme: failed to write {}: {e}", self.config.theme.output_path.display());
        }

        self.theme = Some(theme.clone());

        // Only affects processes started from here on -- see the doc comment
        // on `theme_env` itself.
        self.theme_env = vec![
            ("RUBIX_THEME".to_string(), self.config.theme.output_path.display().to_string()),
            ("RUBIX_BACKGROUND".to_string(), crate::palette::preview_hex(theme.background.abs10k, sdr_white_nits)),
            ("RUBIX_FOREGROUND".to_string(), crate::palette::preview_hex(theme.foreground.abs10k, sdr_white_nits)),
            ("RUBIX_ACCENT".to_string(), crate::palette::preview_hex(theme.accent.abs10k, sdr_white_nits)),
        ];

        // The glow is picked to contrast with the wallpaper rather than to
        // recede into it, so it is the one theme colour that belongs on a
        // border. Cleared rather than left stale when the option is off, so
        // toggling it back to the configured colour takes effect on reload.
        self.theme_border_colors = self.config.theme.apply_to_borders.then(|| {
            // Alpha is the user's, not the theme's: each style's alpha is how
            // that border's strength is dialled in, and replacing it would
            // silently undo that tuning. So each role keeps the alpha of the
            // style it is replacing.
            let themed = |colour: &crate::theme::ThemeColor, alpha: f32| {
                let [r, g, b] =
                    crate::theme::abs10k_to_display_srgb(colour.abs10k, sdr_white_nits);
                smithay::backend::renderer::Color32F::new(r, g, b, alpha)
            };
            (
                themed(&theme.glow, self.config.decoration.active.color.a()),
                themed(&theme.border, self.config.decoration.inactive.color.a()),
            )
        });

        if let Some(registry) = registry {
            crate::ipc::broadcast_theme_changed(registry, &json);
        }

        // Detached, like `notify_config_problems` below: `on_change` is user
        // config and may be slow, hang, or simply be wrong, and none of that
        // may ever block the compositor thread.
        if let Some(command) = &self.config.theme.on_change {
            spawn_detached_shell(command);
        }
    }

    /// Surface config problems through the configured sink.
    ///
    /// `note_config_problem` has already logged every one of these; this is the
    /// half that reaches someone who is not tailing the journal. `Silent` is
    /// therefore not "no diagnostics" -- it restores exactly the old behavior,
    /// log line and all.
    pub fn report_config_diagnostics(&mut self, problems: Vec<String>) {
        if problems.is_empty() {
            return;
        }
        if self.config.diagnostics.wants_ipc() {
            // Drained by the run-loop callback in main.rs, which owns the client
            // registry. Deliberately does NOT set `ipc_dirty`: cube state did not
            // change, and a config typo should not force a snapshot push.
            self.pending_config_errors.extend_from_slice(&problems);
        }
        if self.config.diagnostics.wants_osd() {
            Self::notify_config_problems(&problems);
        }
    }

    /// Fire a desktop notification for config problems. Best-effort by design.
    ///
    /// `notify-send` may not be installed, and a notification daemon may not be
    /// running -- notably at startup, where the daemon is usually itself in the
    /// `startup` list and so races this. A failure here must never take the
    /// compositor down or block the event loop, hence fire-and-forget.
    fn notify_config_problems(problems: &[String]) {
        const MAX_LINES: usize = 6;
        let shown = problems.len().min(MAX_LINES);
        let mut body = problems[..shown]
            .iter()
            .map(|p| escape_markup(p))
            .collect::<Vec<_>>()
            .join("\n");
        if problems.len() > shown {
            body.push_str(&format!(
                "\n... and {} more (see the log)",
                problems.len() - shown
            ));
        }
        let summary = if problems.len() == 1 {
            "Rubix: config problem".to_string()
        } else {
            format!("Rubix: {} config problems", problems.len())
        };
        // Args are passed directly, never through `sh -c`, so a config value
        // echoed back into the message cannot turn into a shell command.
        let spawned = std::process::Command::new("notify-send")
            .arg("--app-name=rubix")
            .arg("--urgency=normal")
            .arg("--icon=dialog-warning")
            .arg(summary)
            .arg(body)
            .spawn();
        match spawned {
            Ok(mut child) => {
                // Nothing in the compositor handles SIGCHLD, so an unreaped
                // notify-send would sit as a zombie for the life of the session.
                // Rare per event, but it accumulates across an editing session.
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
            Err(e) => tracing::warn!("could not run notify-send for config problems: {e}"),
        }
    }

    /// Hot-reload keybinds from the user config file. Only the keybind set is
    /// swapped; `visible_columns` is structural (it seeds the monitor's fixed
    /// column slots at startup), so a live change is logged and ignored until a
    /// restart. On a parse failure `Config::reload` returns `None` and the
    /// current binds stay in place (keep-last-good). The swap is live on the
    /// next keypress -- `process_input_event` reads `config.keybinds` fresh each
    /// time, so no re-registration is needed.
    pub fn reload_config(&mut self) {
        let reloaded = Config::reload();
        // Drained here, not inside the `Some` arm: a config that fails to parse
        // outright is the single most important case to surface, and that is
        // exactly the case where `reload` returns None.
        let mut problems = crate::config::take_config_diagnostics();
        let Some(new) = reloaded else {
            self.report_config_diagnostics(problems);
            return;
        };
        if new.visible_columns != self.config.visible_columns {
            tracing::info!(
                "config visible_columns {} -> {} needs a restart to take effect; keeping {}",
                self.config.visible_columns,
                new.visible_columns,
                self.config.visible_columns,
            );
        }
        let count = new.keybinds.len();
        self.config.keybinds = new.keybinds;
        self.config.animation_duration = new.animation_duration;
        // Gaps are per-frame layout inputs, not structural like visible_columns --
        // safe to swap live. Swapping them is not enough to SEE them, though:
        // the geometry pass only runs when something asks for it, and a repaint
        // redraws the existing rectangles rather than recomputing them. So a
        // gap edit used to sit invisible in the config until an unrelated event
        // -- opening or moving a window -- happened to re-tile. Remembered here
        // and acted on below, once every field is swapped.
        let gaps_changed =
            new.outer_gap != self.config.outer_gap || new.inner_gap != self.config.inner_gap;
        self.config.outer_gap = new.outer_gap;
        self.config.inner_gap = new.inner_gap;
        self.config.outputs = new.outputs;
        self.config.sdr_white_nits = new.sdr_white_nits;
        // Re-seed the live runtime value too (already clamped by resolve()),
        // so a plain config-file edit takes effect immediately without
        // needing a keybind nudge -- matches the gaps' live-swap behavior.
        self.sdr_white_nits = self.config.sdr_white_nits;
        // Decoration is the safest field here to swap live: borders are drawn
        // outside the client rect, so even a width change moves no window and
        // needs no re-layout -- the next frame simply draws a different ring.
        // Covers colors, rules and per-rule luminance in one go.
        self.config.decoration = new.decoration;
        self.config.focus_follows_mouse = new.focus_follows_mouse;
        // Swapped before the report below, so an edit that changes the sink is
        // itself announced through the sink it just asked for.
        self.config.diagnostics = new.diagnostics;
        // Re-seeded like sdr_white_nits: a config edit wins over a runtime
        // toggle, so saving the file is always the way back to a known state.
        self.focus_follows_mouse = self.config.focus_follows_mouse;
        // Live like the rest: a changed `screen_off_seconds` (or `enabled`)
        // re-arms the idle timer immediately, computed against the same
        // `last_activity` -- so shortening the timeout below the current idle
        // duration fires it almost immediately rather than waiting a full
        // fresh interval, and disabling it (or setting `0`) cancels outright.
        self.config.idle = new.idle;
        self.rearm_idle_timer();
        // Wallpapers resolve from both `[wallpaper]` and each `[[output]]`, so
        // this runs after both are swapped in. Decode failures join the config
        // problems already collected and go out through the same sink.
        self.config.wallpaper = new.wallpaper;
        // Swapped before `resolve` below, so a `[theme]` edit (including
        // turning it on or off) takes effect on the SAME reload that touches
        // the wallpaper, rather than lagging one edit behind.
        self.config.theme = new.theme;
        self.wallpaper.set_theme_params(self.theme_params());
        problems.extend(
            self.wallpaper
                .resolve(&self.config.wallpaper, &self.config.outputs, &self.config.decoration),
        );
        // The interval, or the presence of a slideshow at all, may have changed.
        self.rearm_wallpaper_timer();
        tracing::info!("reloaded config: {count} keybinds active");
        // Last, so every field is already swapped and the report goes out through
        // the sink this edit asked for.
        self.report_config_diagnostics(problems);
        // Re-tile only when the geometry inputs actually moved. Unconditional
        // would resize every window on every config save -- including edits
        // that touch nothing geometric, like a colour -- and a resize round-trip
        // is visible churn in clients that redraw on configure.
        if gaps_changed {
            self.apply_layout();
        }
        // Force a repaint so an sdr_white_nits edit is visible immediately,
        // same reasoning as the keybind path in dispatch_nav below.
        self.nudge_render();
    }

    /// Mint the next synthetic window id. Monotonic, never reused within a run.
    pub fn next_window_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Reconcile the model onto the Space. Runs the pure geometry pass over the
    /// active group's tree, unmaps any owned window that fell out of the visible
    /// set (hiding scrolled-away or rotated-out groups), then for each computed
    /// rectangle pushes position (via `map_element`) and size (via an xdg
    /// configure) onto the live window. Idempotent -- call it after every model
    /// mutation; re-mapping a window at an unchanged rect is a no-op and
    /// `send_pending_configure` only emits when the pending size actually differs.
    /// The `Output` backing the active monitor, if it still exists.
    ///
    /// Used as the fallback when a surface asks for its preferred image
    /// description before it has been mapped and so has no output yet.
    pub(crate) fn active_monitor_output(&self) -> Option<Output> {
        let monitor_id = self.workspace.active_monitor_id();
        self.space
            .outputs()
            .find(|o| o.user_data().get::<MonitorId>().is_some_and(|m| m.0 == monitor_id))
            .cloned()
    }

    pub(crate) fn output_bounds_for(&self, monitor_id: u32) -> Option<Rect> {
        let Some(output) = self
            .space
            .outputs()
            .find(|o| o.user_data().get::<MonitorId>().is_some_and(|m| m.0 == monitor_id))
            .cloned()
        else {
            return None;
        };
        let Some(output_geo) = self.space.output_geometry(&output) else {
            return None;
        };
        // Reserve space for exclusive layer surfaces (e.g. waybar). The layer map
        // computes this during `arrange()` (run on every layer commit); its
        // non-exclusive zone is the output-local rect left over after subtracting
        // each anchored bar's exclusive_zone. Tiling into it keeps windows from
        // overlapping the bar. `zone.loc` carries the top/left inset, so offset
        // it by the output's global position.
        let zone = smithay::desktop::layer_map_for_output(&output).non_exclusive_zone();
        let mut bounds = Rect {
            x: (output_geo.loc.x + zone.loc.x).max(0) as u32,
            y: (output_geo.loc.y + zone.loc.y).max(0) as u32,
            width: zone.size.w.max(0) as u32,
            height: zone.size.h.max(0) as u32,
        };
        // The compositor-drawn bar (src/bar.rs) has no layer surface, so
        // `non_exclusive_zone()` above knows nothing about it -- its strip has
        // to be subtracted by hand, alongside the layer-shell zone rather than
        // instead of it. Saturating throughout: a bar taller than the
        // reserved area clamps to a zero-height bounds rather than wrapping
        // `u32` on subtraction.
        if self.config.bar.enabled {
            let bar_height = self.config.bar.height;
            match self.config.bar.position {
                crate::config::BarPosition::Top => {
                    let consumed = bar_height.min(bounds.height);
                    bounds.y = bounds.y.saturating_add(consumed);
                    bounds.height = bounds.height.saturating_sub(consumed);
                }
                crate::config::BarPosition::Bottom => {
                    bounds.height = bounds.height.saturating_sub(bar_height);
                }
            }
        }
        Some(bounds)
    }
    
    pub fn window_rect(&self, id: u32) -> Option<Rect> {
        let monitor = self.workspace.active_monitor()?;
        let bounds = self.output_bounds_for(monitor.id)?;
        monitor
            .compute_layout(bounds, self.config.outer_gap, self.config.inner_gap)
            .into_iter()
            .find(|(wid, _)| *wid == id)
            .map(|(_, rect)| rect)
    }

    fn ease(t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        1.0 - (1.0 - t).powi(3)
    }

    // Position-only interpolation, signed -- no clamp, no size (tweens carry
    // only a top-left position; size is configured once, up front).
    fn lerp_scale(track: ScaleTrack, t: f32) -> f32 {
        track.from + (track.to - track.from) * t
    }

    fn lerp_pos(from: Pos, to: Pos, t: f32) -> Pos {
        let lerp = |a: i32, b: i32| (a as f32 + (b - a) as f32 * t).round() as i32;
        Pos { x: lerp(from.x, to.x), y: lerp(from.y, to.y) }
    }

    /// Plan the tween set for a transition. PURE -- caller supplies current
    /// on-screen positions (read from Space) and the target layout. `bounds`
    /// gives the off-screen slide distance (height for Scroll, width for
    /// Rotate). Endpoints are signed `Pos` -- an off-screen coordinate above
    /// or left of the origin is a plain negative number, not clamped.
    fn plan_transition(
        current: &HashMap<u32, Pos>,
        targets: &[(u32, Rect)],
        transition: Transition,
        bounds: Rect,
        now: Instant,
    ) -> HashMap<u32, Tween> {
        let targets_map: HashMap<u32, Pos> = targets
            .iter()
            .map(|(id, r)| (*id, Pos { x: r.x as i32, y: r.y as i32 }))
            .collect();
        let mut tweens: HashMap<u32, Tween> = HashMap::new();

        // Enter: in targets only
        for &(id, rect) in targets {
            if !current.contains_key(&id) {
                let target = Pos { x: rect.x as i32, y: rect.y as i32 };
                let from = Self::enter_from(transition, target, bounds);
                let scale = matches!(transition, Transition::Reveal)
                    .then_some(ScaleTrack { from: 0.0, to: 1.0 });
                tweens.insert(id, Tween { kind: TweenKind::Enter, from, to: target, start: now, ghost: None, scale });
            }
        }

        // Leave: in current only
        for (&id, &cur) in current {
            if !targets_map.contains_key(&id) {
                let to = Self::leave_to(transition, cur, bounds);
                let scale = matches!(transition, Transition::Reveal)
                    .then_some(ScaleTrack { from: 1.0, to: 0.0 });
                tweens.insert(id, Tween { kind: TweenKind::Leave, from: cur, to, start: now, ghost: None, scale });
            }
        }

        // Move: in both. Rotate wraps a window whose straight-across delta is
        // longer than the shorter cross-edge path; Scroll never wraps.
        for (&id, &cur) in current {
            if let Some(&target) = targets_map.get(&id) {
                let tween = match transition {
                    Transition::Rotate => Self::plan_rotate_move(cur, target, bounds, now),
                    // Reveal only swaps two groups; every other visible column
                    // holds still, so its windows get a degenerate Move rather
                    // than a scale.
                    Transition::Scroll { .. } | Transition::Reveal => {
                        Tween { kind: TweenKind::Move, from: cur, to: target, start: now, ghost: None, scale: None }
                    }
                };
                tweens.insert(id, tween);
            }
        }

        tweens
    }

    /// Plan a single Rotate Move, detecting a band-wrap. `orig_from`/`orig_to`
    /// are the window's current and target `Pos`. STRICT `>` on the threshold:
    /// an exact `width/2` delta (e.g. a 2-column full swap) is NOT a wrap, and
    /// both windows cross through the middle -- intentional for now (see spec
    /// known-limitations; a possible future follow-up).
    fn plan_rotate_move(orig_from: Pos, orig_to: Pos, bounds: Rect, now: Instant) -> Tween {
        let long_delta = orig_to.x - orig_from.x;
        if long_delta.abs() > (bounds.width as i32) / 2 {
            let wrap_delta = if long_delta > 0 {
                long_delta - bounds.width as i32
            } else {
                long_delta + bounds.width as i32
            };
            // Space-mapped copy enters from the far edge and lands at the real
            // destination -- LOAD-BEARING: this copy must end at `orig_to`
            // because the next transition reads `current` from
            // `space.element_location`, and focus/input hit-testing use the
            // Space location.
            let from = Pos { x: orig_to.x - wrap_delta, y: orig_to.y };
            let to = orig_to;
            // Ghost copy exits off the near edge, starting where the window is now.
            let ghost = Some(GhostTrack {
                from: orig_from,
                to: Pos { x: orig_from.x + wrap_delta, y: orig_from.y },
            });
            Tween { kind: TweenKind::Move, from, to, start: now, ghost, scale: None }
        } else {
            Tween { kind: TweenKind::Move, from: orig_from, to: orig_to, start: now, ghost: None, scale: None }
        }
    }

    /// Off-screen starting position for an Enter tween, by transition kind.
    fn enter_from(transition: Transition, target: Pos, bounds: Rect) -> Pos {
        match transition {
            // down == true (content moves up): enter from BELOW.
            // down == false (content moves down): enter from ABOVE.
            Transition::Scroll { down } => {
                let dy = bounds.height as i32;
                Pos { x: target.x, y: if down { target.y + dy } else { target.y - dy } }
            }
            // Nearest-edge: a target on the left half came from off-screen
            // LEFT; right half came from off-screen RIGHT.
            Transition::Rotate => {
                let dx = bounds.width as i32;
                let midpoint = bounds.x as i32 + dx / 2;
                if target.x < midpoint {
                    Pos { x: target.x - dx, y: target.y }
                } else {
                    Pos { x: target.x + dx, y: target.y }
                }
            }
            // Grows in place: start and end are the same point.
            Transition::Reveal => target,
        }
    }

    /// Off-screen ending position for a Leave tween, by transition kind.
    fn leave_to(transition: Transition, cur: Pos, bounds: Rect) -> Pos {
        match transition {
            // down == true (content moves up): leave to TOP.
            // down == false (content moves down): leave to BOTTOM.
            Transition::Scroll { down } => {
                let dy = bounds.height as i32;
                Pos { x: cur.x, y: if down { cur.y - dy } else { cur.y + dy } }
            }
            // Nearest-edge: a window currently on the left half exits LEFT;
            // right half exits RIGHT.
            Transition::Rotate => {
                let dx = bounds.width as i32;
                let midpoint = bounds.x as i32 + dx / 2;
                if cur.x < midpoint {
                    Pos { x: cur.x - dx, y: cur.y }
                } else {
                    Pos { x: cur.x + dx, y: cur.y }
                }
            }
            // Shrinks in place: start and end are the same point.
            Transition::Reveal => cur,
        }
    }

    /// Settle in-flight tweens so Space is a clean baseline. Leave any windows
    /// still owned in `self.windows` mapped at their final rect; unmapping only
    /// for Leave tweens (windows that fell out of the visible set).
    fn settle_tweens(&mut self) {
        let done: Vec<u32> = self.animations.keys().copied().collect();
        for id in done {
            if let Some(tween) = self.animations.remove(&id) {
                if let Some(window) = self.windows.get(&id) {
                    match tween.kind {
                        TweenKind::Leave => {
                            self.space.unmap_elem(window);
                        }
                        _ => {
                            self.space.map_element(window.clone(), (tween.to.x, tween.to.y), false);
                        }
                    }
                }
            }
        }
    }

    /// Advance every active tween one frame. Returns true while any tween is live.
    /// Touches nothing when `self.animations` is empty (otherwise the udev backend
    /// never idles).
    pub fn step_animations(&mut self) -> bool {
        // Cleared FIRST, before the empty-animations guard below: otherwise the
        // last wrap's ghost would leak forever, since the guard returns early
        // on every subsequent idle frame and the list never gets rebuilt.
        self.active_ghosts.clear();
        self.active_scales.clear();
        if self.animations.is_empty() { return false; }
        let duration_secs = self.config.animation_duration.as_secs_f32();
        let now = Instant::now();
        let mut done: Vec<u32> = Vec::new();
        for (id, tween) in self.animations.iter() {
            let t = (now - tween.start).as_secs_f32() / duration_secs;
            let e = Self::ease(t);
            let pos = Self::lerp_pos(tween.from, tween.to, e);
            match tween.scale {
                // Scaling window: kept OUT of the Space for the duration, or
                // render_elements_for_region would draw a second copy at full
                // size underneath the scaled one. Re-mapped on completion below
                // (Enter) or left unmapped for good (Leave).
                Some(track) => {
                    if let Some(window) = self.windows.get(id) {
                        self.space.unmap_elem(window);
                    }
                    self.active_scales.push((*id, pos, Self::lerp_scale(track, e)));
                }
                None => {
                    if let Some(window) = self.windows.get(id) {
                        self.space.map_element(window.clone(), (pos.x, pos.y), false);
                    }
                }
            }
            if let Some(g) = tween.ghost {
                let gpos = Self::lerp_pos(g.from, g.to, e);
                self.active_ghosts.push((*id, gpos));
            }
            if t >= 1.0 { done.push(*id); }
        }
        for id in done {
            if let Some(tween) = self.animations.remove(&id) {
                if let Some(window) = self.windows.get(&id) {
                    match tween.kind {
                        TweenKind::Leave => { self.space.unmap_elem(window); }
                        _ => { self.space.map_element(window.clone(), (tween.to.x, tween.to.y), false); }
                    }
                }
            }
        }
        !self.animations.is_empty()
    }

    /// Insert `id` into the active monitor's grid by splitting the focused
    /// window -- the same rule new windows follow in `xdg_shell::new_toplevel`
    /// and `xwayland::map_window_request`.
    fn insert_into_grid(&mut self, id: u32) {
        let focused_id = self.focused_window_id();
        let direction = focused_id
            .and_then(|fid| self.window_rect(fid))
            .map(Rect::longer_axis)
            .unwrap_or(SplitDirection::Horizontal);
        if let Some(monitor) = self.workspace.active_monitor_mut() {
            monitor.add_window(direction, id, focused_id.unwrap_or(0));
        }
    }

    /// Enter or leave exclusive fullscreen for `id`.
    ///
    /// A fullscreen window LEAVES the tiling grid rather than keeping a slot
    /// that `compute_layout` keeps filling. Holding a slot meant the window only
    /// stopped being drawn once its entire column scrolled off screen, which
    /// never happens when every column already fits -- navigating away from a
    /// game did nothing. Out of the grid the remaining windows reflow into the
    /// whole layout, and `apply_layout` becomes the only thing deciding whether
    /// the fullscreen window is on screen.
    ///
    /// Re-insertion splits the focused window, so a window does not necessarily
    /// return to the slot it left; restoring exactly needs an insert-at-slot API
    /// the model doesn't have yet.
    /// Ask a window to close, the polite way for each shell.
    ///
    /// This is a request, not a teardown: the client decides (it may put up a
    /// "save changes?" dialog and never close). Rubix's own bookkeeping is
    /// driven by the resulting unmap, not from here.
    pub(crate) fn close_window(&mut self, id: u32) {
        let Some(window) = self.windows.get(&id) else { return };
        match window.underlying_surface() {
            WindowSurface::Wayland(toplevel) => toplevel.send_close(),
            WindowSurface::X11(x11) => {
                let _ = x11.close();
            }
        }
    }

    pub fn set_window_fullscreen(&mut self, id: u32, fullscreen: bool) {
        if fullscreen {
            if !self.fullscreen_windows.insert(id) {
                return;
            }
            // A destroyed window may have been on any monitor, and
            // `remove_window` is id-based and a no-op when absent.
            for monitor in &mut self.workspace.monitors {
                let _ = monitor.remove_window(id);
            }
        } else {
            if !self.fullscreen_windows.remove(&id) {
                return;
            }
            self.insert_into_grid(id);
        }
    }

    /// Step the focused window through the maximize cycle.
    ///
    /// Forward is `Group` -> `Monitor` -> `None`; reverse walks the same ring
    /// the other way, so it enters straight at `Monitor` and steps back down to
    /// `Group`. Both directions leave the cycle at `None`. See
    /// [`MaximizeState`].
    ///
    /// The reverse direction exists because the two stages serve different
    /// intents: filling the group is the incremental "give this window its
    /// column" move, while filling the monitor is the one you want *now*.
    /// Binding both directions means neither intent has to press through the
    /// other to get what it wanted.
    ///
    /// Deliberately not tree surgery. An earlier sketch had the first press
    /// *promote* the window to the front of its group's split tree, but that
    /// mutates the layout permanently -- un-maximizing would leave the window
    /// sitting in its new position with nothing to undo it. Covering the group
    /// is a pure layout override, so releasing it is just clearing this field.
    pub fn cycle_maximize(&mut self, forward: bool) {
        let Some(id) = self.focused_window_id() else {
            return;
        };
        // Fullscreen already owns the whole output; maximizing under it would be
        // a no-op the user can't see, and un-maximizing later would look random.
        if self.fullscreen_windows.contains(&id) {
            return;
        }

        self.maximized = next_maximize(
            self.maximized,
            id,
            forward,
            self.group_window_ids(id).len() > 1,
        );
        self.apply_layout();
        self.ipc_dirty = true;
    }

    /// Every window sharing the group that holds `id`, including `id` itself.
    ///
    /// Empty when the window is not in the grid at all -- fullscreen windows are
    /// detached from it, so this is also the "not tiled" answer.
    fn group_window_ids(&self, id: u32) -> Vec<u32> {
        for monitor in &self.workspace.monitors {
            let Some((col, row)) = monitor.locate(id) else {
                continue;
            };
            return monitor
                .columns()
                .get(col)
                .and_then(|c| c.groups().get(row))
                .map(|g| g.window_ids())
                .unwrap_or_default();
        }
        Vec::new()
    }

    /// Focus a fullscreen window, cycling if there is more than one.
    ///
    /// Fullscreen windows are outside the grid, so `focus_active_window` -- which
    /// walks active_column -> active_row -> first leaf -- can never land on one.
    /// Without this there is no way back to a game once you navigate off it. The
    /// real answer is a focus-by-id path the launcher can drive; this is the
    /// keyboard escape hatch until that exists.
    pub fn focus_next_fullscreen(&mut self) {
        let mut ids: Vec<u32> = self
            .fullscreen_windows
            .iter()
            .copied()
            .filter(|id| self.windows.contains_key(id))
            .collect();
        if ids.is_empty() {
            return;
        }
        // Sorted so "next" is stable across calls -- HashSet iteration order is
        // not, and cycling that would revisit windows at random.
        ids.sort_unstable();

        let current = self.focused_window_id();
        let next = current
            .and_then(|cur| ids.iter().position(|id| *id == cur))
            .map(|pos| ids[(pos + 1) % ids.len()])
            .unwrap_or(ids[0]);
        self.focus_by_id(next);
    }

    /// Give keyboard focus to the window now under the pointer, if
    /// focus-follows-mouse is on and nothing vetoes it.
    ///
    /// Driven from pointer *motion* only, never from a window arriving under a
    /// stationary cursor -- matching sway, and avoiding focus lurching around
    /// on its own during a rotate animation.
    pub(crate) fn focus_follows_pointer(&mut self, pos: Point<f64, Logical>) {
        if !self.focus_follows_mouse {
            return;
        }
        // A grab owns the pointer for the length of a drag or resize; moving
        // focus mid-drag pulls the grab out from under itself.
        if self.seat.get_pointer().is_some_and(|p| p.is_grabbed()) {
            return;
        }
        // Suspended entirely while anything is fullscreen. `reconcile_focus_state`
        // drops a non-focused window's fullscreen, so a stray hover would kick a
        // game out of fullscreen -- and a fullscreen window is outside the grid,
        // making that hard to undo.
        if !self.fullscreen_windows.is_empty() {
            return;
        }
        // `element_under` only sees space elements, i.e. real toplevels, so
        // layer-shell surfaces are structurally excluded: hovering the bar or
        // rofi must never take the keyboard away from them.
        let Some(id) = self.window_id_at(pos) else { return };
        if self.focused_window_id() == Some(id) {
            return;
        }
        // Focus without raising: hover is not a gesture at the window, so it
        // must not reorder the stack. See focus_by_id_without_raising.
        self.focus_by_id_without_raising(id);
        self.ipc_dirty = true;
    }

    /// The window under a point, as a Rubix id. Resolved through the Space so
    /// subsurfaces and popups map to their owning toplevel.
    pub(crate) fn window_id_at(&self, pos: Point<f64, Logical>) -> Option<u32> {
        let (window, _) = self.space.element_under(pos)?;
        self.windows
            .iter()
            .find(|(_, candidate)| *candidate == window)
            .map(|(id, _)| *id)
    }

    /// Deactivate the pointer constraint on every window except `keep`.
    ///
    /// The protocol releases a constraint when its surface loses *pointer* focus,
    /// but a locked pointer cannot move, so pointer focus never changes on its
    /// own: navigate away from a game that holds a lock and the cursor stays
    /// frozen on it forever. Keyboard focus moving off the window is the signal
    /// we actually get, so the release is driven from there.
    pub(crate) fn release_pointer_constraints(&mut self, keep: Option<u32>) {
        let Some(pointer) = self.seat.get_pointer() else { return };
        let surfaces: Vec<WlSurface> = self
            .windows
            .iter()
            .filter(|(id, _)| Some(**id) != keep)
            .filter_map(|(_, window)| window.wl_surface().map(|s| s.into_owned()))
            .collect();

        for surface in surfaces {
            with_pointer_constraint(&surface, &pointer, |constraint| {
                if let Some(constraint) = constraint {
                    if constraint.is_active() {
                        constraint.deactivate();
                    }
                }
            });
        }
    }

    /// Reconcile focus-dependent window state after keyboard focus moves.
    ///
    /// Maximize releases on any focus change, mirroring the `unfocus` handler in
    /// Max's AwesomeWM config. Fullscreen is split by client type: a Wayland
    /// toplevel accepts the compositor's word and is genuinely un-fullscreened
    /// back into the grid, while an X11 client keeps its fullscreen state and is
    /// merely hidden -- telling one it is windowed just invites it to set
    /// `_NET_WM_STATE_FULLSCREEN` straight back.
    pub(crate) fn reconcile_focus_state(&mut self) {
        let focused = self.focused_window_id();
        let mut dirty = false;

        self.release_pointer_constraints(focused);

        if self.maximized.window().is_some() && self.maximized.window() != focused {
            self.maximized = MaximizeState::None;
            dirty = true;
        }

        let unfullscreen: Vec<u32> = self
            .fullscreen_windows
            .iter()
            .copied()
            .filter(|id| Some(*id) != focused)
            .filter(|id| {
                self.windows.get(id).is_some_and(|w| {
                    matches!(w.underlying_surface(), WindowSurface::Wayland(_))
                })
            })
            .collect();

        for id in unfullscreen {
            if let Some(WindowSurface::Wayland(toplevel)) =
                self.windows.get(&id).map(|w| w.underlying_surface())
            {
                toplevel.with_pending_state(|state| {
                    state.states.unset(xdg_toplevel::State::Fullscreen);
                });
            }
            self.set_window_fullscreen(id, false);
            tracing::info!("window {id} left fullscreen (lost focus)");
            dirty = true;
        }

        if dirty {
            self.apply_layout();
            self.ipc_dirty = true;
        }
    }

    /// Tell X11 clients whether the grid is currently showing them.
    ///
    /// An X11 client will not accept being un-fullscreened: drop
    /// `_NET_WM_STATE_FULLSCREEN` and a game sets it straight back (Against the
    /// Storm re-asserted ~3s later, taking the output with it). `WM_STATE =
    /// IconicState` is a state it will respect, because the correct response to
    /// being minimized is to stop rather than to argue. Hiding it is done by
    /// unmapping the frame ourselves rather than by asking the client to iconify
    /// itself: a client that reads IconicState and unmaps its own window in
    /// response is treated as withdrawn, and a withdrawn window cannot be
    /// recovered.
    ///
    /// Driven by visibility rather than by the nav paths, since the grid hides
    /// windows several ways -- scrolling past `visible_columns`, a non-active
    /// row, a new window displacing one -- and all of them owe the client the
    /// same notification.
    fn sync_x11_iconic(&mut self, visible: &HashSet<u32>) {
        // `self.iconified` tracks what each client was last told, so a layout
        // pass that changes nothing doesn't spray redundant property writes.
        let mut changed: Vec<(u32, bool)> = Vec::new();
        for (id, window) in &self.windows {
            if let WindowSurface::X11(x11) = window.underlying_surface() {
                // Override-redirect windows own their own geometry and never
                // appear in `targets`, so a visibility diff would read as
                // "always hidden" and try to iconify a menu or tooltip. They
                // also have no frame to unmap -- `set_mapped` rejects them
                // outright.
                if x11.is_override_redirect() {
                    continue;
                }
                let hidden = !visible.contains(id);
                if self.iconified.contains(id) != hidden {
                    if hidden {
                        // Unmap the FRAME, then set the property. The frame is
                        // what leaves the screen, and the client's own window is
                        // never unmapped, so XWayland delivers no UnmapNotify
                        // for it. That is the whole point: smithay reads a
                        // client unmap as a withdraw and responds by destroying
                        // the frame and detaching the wl_surface, which strands
                        // the window with nothing left to map back. Setting the
                        // property alone invites exactly that -- a game reads
                        // IconicState, unmaps itself, and is gone for good.
                        //
                        // `set_mapped(false)` writes WithdrawnState, so
                        // `set_hidden` runs second to land WM_STATE on
                        // IconicState. It still writes, because `mapped_onto` is
                        // only cleared by a real UnmapNotify, which this path no
                        // longer provokes.
                        let _ = x11.set_mapped(false);
                        let _ = x11.set_hidden(true);
                    } else {
                        // Clear the property first: `set_mapped` chooses
                        // NormalState vs IconicState by reading
                        // _NET_WM_STATE_HIDDEN back out, so a stale HIDDEN here
                        // would restore the frame still flagged iconic.
                        let _ = x11.set_hidden(false);
                        let _ = x11.set_mapped(true);
                    }
                    changed.push((*id, hidden));
                }
            }
        }

        for (id, hidden) in changed {
            if hidden {
                tracing::info!("window {id} iconified (hidden by layout)");
                self.iconified.insert(id);
            } else {
                self.iconified.remove(&id);
            }
        }
    }

    // Evict a window from every place its id can linger: the tiling model, the
    // registry, the render space, and the fullscreen/iconified sets. Shared by
    // both destroy paths (xdg_shell::toplevel_destroyed, xwayland's
    // remove_x11_window) and the eviction Change 3/Change 4 need, so there is
    // one place that knows the full teardown instead of four that might drift.
    // Deliberately does NOT call `apply_layout`/set `ipc_dirty` -- callers that
    // evict a batch (the Xwayland-crash sweep) need exactly one re-tile for the
    // whole batch, not one per window.
    pub(crate) fn remove_window_by_id(&mut self, id: u32) {
        // A window may be on any monitor, not just the active one --
        // remove_window is id-based and a no-op when absent, so sweeping all
        // monitors is safe.
        for monitor in &mut self.workspace.monitors {
            monitor.remove_window(id);
        }
        if let Some(win) = self.windows.remove(&id) {
            self.space.unmap_elem(&win);
        }
        self.fullscreen_windows.remove(&id);
        self.iconified.remove(&id);
    }

    pub fn apply_layout(&mut self) {
        let mut targets: Vec<(u32, Rect)> = Vec::new();
        for monitor in &self.workspace.monitors {
            if let Some(bounds) = self.output_bounds_for(monitor.id) {
                targets.extend(monitor.compute_layout(bounds, self.config.outer_gap, self.config.inner_gap));
            }
        }

        // Maximize: keep the grid slot, take a bigger rect. Only applies while
        // the window is actually on screen -- a maximized window scrolled out of
        // view has no entry to override and stays out of view. Both stages work
        // by overwriting that one entry, which is why neither disturbs the grid.
        let maximize_rect = match self.maximized {
            MaximizeState::None => None,
            MaximizeState::Group(id) => group_bounds(&targets, &self.group_window_ids(id)),
            MaximizeState::Monitor(_) => self
                .workspace
                .active_monitor()
                .and_then(|m| self.output_bounds_for(m.id))
                .map(|bounds| {
                    let gap = self.config.outer_gap;
                    Rect {
                        x: bounds.x + gap,
                        y: bounds.y + gap,
                        width: bounds.width.saturating_sub(2 * gap),
                        height: bounds.height.saturating_sub(2 * gap),
                    }
                }),
        };
        if let (Some(id), Some(rect)) = (self.maximized.window(), maximize_rect) {
            if let Some(entry) = targets.iter_mut().find(|(tid, _)| *tid == id) {
                entry.1 = rect;
            }
        }

        // Fullscreen windows are OUT of the grid (`set_window_fullscreen`), so
        // `compute_layout` never emits them and their rect comes from here --
        // but only for the focused one. That is what makes navigating away
        // actually leave a fullscreen game, rather than depending on its column
        // happening to scroll off screen (which never happens when every column
        // already fits). Anything left out is reported iconic below.
        let focused = self.focused_window_id();
        let fullscreen_ids: Vec<u32> = self.fullscreen_windows.iter().cloned().collect();
        for id in fullscreen_ids {
            if Some(id) != focused {
                continue;
            }
            if let Some(window) = self.windows.get(&id) {
                // Find which monitor this window is on by checking its current location
                let output = self
                    .space
                    .element_location(window)
                    .and_then(|loc| self.output_at(loc.to_f64()))
                    .or_else(|| self.space.outputs().next().cloned());

                if let Some(output) = output {
                    if let Some(bounds) = self.space.output_geometry(&output) {
                        let rect = Rect {
                            x: bounds.loc.x as u32,
                            y: bounds.loc.y as u32,
                            width: bounds.size.w as u32,
                            height: bounds.size.h as u32,
                        };
                        // Normally there is no entry to override, since the window
                        // left the grid on going fullscreen. The override still
                        // covers the transient case where a client requests
                        // fullscreen in the same pass its tiled entry was built.
                        if let Some(entry) = targets.iter_mut().find(|(tid, _)| *tid == id) {
                            entry.1 = rect;
                        } else {
                            targets.push((id, rect));
                        }
                    }
                }
            }
        }

        // Whatever `targets` holds now is exactly what the grid intends to show,
        // on both the snap and the animate path, so this is the one place that
        // knows which X11 clients just became (in)visible.
        let visible: HashSet<u32> = targets.iter().map(|(id, _)| *id).collect();
        self.sync_x11_iconic(&visible);

        match self.pending_transition.take() {
            None => {
                // SNAP PATH — byte-for-byte today's behavior.
                // Settle in-flight animations first (safety): for each tween,
                // if Leave -> unmap; else map_element at tween.to. Clear the map.
                self.settle_tweens();

                let stale: Vec<Window> = self
                    .windows
                    .iter()
                    .filter(|(id, _)| !visible.contains(id))
                    .map(|(_, window)| window.clone())
                    .collect();
                for window in stale {
                    self.space.unmap_elem(&window);
                }

                for (id, rect) in targets {
                    let Some(window) = self.windows.get(&id).cloned() else {
                        continue;
                    };

                    let is_fullscreen = self.fullscreen_windows.contains(&id);

                    // Check if geometry or fullscreen state has actually changed since last configure.
                    // This gates redundant configure resends during reflows of unrelated windows,
                    // preventing spurious swapchain recreations that cause flicker during exclusive
                    // fullscreen scanout.
                    let state_changed = self.last_configured.get(&id)
                        .map(|(last_rect, last_fullscreen)| {
                            last_rect != &rect || *last_fullscreen != is_fullscreen
                        })
                        .unwrap_or(true);

                    if state_changed {
                        match window.underlying_surface() {
                            WindowSurface::Wayland(toplevel) => {
                                toplevel.with_pending_state(|state| {
                                    state.size = Some((rect.width as i32, rect.height as i32).into());
                                    if is_fullscreen {
                                        state.states.set(xdg_toplevel::State::Fullscreen);
                                        state.states.unset(xdg_toplevel::State::TiledLeft);
                                        state.states.unset(xdg_toplevel::State::TiledRight);
                                        state.states.unset(xdg_toplevel::State::TiledTop);
                                        state.states.unset(xdg_toplevel::State::TiledBottom);
                                    } else {
                                        state.states.set(xdg_toplevel::State::TiledLeft);
                                        state.states.set(xdg_toplevel::State::TiledRight);
                                        state.states.set(xdg_toplevel::State::TiledTop);
                                        state.states.set(xdg_toplevel::State::TiledBottom);
                                        state.states.unset(xdg_toplevel::State::Fullscreen);
                                    }
                                });
                                toplevel.send_pending_configure();
                                self.last_configured.insert(id, (rect.clone(), is_fullscreen));
                            }
                            WindowSurface::X11(x11) => {
                                // X11 configure carries position AND size in one rect.
                                // map_element below still sets the compositor-side
                                // location; keep both.
                                let _ = x11.configure(Some(Rectangle::new(
                                    (rect.x as i32, rect.y as i32).into(),
                                    (rect.width as i32, rect.height as i32).into(),
                                )));
                                self.last_configured.insert(id, (rect.clone(), is_fullscreen));
                            }
                        }
                    }

                    self.space.map_element(window, (rect.x as i32, rect.y as i32), false);
                }
            }
            Some(transition) => {
                // ANIMATE PATH
                // Nav dispatch (the only source of a pending_transition) always
                // mutates the ACTIVE monitor, so the active monitor's bounds are
                // the correct off-screen reference frame for this tween -- not an
                // approximation. If the active monitor has no bound output (or
                // there's no active monitor), settle in-flight tweens and bail,
                // matching the prior no-output early-return.
                let Some(bounds) = self
                    .workspace
                    .active_monitor()
                    .and_then(|m| self.output_bounds_for(m.id))
                else {
                    self.settle_tweens();
                    return;
                };

                // 1. Settle in-flight tweens first so Space is a clean baseline.
                self.settle_tweens();

                // 2. Build current positions from windows still mapped in Space.
                let mut current: HashMap<u32, Pos> = HashMap::new();
                for (id, window) in &self.windows {
                    if let Some(loc) = self.space.element_location(window) {
                        current.insert(*id, Pos { x: loc.x, y: loc.y });
                    }
                }

                // 3. For every target id: send the size configure.
                for (id, rect) in &targets {
                    let Some(window) = self.windows.get(id).cloned() else {
                        continue;
                    };

                    let is_fullscreen = self.fullscreen_windows.contains(id);

                    match window.underlying_surface() {
                        WindowSurface::Wayland(toplevel) => {
                            toplevel.with_pending_state(|state| {
                                state.size = Some((rect.width as i32, rect.height as i32).into());
                                if is_fullscreen {
                                    state.states.set(xdg_toplevel::State::Fullscreen);
                                    state.states.unset(xdg_toplevel::State::TiledLeft);
                                    state.states.unset(xdg_toplevel::State::TiledRight);
                                    state.states.unset(xdg_toplevel::State::TiledTop);
                                    state.states.unset(xdg_toplevel::State::TiledBottom);
                                } else {
                                    state.states.set(xdg_toplevel::State::TiledLeft);
                                    state.states.set(xdg_toplevel::State::TiledRight);
                                    state.states.set(xdg_toplevel::State::TiledTop);
                                    state.states.set(xdg_toplevel::State::TiledBottom);
                                    state.states.unset(xdg_toplevel::State::Fullscreen);
                                }
                            });
                            toplevel.send_pending_configure();
                        }
                        WindowSurface::X11(x11) => {
                            // Same rect-carries-position-and-size as the snap
                            // path; map_element for the tween is driven by the
                            // plan below (unchanged) -- X11 windows animate
                            // identically since space placement is
                            // surface-agnostic.
                            let _ = x11.configure(Some(Rectangle::new(
                                (rect.x as i32, rect.y as i32).into(),
                                (rect.width as i32, rect.height as i32).into(),
                            )));
                        }
                    }
                }

                // 4-6. Plan tweens, map enter/move at from-position, store.
                let plan = Self::plan_transition(&current, &targets, transition, bounds, Instant::now());
                for (id, tween) in &plan {
                    if let Some(window) = self.windows.get(id).cloned() {
                        // Scaling tweens are drawn outside the Space for their
                        // whole run, so they must not be mapped here either --
                        // otherwise the window paints once at full size in the
                        // frame between planning and the first step_animations.
                        if tween.scale.is_some() {
                            self.space.unmap_elem(&window);
                            continue;
                        }
                        match tween.kind {
                            TweenKind::Enter | TweenKind::Move => {
                                self.space.map_element(window, (tween.from.x, tween.from.y), false);
                            }
                            TweenKind::Leave => {
                                // Leave it mapped where it is; step_animations unmaps at completion.
                            }
                        }
                    }
                }
                self.animations = plan;
            }
        }

        // A maximized window overlaps its neighbours' tiles, so stack order is
        // what decides whether it reads as maximized or as half-buried. Raising
        // it within `Space` puts it above every other toplevel and no higher:
        // layer-shell Top/Overlay (the bar, notifications) are composited from
        // separate lists that always sit in front of space elements, so this
        // cannot paint over them. Raised before fullscreen so that if both
        // somehow apply at once, fullscreen still wins the top slot.
        if let Some(window) = self.maximized.window().and_then(|id| self.windows.get(&id).cloned()) {
            self.space.raise_element(&window, false);
        }

        // A fullscreen window must be topmost in the Space stack or a tiled
        // window rendered above it kills primary-plane promotion. Raise after
        // both the SNAP and ANIMATE paths have finished mapping.
        for id in self.fullscreen_windows.iter().copied().collect::<Vec<_>>() {
            if let Some(window) = self.windows.get(&id).cloned() {
                self.space.raise_element(&window, false);
            }
        }
    }
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

use smithay::utils::{ClockSource, Monotonic};

/// Pure policy backing `RubixState::any_output_off`: true if any listed
/// output is off. Split out (mirroring `idle_timer_delay` below) so the
/// any/all distinction between wake-on-input and the idle timer's disarm
/// guard is unit-testable without a real udev backend -- see state_tests.rs.
fn any_off(statuses: &[(String, bool)]) -> bool {
    statuses.iter().any(|(_, off)| *off)
}

/// Pure policy backing `RubixState::all_outputs_off`. Deliberately `false`,
/// not vacuously `true`, on an empty list -- see that method's doc comment.
fn all_off(statuses: &[(String, bool)]) -> bool {
    !statuses.is_empty() && statuses.iter().all(|(_, off)| *off)
}

/// Pure policy for `RubixState::rearm_idle_timer`: given the current guard
/// conditions and how long it's been since the last activity, how much
/// longer (if any) the idle timer should wait before firing. `None` means
/// stay disarmed. Split out purely so this decision is unit-testable without
/// a real calloop event loop -- see state_tests.rs.
fn idle_timer_delay(
    screen_off: bool,
    idle_inhibited: bool,
    idle: &crate::config::IdleConfig,
    elapsed: std::time::Duration,
) -> Option<std::time::Duration> {
    if screen_off || idle_inhibited || !idle.enabled || idle.screen_off_seconds == 0 {
        return None;
    }
    let timeout = std::time::Duration::from_secs(idle.screen_off_seconds);
    Some(timeout.saturating_sub(elapsed))
}

/// Bounding box of every on-screen tile belonging to `ids`.
///
/// This is how the `Group` maximize stage gets its rect, and it reads the
/// already-computed `targets` rather than recomputing the column band. The two
/// are identical by construction: a group's split tree fills its band exactly,
/// because `inner_gap` only carves seams *between* leaves and never insets the
/// outer edge -- so the union of the leaves is the band. Deriving it this way
/// means the stage cannot drift from the gap arithmetic in `compute_layout`,
/// which is fiddly enough that a second copy would eventually disagree.
///
/// `None` when the group has no tiles in `targets`, which is exactly the case
/// where it is scrolled off screen and must not be blown up.
pub(crate) fn group_bounds(targets: &[(u32, Rect)], ids: &[u32]) -> Option<Rect> {
    targets
        .iter()
        .filter(|(id, _)| ids.contains(id))
        .fold(None, |acc: Option<Rect>, (_, rect)| {
            Some(match acc {
                None => *rect,
                Some(union) => {
                    let x = union.x.min(rect.x);
                    let y = union.y.min(rect.y);
                    let right = (union.x + union.width).max(rect.x + rect.width);
                    let bottom = (union.y + union.height).max(rect.y + rect.height);
                    Rect { x, y, width: right - x, height: bottom - y }
                }
            })
        })
}

/// The next stage in the maximize ring for `id`.
///
/// Split out from [`RubixState::cycle_maximize`] so the ring itself is testable
/// without a live compositor -- it is the part with the interesting edges.
///
/// `has_siblings` gates the `Group` stage in both directions: a window alone in
/// its group already fills it, so landing there would look like a dead press.
/// Forward that means entering at `Monitor`; reverse it means stepping from
/// `Monitor` past `Group` to `None`.
///
/// A press whose `current` names a *different* window takes the cycle over from
/// it rather than continuing its position, which is why those cases fall through
/// to the same entry arms as starting from rest.
pub(crate) fn next_maximize(
    current: MaximizeState,
    id: u32,
    forward: bool,
    has_siblings: bool,
) -> MaximizeState {
    let group_or = |fallback| {
        if has_siblings {
            MaximizeState::Group(id)
        } else {
            fallback
        }
    };
    match (current, forward) {
        (MaximizeState::Group(c), true) if c == id => MaximizeState::Monitor(id),
        (MaximizeState::Monitor(c), true) if c == id => MaximizeState::None,
        (MaximizeState::Group(c), false) if c == id => MaximizeState::None,
        (MaximizeState::Monitor(c), false) if c == id => group_or(MaximizeState::None),
        // Entering the ring: from rest, or taking it over from another window.
        (_, true) => group_or(MaximizeState::Monitor(id)),
        (_, false) => MaximizeState::Monitor(id),
    }
}

/// Escape the three characters Pango treats as markup.
///
/// Most notification daemons parse the body as Pango markup. Config diagnostics
/// quote the offending value back at the user verbatim, so an unbalanced `<` in a
/// config string would otherwise swallow the rest of the message -- the failure
/// mode being that the notification explaining a typo is itself mangled by it.
fn escape_markup(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Every colour role as hex + `abs10k` + achieved `Lc`, matching exactly what
/// `rubix theme` prints on stdout (see `handle_theme_subcommand` in main.rs)
/// so a file watcher and a CLI invocation agree on one shape.
fn theme_json(wallpaper_path: &std::path::Path, theme: &crate::theme::Theme, sdr_white_nits: f32) -> serde_json::Value {
    let colour = |c: &crate::theme::ThemeColor| {
        serde_json::json!({
            "hex": crate::palette::preview_hex(c.abs10k, sdr_white_nits),
            "abs10k": c.abs10k,
            "lc": c.lc,
        })
    };
    let ansi_names = ["red", "green", "yellow", "blue", "magenta", "cyan"];
    let ansi: serde_json::Map<String, serde_json::Value> =
        ansi_names.iter().zip(theme.ansi.iter()).map(|(name, c)| (name.to_string(), colour(c))).collect();
    serde_json::json!({
        "wallpaper": wallpaper_path.display().to_string(),
        "background": colour(&theme.background),
        "surface": colour(&theme.surface),
        "foreground": colour(&theme.foreground),
        "muted": colour(&theme.muted),
        "accent": colour(&theme.accent),
        "border": colour(&theme.border),
        "glow": colour(&theme.glow),
        "ansi": ansi,
        "effective_background": theme.effective_background,
        "met_targets": theme.met_targets,
    })
}

/// Write `json` to `path` atomically: a temp file in the SAME directory (so
/// the rename stays on one filesystem and is therefore atomic), then
/// `rename` over the final name. A consumer polling or watching `path` never
/// observes a half-written file.
fn write_theme_file(path: &std::path::Path, json: &serde_json::Value) -> std::io::Result<()> {
    let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "theme output_path has no parent directory",
        ));
    };
    std::fs::create_dir_all(dir)?;
    // PID-suffixed so two rubix processes (or a stale one that never exited)
    // sharing an output_path cannot race each other's temp file.
    let tmp = dir.join(format!(".theme.json.{}.tmp", std::process::id()));
    let write_result = std::fs::write(&tmp, serde_json::to_vec_pretty(json).unwrap_or_default());
    if write_result.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return write_result;
    }
    std::fs::rename(&tmp, path)
}

/// Run a shell command detached, same posture as `notify_config_problems`:
/// never blocks the compositor thread, and the child is reaped on a
/// throwaway thread rather than left as a zombie for the life of the
/// session. `on_change` is user config and may be slow or simply wrong --
/// none of that may ever reach the render loop.
fn spawn_detached_shell(command: &str) {
    match std::process::Command::new("sh").arg("-c").arg(command).spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => tracing::warn!("theme: on_change command failed to spawn: {e}"),
    }
}
