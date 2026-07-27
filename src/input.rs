use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
        KeyState, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent,
    },
    input::{
        keyboard::{
            keysyms::{KEY_XF86Switch_VT_1, KEY_XF86Switch_VT_12},
            FilterResult,
        },
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::SERIAL_COUNTER,
};

use serde::Deserialize;

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
}

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
            InputEvent::PointerMotion { .. } => {}
            InputEvent::PointerMotionAbsolute { event, .. } => {
                let Some(output) = self.space.outputs().next() else { return; };

                let Some(output_geo) = self.space.output_geometry(output) else { return; };

                let pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();

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
                        let Some(toplevel) = window.toplevel() else { return; };
                        keyboard.set_focus(
                            self,
                            Some(toplevel.wl_surface().clone()),
                            serial,
                        );
                        self.space.elements().for_each(|window| {
                            let Some(toplevel) = window.toplevel() else { return; };
                            toplevel.send_pending_configure();
                        });
                    } else {
                        self.space.elements().for_each(|window| {
                            window.set_activated(false);
                            let Some(toplevel) = window.toplevel() else { return; };
                            toplevel.send_pending_configure();
                        });
                        keyboard.set_focus(self, Option::<WlSurface>::None, serial);
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
    fn dispatch_nav(&mut self, action: NavAction) {
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
            NavAction::RotateColumnsLeft => { self.pending_transition = Some(Transition::Rotate); self.monitor.rotate_columns(-1); },
            NavAction::RotateColumnsRight => { self.pending_transition = Some(Transition::Rotate); self.monitor.rotate_columns(1); },
            NavAction::ScrollColumnUp => { self.pending_transition = Some(Transition::Scroll { down: false }); self.monitor.scroll_active_column(-1); },
            NavAction::ScrollColumnDown => { self.pending_transition = Some(Transition::Scroll { down: true }); self.monitor.scroll_active_column(1); },
            NavAction::MoveToNewColumn => { self.move_focused_window_to_new_column() },
            NavAction::MoveActiveColumnLeft => self.monitor.move_active_column(-1),
            NavAction::MoveActiveColumnRight => self.monitor.move_active_column(1),
            NavAction::NewGroup => self.monitor.grow_active_column(),
            NavAction::Spawn(command) => {
                std::process::Command::new("sh").arg("-c").arg(&command).spawn().ok();
            },
            NavAction::Quit => {
                tracing::info!("quit requested; stopping event loop");
                self.loop_signal.stop();
            },
            NavAction::IncrementVisibleColumns => self.monitor.increment_visible_columns(1),
            NavAction::DecrementVisibleColumns => self.monitor.increment_visible_columns(-1),
            NavAction::FlipSplitDirection => self.flip_focused_parent_split_direction(),
            NavAction::MoveFocusedWindowUp => self.move_focused_window_by_direction(Direction::Up),
            NavAction::MoveFocusedWindowDown => self.move_focused_window_by_direction(Direction::Down),
            NavAction::MoveFocusedWindowLeft => self.move_focused_window_by_direction(Direction::Left),
            NavAction::MoveFocusedWindowRight => self.move_focused_window_by_direction(Direction::Right),
        }
        self.apply_layout();
        if refocus {
            self.focus_active_window();
        }
    }

    /// Move keyboard focus to the model's current active window, mirroring the
    /// pointer-click focus path (raise + activate-toggle + set_focus). Derived
    /// fresh from the model each call -- `active_window` walks active_column ->
    /// active_row -> first leaf, so it always tracks the latest nav. A `None`
    /// target (empty active band) clears focus; nav chords still work because
    /// the input filter intercepts them regardless of who holds focus.
    fn focus_active_window(&mut self) {
        let serial = SERIAL_COUNTER.next_serial();
        let keyboard = self.seat.get_keyboard().expect("keyboard added to seat at startup");
        let target = self
            .monitor
            .active_window()
            .and_then(|id| self.windows.get(&id).cloned());

        match target {
            Some(window) => {
                self.space.raise_element(&window, true);
                let Some(toplevel) = window.toplevel() else { return; };
                let surface = toplevel.wl_surface().clone();
                self.space.elements().for_each(|w| {
                    w.set_activated(w == &window);
                    let Some(toplevel) = w.toplevel() else { return; };
                    toplevel.send_pending_configure();
                });
                keyboard.set_focus(self, Some(surface), serial);
            }
            None => {
                self.space.elements().for_each(|w| {
                    w.set_activated(false);
                    let Some(toplevel) = w.toplevel() else { return; };
                    toplevel.send_pending_configure();
                });
                keyboard.set_focus(self, Option::<WlSurface>::None, serial);
            }
        }
    }
}
