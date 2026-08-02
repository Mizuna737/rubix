use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
        KeyState, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
    },
    input::{
        keyboard::{
            keysyms::{KEY_XF86Switch_VT_1, KEY_XF86Switch_VT_12},
            FilterResult,
        },
        pointer::{AxisFrame, ButtonEvent, MotionEvent, RelativeMotionEvent},
    },
    utils::SERIAL_COUNTER,
    wayland::pointer_constraints::{PointerConstraint, with_pointer_constraint},
};

use serde::Deserialize;

use crate::focus::KeyboardFocusTarget;
use crate::state::{RubixState, Transition};
use crate::model::grid::Direction;

/// A Rubix navigation chord. Bound to a chord in `config.toml`, where the value
/// is this variant's exact name -- serde deserializes it straight into the enum,
/// so there is no chord-name translation table in code. The direction is baked
/// into the variant name; the motion sign is derived once at the dispatch site.
#[derive(Debug, Clone, Deserialize)]
pub(crate) enum NavAction {
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

impl RubixState {
    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
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
                        match state.config.keybinds.iter().find(|kb| kb.matches(mods, sym)) {
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
                let proposed = self.pointer_location + delta;

                let pointer = self.seat.get_pointer().expect("pointer added to seat at startup");
                let focused_surface = pointer.current_focus();

                // Check for pointer constraints on the focused surface
                let (loc, send_relative) = if let Some(ref surface) = focused_surface {
                    with_pointer_constraint(surface, &pointer, |constraint| {
                        match constraint {
                            Some(ref constraint_ref) => {
                                // Pointer is locked or confined
                                match &**constraint_ref {
                                    PointerConstraint::Locked(_) => {
                                        // Locked pointer: position stays frozen, send relative deltas
                                        // The client expects motion events at the lock position with relative deltas
                                        (self.pointer_location, true)
                                    }
                                    PointerConstraint::Confined(_) => {
                                        // Confined pointer: clamp to confinement region
                                        // For now, we'll just clamp to output as before
                                        // (full region support would require checking constraint.region())
                                        let current_output = self
                                            .output_at(self.pointer_location)
                                            .or_else(|| self.space.outputs().next().cloned());
                                        let Some(current_output) = current_output else { return (self.pointer_location, false); };
                                        let Some(output_geo) = self.space.output_geometry(&current_output) else { return (self.pointer_location, false); };

                                        let mut clamped = proposed;
                                        clamped.x = clamped.x.clamp(output_geo.loc.x as f64, (output_geo.loc.x + output_geo.size.w) as f64);
                                        clamped.y = clamped.y.clamp(output_geo.loc.y as f64, (output_geo.loc.y + output_geo.size.h) as f64);
                                        (clamped, false)
                                    }
                                }
                            }
                            None => {
                                // No constraint: normal clamping behavior
                                let loc = if self.output_at(proposed).is_some() {
                                    proposed
                                } else {
                                    let current_output = self
                                        .output_at(self.pointer_location)
                                        .or_else(|| self.space.outputs().next().cloned());
                                    let Some(current_output) = current_output else { return (self.pointer_location, false); };
                                    let Some(output_geo) = self.space.output_geometry(&current_output) else { return (self.pointer_location, false); };

                                    let mut clamped = proposed;
                                    clamped.x = clamped.x.clamp(output_geo.loc.x as f64, (output_geo.loc.x + output_geo.size.w) as f64);
                                    clamped.y = clamped.y.clamp(output_geo.loc.y as f64, (output_geo.loc.y + output_geo.size.h) as f64);
                                    clamped
                                };
                                (loc, false)
                            }
                        }
                    })
                } else {
                    // No focused surface: normal clamping
                    let loc = if self.output_at(proposed).is_some() {
                        proposed
                    } else {
                        let current_output = self
                            .output_at(self.pointer_location)
                            .or_else(|| self.space.outputs().next().cloned());
                        let Some(current_output) = current_output else { return; };
                        let Some(output_geo) = self.space.output_geometry(&current_output) else { return; };

                        let mut clamped = proposed;
                        clamped.x = clamped.x.clamp(output_geo.loc.x as f64, (output_geo.loc.x + output_geo.size.w) as f64);
                        clamped.y = clamped.y.clamp(output_geo.loc.y as f64, (output_geo.loc.y + output_geo.size.h) as f64);
                        clamped
                    };
                    (loc, false)
                };

                self.pointer_location = loc;

                let serial = SERIAL_COUNTER.next_serial();
                let under = self.surface_under(loc);

                // If pointer is locked, send relative motion event
                if send_relative {
                    if let Some(ref surface) = focused_surface {
                        pointer.relative_motion(
                            self,
                            Some((surface.clone(), loc)),
                            &RelativeMotionEvent {
                                delta,
                                delta_unaccel: delta,
                                utime: event.time_msec() as u64 * 1_000_000, // Convert ms to us
                            },
                        );
                    }
                }

                // Always send motion event (even for locked pointers, to maintain protocol compliance)
                pointer.motion(
                    self,
                    under,
                    &MotionEvent {
                        location: loc,
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
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
            }
            InputEvent::PointerButton { event, .. } => {
                let pointer = self.seat.get_pointer().expect("pointer added to seat at startup");
                let keyboard = self.seat.get_keyboard().expect("keyboard added to seat at startup");

                let serial = SERIAL_COUNTER.next_serial();

                let button = event.button_code();

                let button_state = event.state();

                if ButtonState::Pressed == button_state && !pointer.is_grabbed() {
                    if let Some((window, _loc)) = self
                        .space
                        .element_under(pointer.current_location())
                        .map(|(w, l)| (w.clone(), l))
                    {
                        self.space.raise_element(&window, true);
                        // from_window keeps X11 clicks working -- window.toplevel()
                        // is None for X11, so the old early-return dropped focus
                        // entirely on click.
                        let target = KeyboardFocusTarget::from_window(&window);
                        self.space.elements().for_each(|w| {
                            w.set_activated(w == &window);
                            let Some(toplevel) = w.toplevel() else { return; };
                            toplevel.send_pending_configure();
                        });
                        keyboard.set_focus(self, target, serial);
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
        // Motion actions reposition the active column/group, so keyboard focus
        // should follow to whatever now sits in the active slot. Spawn (its focus
        // is a separate on-map concern) and the MoveToNewColumn stub do not.
        let refocus = matches!(
            action,
            NavAction::RotateColumnsLeft
                | NavAction::RotateColumnsRight
                | NavAction::ScrollColumnUp
                | NavAction::ScrollColumnDown
                | NavAction::MoveActiveColumnLeft
                | NavAction::MoveActiveColumnRight
                | NavAction::NewGroup
        );

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
            NavAction::FlipSplitDirection => self.flip_focused_parent_split_direction(),
            NavAction::MoveFocusedWindowUp => self.move_focused_window_by_direction(Direction::Up),
            NavAction::MoveFocusedWindowDown => self.move_focused_window_by_direction(Direction::Down),
            NavAction::MoveFocusedWindowLeft => self.move_focused_window_by_direction(Direction::Left),
            NavAction::MoveFocusedWindowRight => self.move_focused_window_by_direction(Direction::Right),
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
        self.ipc_dirty = true;
    }

    /// Focus a specific window by id: raise it, mark it the sole activated
    /// toplevel, and set seat keyboard focus to its surface. No-op if the id
    /// isn't tracked or has no surface yet. This is the generic primitive --
    /// `focus_active_window` wraps it with the model-derived active id, on-map
    /// handlers pass the freshly-created id, and directional-move chords will
    /// pass the destination id.
    pub(crate) fn focus_by_id(&mut self, id: u32) {
        let Some(window) = self.windows.get(&id).cloned() else { return; };
        // X11 windows focus as their X11Surface so input focus is actually
        // driven (XSetInputFocus/WM_TAKE_FOCUS); wayland windows as wl_surface.
        let Some(target) = KeyboardFocusTarget::from_window(&window) else { return; };
        let serial = SERIAL_COUNTER.next_serial();
        let keyboard = self.seat.get_keyboard().expect("keyboard added to seat at startup");
        self.space.raise_element(&window, true);
        self.space.elements().for_each(|w| {
            w.set_activated(w == &window);
            let Some(toplevel) = w.toplevel() else { return; };
            toplevel.send_pending_configure();
        });
        keyboard.set_focus(self, Some(target), serial);
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
            }
        }
    }
}
