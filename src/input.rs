use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
        KeyState, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
    },
    desktop::WindowSurface,
    input::{
        keyboard::{
            keysyms::{
                KEY_Hyper_R, KEY_ISO_Level3_Shift, KEY_ISO_Level5_Shift, KEY_Shift_L,
                KEY_XF86Switch_VT_1, KEY_XF86Switch_VT_12,
            },
            xkb::{self, Keysym},
            FilterResult, ModifiersState,
        },
        pointer::{AxisFrame, ButtonEvent, MotionEvent, RelativeMotionEvent},
    },
    utils::SERIAL_COUNTER,
    wayland::{
        compositor::RegionAttributes,
        pointer_constraints::{PointerConstraint, with_pointer_constraint},
    },
};

use serde::{Deserialize, Serialize};

use crate::focus::KeyboardFocusTarget;
use crate::state::{MaximizeState, RubixState, Transition};
use crate::model::grid::{Direction, RevealKind};

/// A Rubix navigation chord. Bound to a chord in `config.toml`, where the value
/// is this variant's exact name -- serde deserializes it straight into the enum,
/// so there is no chord-name translation table in code. The direction is baked
/// into the variant name; the motion sign is derived once at the dispatch site.
/// `Serialize` is the wire format `Reply::Chord` and the listen-mode editor
/// consume (see ipc.rs's `broadcast_chord`) -- unit variants serialize as
/// their bare name and `Spawn` as `{"Spawn":"cmd"}`, the same vocabulary
/// `input.toml` already uses on the way in. No `rename_all`/`rename`: the
/// exact Rust identifier IS the config syntax, so any attribute here would
/// silently fork the two.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum NavAction {
    ScrollColumnDown,   // scroll rows within the active column
    ScrollColumnUp,     //  "
    RotateColumnsRight, // rotate active groups across columns
    RotateColumnsLeft,  //  "
    MoveToNewColumn,    // Promote the focused window into a new column
    MoveActiveColumnRight, // move the active_column pointer through the list of visible columns
    MoveActiveColumnLeft,  // without actually mutating the list.
    NewGroup,           // insert a fresh empty group after the active one and make it active
    Spawn(String),                 // Spawn a new command.
    Quit,               // stop the compositor (the only in-session exit on the TTY backend)
    IncrementVisibleColumns,
    DecrementVisibleColumns,
    FlipSplitDirection,
    MoveFocusedWindowUp,
    MoveFocusedWindowDown,
    MoveFocusedWindowLeft,
    MoveFocusedWindowRight,
    ToggleMaximize,        // cycle the focused window group -> monitor -> none; releases on focus change
    ToggleMaximizeReverse, // the same cycle walked backwards: straight to monitor, then group, then none
    FocusFullscreen,   // return to a fullscreen window (they sit outside the grid)
    RemoveWindow,      // close (politely) then unconditionally detach the focused window
    ToggleFocusFollowsMouse, // flip hover-to-focus live; config re-seeds it on save
    ToggleMouseFollowsFocus, // flip warp-on-focus live; config re-seeds it on save
    IncreaseSdrWhite,
    DecreaseSdrWhite,
    ToggleHdr,
}

/// Per-press adjustment for the IncreaseSdrWhite/DecreaseSdrWhite chords, in
/// nits. Tunable; the resolved value is always clamped to [80, 300] (mirrors
/// hdr_shaders::SDR_WHITE_NITS's doc comment and Config::resolve's clamp).
const SDR_WHITE_STEP: f32 = 10.0;

/// The outcome of the keyboard filter: either a config-bound navigation action,
/// or a VT switch. VT switching is a backend/session concern (only the udev
/// backend owns a session), so the filter can't act on it directly -- it stashes
/// the target VT in [`RubixState::pending_vt`] for the backend's input source to
/// consume. Resolving the chord here reuses the seat's live xkb state (the
/// keymap turns Ctrl+Alt+Fn into an `XF86Switch_VT_n` keysym) instead of
/// re-deriving modifiers in the backend.
enum KeyAction {
    Nav(NavAction),
    SwitchVt(i32),
}

/// Render a chord the same way `config::parse_chord` reads one, so a
/// listen-mode editor can feed what it sees straight back into
/// `input.toml`/`triggers.toml`: modifier tokens in a fixed order (Super,
/// Control, Alt, Shift), joined with `+`, ending in the xkb keysym name for
/// the resolved (modified) symbol -- e.g. Shift+1 comes out `Super+Control+
/// Shift+exclam`, not `Super+Control+Shift+1`, because `handle.modified_sym`
/// already applied Shift before this runs.
/// True for the keysyms that are themselves modifiers.
///
/// keyd sends a Tartarus chord as three separate presses -- Super down, Control
/// down, then the key -- and the first two arrive with the modifier state
/// already set. They pass the echo gate in `process_input_event` and would be
/// reported as `Super+Super_L` and `Super+Control+Control_L`. A modifier press
/// is not a chord: it can match no bind, so echoing it makes a listen-mode UI
/// announce two misses before every real chord.
fn is_modifier_keysym(sym: u32) -> bool {
    // Shift_L (0xffe1) through Hyper_R (0xffee) is one contiguous block
    // covering shift/control/caps/meta/alt/super/hyper. The ISO level shifts
    // sit outside it.
    (KEY_Shift_L..=KEY_Hyper_R).contains(&sym)
        || sym == KEY_ISO_Level3_Shift
        || sym == KEY_ISO_Level5_Shift
}

fn format_chord(mods: &ModifiersState, keysym: Keysym) -> String {
    let mut chord = String::new();
    if mods.logo {
        chord.push_str("Super+");
    }
    if mods.ctrl {
        chord.push_str("Control+");
    }
    if mods.alt {
        chord.push_str("Alt+");
    }
    if mods.shift {
        chord.push_str("Shift+");
    }
    chord.push_str(&xkb::keysym_get_name(keysym));
    chord
}

impl RubixState {
    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        // Single activity signal, computed once, feeding two independent
        // consumers below: phase-1's screen wake, and phase-2's idle timer /
        // ext-idle-notify (`notify_activity` bumps `last_activity`, pings
        // ext-idle-notify listeners, and re-arms the idle timeout -- see
        // state.rs). `InputEvent::DeviceAdded`/`DeviceRemoved` never reach
        // here -- the udev libinput source filters those out before calling
        // `process_input_event` (see src/udev.rs) specifically so a hotplug
        // counts as neither a wake nor idle-timer activity.
        let is_activity = matches!(
            &event,
            InputEvent::Keyboard { .. }
                | InputEvent::PointerMotion { .. }
                | InputEvent::PointerMotionAbsolute { .. }
                | InputEvent::PointerButton { .. }
                | InputEvent::PointerAxis { .. }
        );

        if is_activity {
            self.notify_activity();
        }

        // Keyboard/pointer-button *presses* that cause the wake are consumed
        // (returned early, never forwarded) so waking never types a character
        // into whatever had focus or clicks whatever was under the cursor.
        // Motion and axis events, and button/key releases, fall through to the
        // normal dispatch below unchanged.
        //
        // Phase 3 policy: wake turns ON EVERY output that is off, regardless
        // of which one (or why) -- `set_screen_power(true, None)` is "all
        // outputs", not just whichever one the cursor happens to be over. A
        // user must never end up unable to recover a dark panel from the
        // keyboard just because it was a client's `set_mode(Off)` that put
        // it there, not the idle timer.
        if self.any_output_off() && is_activity {
            self.set_screen_power(true, None);

            let consume_press = match &event {
                InputEvent::Keyboard { event, .. } => event.state() == KeyState::Pressed,
                InputEvent::PointerButton { event, .. } => event.state() == ButtonState::Pressed,
                _ => false,
            };
            if consume_press {
                return;
            }
        }

        match event {
            InputEvent::Keyboard { event, .. } => {
                let serial = SERIAL_COUNTER.next_serial();
                let time = Event::time_msec(&event);
                let key_state = event.state();

                // The filter intercepts our Super chords on BOTH press and release
                // (so the client never sees an orphaned edge) and returns the resolved
                // NavAction. Everything else forwards to the focused client unchanged.
                let action = self.seat.get_keyboard().expect("keyboard added to seat at startup").input::<KeyAction, _>(
                    self,
                    event.key_code(),
                    key_state,
                    serial,
                    time,
                    |state, mods, handle| {
                        let sym = handle.modified_sym().raw();
                        // Ctrl+Alt+Fn resolves (via xkb) to an XF86Switch_VT_n keysym.
                        // These are contiguous, so the VT number is the offset + 1.
                        if (KEY_XF86Switch_VT_1..=KEY_XF86Switch_VT_12).contains(&sym) {
                            let vt = (sym - KEY_XF86Switch_VT_1 + 1) as i32;
                            return FilterResult::Intercept(KeyAction::SwitchVt(vt));
                        }
                        let matched_kb = state.config.keybinds.iter().find(|kb| kb.matches(mods, sym));

                        // SECURITY: chord-echo is IPC-visible to any process that can
                        // open the socket (see `Request::SubscribeChords`), so it must
                        // never carry ordinary typing. Gate on the *modifier state*,
                        // not on whether a bind matched -- an unbound Super+X is still
                        // a deliberate chord and safe to report; a bare letter or
                        // Shift+letter is prose and must never reach this branch. Only
                        // the press edge is echoed (release is swallowed by the
                        // `key_state == Pressed` check below the filter), and the
                        // string is only built once the gate passes, so ordinary typing
                        // never allocates here.
                        if key_state == KeyState::Pressed
                            && (mods.logo || mods.ctrl || mods.alt)
                            && !is_modifier_keysym(sym)
                        {
                            let chord = format_chord(mods, handle.modified_sym());
                            state.pending_chords.push((chord, matched_kb.map(|kb| kb.action.clone())));
                        }

                        match matched_kb {
                            Some(kb) => FilterResult::Intercept(KeyAction::Nav(kb.action.clone())),
                            None => FilterResult::Forward,
                        }
                    },
                );

                // Act on the press edge only; the release was swallowed above purely
                // to keep the client's key state consistent.
                if key_state == KeyState::Pressed {
                    match action {
                        Some(KeyAction::Nav(action)) => self.dispatch_nav(action),
                        // Backend-neutral hand-off: the udev input source picks this
                        // up after the event and calls `session.change_vt`. winit has
                        // no session, so it simply never reads the field.
                        Some(KeyAction::SwitchVt(vt)) => self.pending_vt = Some(vt),
                        None => {}
                    }
                }
            }
            // Relative motion (udev/libinput mice): mirrors the absolute arm
            // below, but the new location is accumulated from a delta instead
            // of read off the device directly. Multi-monitor: the proposed
            // position is accepted as-is when it lands inside some output's
            // geometry (crossing freely between adjacent heads); otherwise it
            // is clamped per-axis to the geometry of whichever output the
            // pointer is currently in, so the cursor stops at that head's
            // edge where there's no neighbour.
            //
            // Pointer constraints (lock/confine): when a client locks the pointer,
            // motion is still received but the pointer stays visually at the lock
            // position while the client gets raw relative deltas via relative_pointer.
            // When confined, motion is clamped to the confine region.
            InputEvent::PointerMotion { event, .. } => {
                let delta = event.delta();
                let pointer = self.seat.get_pointer().expect("pointer added to seat at startup");
                let serial = SERIAL_COUNTER.next_serial();
                let origin = self.pointer_location;

                // Constraints belong to the surface the pointer is *over*, not to
                // whatever holds keyboard focus, so they are evaluated against
                // `surface_under` -- which also hands back the surface's top-left
                // in global coords, needed to test the constraint region.
                let under = self.surface_under(origin);

                // Only an *active* constraint applies. Honouring an inactive one
                // pins the cursor while the client -- never having been sent
                // `locked`/`confined` -- still believes it is free, and nothing
                // ever releases it. That was the frozen-cursor bug.
                let mut locked = false;
                let mut confined = false;
                let mut confine_region: Option<RegionAttributes> = None;
                if let Some((surface, surface_loc)) = under.as_ref() {
                    with_pointer_constraint(surface, &pointer, |constraint| {
                        let Some(constraint) = constraint else { return };
                        if !constraint.is_active() {
                            return;
                        }
                        // A constraint carrying a region only binds while the
                        // pointer is actually inside that region.
                        let point = (origin - *surface_loc).to_i32_round();
                        if !constraint.region().is_none_or(|r| r.contains(point)) {
                            return;
                        }
                        match &*constraint {
                            PointerConstraint::Locked(_) => locked = true,
                            PointerConstraint::Confined(confine) => {
                                confined = true;
                                confine_region = confine.region().cloned();
                            }
                        }
                    });
                }

                // Relative motion goes out first and unconditionally: a locked
                // client (an FPS turning its camera) has nothing else to drive
                // from. Unaccelerated deltas are passed through as the protocol
                // intends rather than duplicating the accelerated ones.
                pointer.relative_motion(
                    self,
                    under.clone(),
                    &RelativeMotionEvent {
                        delta,
                        delta_unaccel: event.delta_unaccel(),
                        utime: event.time(),
                    },
                );

                // Locked: the cursor does not move at all, and no absolute motion
                // is sent for the duration of the lock.
                if locked {
                    pointer.frame(self);
                    return;
                }

                let loc = self.clamp_to_outputs(origin + delta);
                let new_under = self.surface_under(loc);

                // Confined: reject outright any motion that would leave the
                // constraining surface or its region. The previous code clamped
                // to the whole output here, which confined nothing.
                if confined {
                    if let Some((surface, surface_loc)) = under.as_ref() {
                        if new_under.as_ref().map(|(s, _)| s) != Some(surface) {
                            pointer.frame(self);
                            return;
                        }
                        if let Some(region) = confine_region {
                            if !region.contains((loc - *surface_loc).to_i32_round()) {
                                pointer.frame(self);
                                return;
                            }
                        }
                    }
                }

                self.pointer_location = loc;

                pointer.motion(
                    self,
                    new_under.clone(),
                    &MotionEvent {
                        location: loc,
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);

                self.focus_follows_pointer(loc);

                // Moving into a constraint's region arms it: a client may create
                // the constraint while the pointer sits outside the region, and
                // `new_constraint` declines to activate in that case.
                if let Some((surface, surface_loc)) = new_under {
                    with_pointer_constraint(&surface, &pointer, |constraint| {
                        let Some(constraint) = constraint else { return };
                        if constraint.is_active() {
                            return;
                        }
                        let point = (loc - surface_loc).to_i32_round();
                        if constraint.region().is_none_or(|r| r.contains(point)) {
                            constraint.activate();
                        }
                    });
                }
            }
            InputEvent::PointerMotionAbsolute { event, .. } => {
                // Absolute devices (touchscreens/tablets) report position
                // relative to a single mapped output; multi-monitor absolute
                // input is rare on this setup, so this stays minimal: prefer
                // the output under the current pointer location, falling
                // back to the first known output.
                let output = self
                    .output_at(self.pointer_location)
                    .or_else(|| self.space.outputs().next().cloned());
                let Some(output) = output else { return; };

                let Some(output_geo) = self.space.output_geometry(&output) else { return; };

                let pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();
                // Keep the single source of truth in sync with the relative
                // path above -- the cursor renderer (src/cursor.rs) reads
                // `pointer_location` regardless of which input path moved it.
                self.pointer_location = pos;

                let serial = SERIAL_COUNTER.next_serial();

                let pointer = self.seat.get_pointer().expect("pointer added to seat at startup");

                let under = self.surface_under(pos);

                pointer.motion(
                    self,
                    under,
                    &MotionEvent {
                        location: pos,
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);

                self.focus_follows_pointer(pos);
            }
            InputEvent::PointerButton { event, .. } => {
                let pointer = self.seat.get_pointer().expect("pointer added to seat at startup");
                let keyboard = self.seat.get_keyboard().expect("keyboard added to seat at startup");

                let serial = SERIAL_COUNTER.next_serial();

                let button = event.button_code();

                let button_state = event.state();

                if ButtonState::Pressed == button_state && !pointer.is_grabbed() {
                    if let Some(id) = self.window_id_at(pointer.current_location()) {
                        // Routed through focus_by_id rather than setting seat
                        // focus inline: clicking has to sync active_monitor and
                        // move the model cursor exactly the way every other
                        // focus route does, or clicking a window on the second
                        // head leaves nav chords driving the first.
                        self.focus_by_id(id);
                        // Keyboard focus moved; push a fresh snapshot so the bar
                        // tracks click-to-focus, not just nav chords.
                        self.ipc_dirty = true;
                    } else if self.surface_under(pointer.current_location()).is_none() {
                        // No toplevel *and* no layer surface under the pointer: a
                        // genuine empty-desktop click, so drop keyboard focus. A
                        // click that landed on a (non-keyboard) layer surface --
                        // e.g. a mako notification or the bar -- must NOT steal
                        // focus from the active window, so it's excluded here.
                        self.space.elements().for_each(|window| {
                            window.set_activated(false);
                            let Some(toplevel) = window.toplevel() else { return; };
                            toplevel.send_pending_configure();
                        });
                        keyboard.set_focus(self, Option::<KeyboardFocusTarget>::None, serial);
                        self.ipc_dirty = true;
                    }
                };

                pointer.button(
                    self,
                    &ButtonEvent {
                        button,
                        state: button_state,
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
            }
            InputEvent::PointerAxis { event, .. } => {
                let source = event.source();

                let horizontal_amount = event
                    .amount(Axis::Horizontal)
                    .unwrap_or_else(|| event.amount_v120(Axis::Horizontal).unwrap_or(0.0) * 15.0 / 120.);
                let vertical_amount = event
                    .amount(Axis::Vertical)
                    .unwrap_or_else(|| event.amount_v120(Axis::Vertical).unwrap_or(0.0) * 15.0 / 120.);
                let horizontal_amount_discrete = event.amount_v120(Axis::Horizontal);
                let vertical_amount_discrete = event.amount_v120(Axis::Vertical);

                let mut frame = AxisFrame::new(event.time_msec()).source(source);
                if horizontal_amount != 0.0 {
                    frame = frame.value(Axis::Horizontal, horizontal_amount);
                    if let Some(discrete) = horizontal_amount_discrete {
                        frame = frame.v120(Axis::Horizontal, discrete as i32);
                    }
                }
                if vertical_amount != 0.0 {
                    frame = frame.value(Axis::Vertical, vertical_amount);
                    if let Some(discrete) = vertical_amount_discrete {
                        frame = frame.v120(Axis::Vertical, discrete as i32);
                    }
                }

                if source == AxisSource::Finger {
                    if event.amount(Axis::Horizontal) == Some(0.0) {
                        frame = frame.stop(Axis::Horizontal);
                    }
                    if event.amount(Axis::Vertical) == Some(0.0) {
                        frame = frame.stop(Axis::Vertical);
                    }
                }

                let pointer = self.seat.get_pointer().expect("pointer added to seat at startup");
                pointer.axis(self, frame);
                pointer.frame(self);
            }
            _ => {}
        }
    }

    /// Apply a navigation chord to the model, then re-tile. Step 1 proves the
    /// interception path only -- the model is still a single Group, so this just
    /// confirms the chord reached us (and never reached the client). Steps 2-3
    /// promote the model to a Monitor and wire scroll/rotate/move here.
    pub(crate) fn dispatch_nav(&mut self, action: NavAction) {
        // Captured before dispatch so the tail can tell whether this action
        // actually moved focus -- a Spawn or an SDR-white nudge must not warp
        // the pointer just because dispatch_nav ran.
        let focus_before = self.focused_window_id();
        // Focus identity alone is not enough: a move action carries the focused
        // window to a new slot without ever changing which window is focused, so
        // the pointer has to follow the geometry too.
        let origin_before = focus_before.and_then(|id| self.settled_window_location(id));

        // Motion actions reposition the active column/group, so keyboard focus
        // should follow to whatever now sits in the active slot. Spawn (its focus
        // is a separate on-map concern) and the MoveToNewColumn stub do not.
        // RemoveWindow belongs here too: it evicts the focused window, so seat
        // focus is left pointing at a dead surface unless something moves it to
        // whatever the model now considers active in that slot.
        let refocus = matches!(
            action,
            NavAction::RotateColumnsLeft
                | NavAction::RotateColumnsRight
                | NavAction::ScrollColumnUp
                | NavAction::ScrollColumnDown
                | NavAction::MoveActiveColumnLeft
                | NavAction::MoveActiveColumnRight
                | NavAction::NewGroup
                | NavAction::RemoveWindow
        );

        // Maximize is transient: anything that moves through the layout drops it,
        // without waiting on a focus change to notice. Rotating onto an EMPTY
        // group is the case that forces this -- there is no new window to take
        // focus, so a focus-driven release never fires and the maximized window
        // sits over the top of everything that rotated in.
        //
        // Excluded: the toggle itself (it would clear the state it is about to
        // read, so a second press could never un-maximize) and the display
        // adjustments, which don't move anything.
        let keeps_maximize = matches!(
            action,
            NavAction::ToggleMaximize
                | NavAction::ToggleMaximizeReverse
                | NavAction::IncreaseSdrWhite
                | NavAction::DecreaseSdrWhite
                | NavAction::ToggleHdr
                | NavAction::ToggleFocusFollowsMouse
                | NavAction::ToggleMouseFollowsFocus
                | NavAction::Quit
        );
        if !keeps_maximize {
            self.maximized = MaximizeState::None;
        }

        match action {
            NavAction::RotateColumnsLeft => {
                self.pending_transition = Some(Transition::Rotate);
                if let Some(monitor) = self.workspace.active_monitor_mut() { monitor.rotate_columns(-1); }
            },
            NavAction::RotateColumnsRight => {
                self.pending_transition = Some(Transition::Rotate);
                if let Some(monitor) = self.workspace.active_monitor_mut() { monitor.rotate_columns(1); }
            },
            NavAction::ScrollColumnUp => {
                self.pending_transition = Some(Transition::Scroll { down: false });
                if let Some(monitor) = self.workspace.active_monitor_mut() { monitor.scroll_active_column(-1); }
            },
            NavAction::ScrollColumnDown => {
                self.pending_transition = Some(Transition::Scroll { down: true });
                if let Some(monitor) = self.workspace.active_monitor_mut() { monitor.scroll_active_column(1); }
            },
            NavAction::ToggleFocusFollowsMouse => {
                self.focus_follows_mouse = !self.focus_follows_mouse;
                tracing::info!("focus follows mouse: {}", self.focus_follows_mouse);
                // Takes effect on the next pointer motion rather than adopting
                // whatever happens to sit under a stationary cursor -- turning it
                // on should not itself move focus.
            }
            NavAction::ToggleMouseFollowsFocus => {
                self.mouse_follows_focus = !self.mouse_follows_focus;
                tracing::info!("mouse follows focus: {}", self.mouse_follows_focus);
                // Takes effect on the next focus change rather than warping
                // immediately -- turning it on should not itself move the pointer.
            }
            NavAction::MoveToNewColumn => { self.move_focused_window_to_new_column() },
            NavAction::MoveActiveColumnLeft => {
                if let Some(monitor) = self.workspace.active_monitor_mut() { monitor.move_active_column(-1); }
            },
            NavAction::MoveActiveColumnRight => {
                if let Some(monitor) = self.workspace.active_monitor_mut() { monitor.move_active_column(1); }
            },
            NavAction::NewGroup => {
                if let Some(monitor) = self.workspace.active_monitor_mut() { monitor.grow_active_column(); }
            },
            NavAction::Spawn(command) => {
                // Set DISPLAY explicitly from the live XWayland display number so
                // spawned X11 clients find the server regardless of when XWayland
                // became ready or what stale value the inherited env holds. None
                // until XWayland signals Ready -- native-Wayland clients don't
                // need it anyway (they use WAYLAND_DISPLAY).
                let mut cmd = std::process::Command::new("sh");
                cmd.arg("-c").arg(&command);
                if let Some(n) = self.xdisplay {
                    cmd.env("DISPLAY", format!(":{n}"));
                }
                // Newly launched apps pick up the current wallpaper theme
                // this way; see `RubixState::theme_env`.
                cmd.envs(self.theme_env.iter().cloned());
                cmd.spawn().ok();
            },
            NavAction::Quit => {
                tracing::info!("quit requested; stopping event loop");
                self.loop_signal.stop();
            },
            NavAction::IncrementVisibleColumns => {
                if let Some(monitor) = self.workspace.active_monitor_mut() { monitor.increment_visible_columns(1); }
            },
            NavAction::DecrementVisibleColumns => {
                if let Some(monitor) = self.workspace.active_monitor_mut() { monitor.increment_visible_columns(-1); }
            },
            NavAction::ToggleMaximize => self.cycle_maximize(true),
            NavAction::ToggleMaximizeReverse => self.cycle_maximize(false),
            NavAction::FocusFullscreen => self.focus_next_fullscreen(),
            NavAction::FlipSplitDirection => self.flip_focused_parent_split_direction(),
            NavAction::MoveFocusedWindowUp => self.move_focused_window_by_direction(Direction::Up),
            NavAction::MoveFocusedWindowDown => self.move_focused_window_by_direction(Direction::Down),
            NavAction::MoveFocusedWindowLeft => self.move_focused_window_by_direction(Direction::Left),
            NavAction::MoveFocusedWindowRight => self.move_focused_window_by_direction(Direction::Right),
            NavAction::RemoveWindow => {
                if let Some(id) = self.focused_window_id() {
                    // Ask nicely first -- send_close/close are requests, not
                    // guarantees, which is exactly why the eviction below is
                    // unconditional: a dead or unresponsive client must not be
                    // able to keep its frame on screen (window 74, the
                    // undismissable blank box left behind by the 2026-08-25
                    // XWayland crash, is what this exists to fix).
                    if let Some(window) = self.windows.get(&id) {
                        if let Some(toplevel) = window.toplevel() {
                            toplevel.send_close();
                        } else if let Some(x11) = window.x11_surface() {
                            let _ = x11.close();
                        }
                    }
                    self.remove_window_by_id(id);
                    tracing::info!("window {id} removed via RemoveWindow ({} tracked)", self.windows.len());
                }
            },
            NavAction::IncreaseSdrWhite => {
                self.sdr_white_nits = (self.sdr_white_nits + SDR_WHITE_STEP).clamp(80.0, 300.0);
                // apply_layout below doesn't touch geometry for this action, so
                // the udev backend (which renders on demand) would otherwise
                // stay on the last-painted frame until something else dirties
                // the screen -- force one now, same as screencopy's nudge.
                self.nudge_render();
            },
            NavAction::DecreaseSdrWhite => {
                self.sdr_white_nits = (self.sdr_white_nits - SDR_WHITE_STEP).clamp(80.0, 300.0);
                self.nudge_render();
            },
            NavAction::ToggleHdr => {
                // toggle_hdr does its own render scheduling; don't also fall
                // through to apply_layout for geometry it doesn't need.
                self.toggle_hdr();
            },
        }
        self.apply_layout();
        if refocus {
            self.focus_active_window();
        }
        let focus_after = self.focused_window_id();
        let moved = focus_after != focus_before
            || focus_after.and_then(|id| self.settled_window_location(id)) != origin_before;
        if moved {
            self.warp_pointer_to_focused_window();
        }
        self.ipc_dirty = true;
    }

    /// Focus a specific window by id: raise it, mark it the sole activated
    /// toplevel, and set seat keyboard focus to its surface. No-op if the id
    /// isn't tracked or has no surface yet. This is the generic primitive --
    /// `focus_active_window` wraps it with the model-derived active id, on-map
    /// handlers pass the freshly-created id, and directional-move chords will
    /// pass the destination id.
    pub(crate) fn focus_by_id(&mut self, id: u32) {
        self.focus_by_id_raising(id, true);
    }

    /// As `focus_by_id`, but leaves the stacking order alone.
    ///
    /// Focus and raise are independent: focus decides who gets keys, raise
    /// decides who paints on top. Click-to-focus and explicit activation raise
    /// because the user gestured at the window; hover must not, or sweeping the
    /// pointer across the screen reshuffles z-order with no gesture behind it.
    /// Matches sway, whose focus-follows-mouse likewise does not raise.
    pub(crate) fn focus_by_id_without_raising(&mut self, id: u32) {
        self.focus_by_id_raising(id, false);
    }

    fn focus_by_id_raising(&mut self, id: u32, raise: bool) {
        let Some(window) = self.windows.get(&id).cloned() else { return; };
        // Captured before focus moves: `apply_layout` hands a fullscreen window
        // a rect ONLY while it is focused, so focus entering or leaving the
        // fullscreen set changes the layout even though the grid is untouched.
        let previous_focus = self.focused_window_id();
        // X11 windows focus as their X11Surface so input focus is actually
        // driven (XSetInputFocus/WM_TAKE_FOCUS); wayland windows as wl_surface.
        let Some(target) = KeyboardFocusTarget::from_window(&window) else { return; };

        // The window's own head becomes the active one, so nav chords act on
        // what the user just selected rather than wherever the cursor was left.
        if let Some(monitor_id) = self.workspace.get_monitor_id_by_window_id(id) {
            self.set_active_monitor(monitor_id);
        }

        // Reveal, THEN move the cursor. Order matters: the Swapped branch
        // relocates the group, and focus_window resolves coordinates through
        // locate, so running it second is what makes it read the post-swap
        // position. Fullscreen windows sit outside the grid, so both calls find
        // nothing and no-op -- focus still lands, which is how a hidden X11
        // fullscreen client gets restored.
        let revealed = self.workspace.active_monitor_mut().and_then(|monitor| {
            let kind = monitor.reveal_window(id);
            monitor.focus_window(id);
            kind
        });

        // Arm the transition BEFORE any apply_layout -- that call consumes
        // pending_transition via take(), so arming afterwards animates nothing.
        match revealed {
            Some(RevealKind::Scrolled { down }) => {
                self.pending_transition = Some(Transition::Scroll { down });
            }
            // The swap trades two non-adjacent groups, so there is no edge to
            // slide toward: the revealed group grows in place while the
            // displaced one shrinks away.
            Some(RevealKind::Swapped) => {
                self.pending_transition = Some(Transition::Reveal);
            }
            Some(RevealKind::AlreadyVisible) | None => {}
        }

        let serial = SERIAL_COUNTER.next_serial();
        let keyboard = self.seat.get_keyboard().expect("keyboard added to seat at startup");
        if raise {
            self.space.raise_element(&window, true);
        }
        self.space.elements().for_each(|w| {
            w.set_activated(w == &window);
            let Some(toplevel) = w.toplevel() else { return; };
            toplevel.send_pending_configure();
        });
        // A Globally Active X11 client (WM_HINTS input=False plus WM_TAKE_FOCUS)
        // never has focus forced on it -- smithay only offers, and the client
        // takes focus by its own XSetInputFocus. A client that believes it is
        // iconified declines that offer, and hiding is exactly what Rubix just
        // did to it. So make the window viewable again BEFORE offering focus;
        // offering first is offering to a client that is certain to refuse, and
        // the refusal is silent.
        if self.iconified.remove(&id) {
            if let Some(WindowSurface::X11(x11)) = self.windows.get(&id).map(|w| w.underlying_surface()) {
                let _ = x11.set_hidden(false);
                let _ = x11.set_mapped(true);
            }
        }
        keyboard.set_focus(self, Some(target), serial);
        self.reconcile_focus_state();

        // Re-lay-out when the reveal restructured something, OR when focus
        // crossed the fullscreen set in either direction.
        //
        // The reveal half: AlreadyVisible and None leave the grid byte-identical,
        // and an unconditional apply_layout here would take the snap path --
        // whose first act is settle_tweens() -- immediately after nav dispatch
        // armed and started an animation, killing every nav transition on the
        // focus_active_window that follows it.
        //
        // The fullscreen half: fullscreen windows are out of the grid, so the
        // reveal is always a no-op for them and the test above never fires. But
        // `apply_layout` only emits a rect for the *focused* fullscreen window,
        // so focus arriving at one is precisely when it needs a layout pass. Any
        // client that requested fullscreen before its first commit hits this: the
        // map path lays out first and focuses second, so the window is unfocused
        // at layout time, gets no rect, and never reaches the Space -- invisible
        // until some unrelated event happens to run a layout. Leaving one matters
        // for the mirror reason: its rect has to stop being emitted.
        //
        // Deliberately narrower than "anything is fullscreen". That version would
        // settle tweens on every focus change while a game sat unfocused in the
        // background, killing nav animation for the whole session.
        let crossed_fullscreen = self.fullscreen_windows.contains(&id)
            || previous_focus.is_some_and(|prev| self.fullscreen_windows.contains(&prev));
        if crossed_fullscreen
            || matches!(revealed, Some(RevealKind::Scrolled { .. }) | Some(RevealKind::Swapped))
        {
            self.apply_layout();
        }
    }

    /// Move keyboard focus to the model's current active window, mirroring the
    /// pointer-click focus path. Derived fresh from the model each call --
    /// `active_window` walks active_column -> active_row -> first leaf, so it
    /// always tracks the latest nav. A `None` target (empty active band) clears
    /// focus; nav chords still work because the input filter intercepts them
    /// regardless of who holds focus.
    pub(crate) fn focus_active_window(&mut self) {
        match self.workspace.active_monitor().and_then(|m| m.active_window()) {
            Some(id) => self.focus_by_id(id),
            None => {
                let serial = SERIAL_COUNTER.next_serial();
                let keyboard = self.seat.get_keyboard().expect("keyboard added to seat at startup");
                self.space.elements().for_each(|w| {
                    w.set_activated(false);
                    let Some(toplevel) = w.toplevel() else { return; };
                    toplevel.send_pending_configure();
                });
                keyboard.set_focus(self, Option::<KeyboardFocusTarget>::None, serial);
                // Focus went nowhere -- rotating onto an empty group takes this
                // branch, never `focus_by_id`, so this is the only place the
                // reconcile can happen for it.
                self.reconcile_focus_state();
            }
        }
    }
}
