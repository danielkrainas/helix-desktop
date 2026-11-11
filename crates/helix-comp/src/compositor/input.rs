//! Input dispatch: keyboard, pointer, click-to-focus.

use smithay::backend::input::{
    AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
    KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
};
use smithay::utils::{Logical, Point};
use smithay::backend::input::KeyState;
use smithay::input::keyboard::FilterResult;
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent};
use smithay::utils::SERIAL_COUNTER;

use super::shortcuts::{Action, Mods};

const BTN_LEFT: u32 = 0x110;
use super::Helix;

impl Helix {
    fn pointer_to(&mut self, pos: Point<f64, Logical>, time: u32) {
        let serial = SERIAL_COUNTER.next_serial();
        let pointer = self.seat.get_pointer().unwrap();
        let under = self.surface_under(pos);
        pointer.motion(self, under, &MotionEvent { location: pos, serial, time });
        pointer.frame(self);
    }

    /// Keep the pointer within the outputs' combined bounding box.
    fn clamp_to_outputs(&self, pos: Point<f64, Logical>) -> Point<f64, Logical> {
        let bounds = self
            .space
            .outputs()
            .filter_map(|o| self.space.output_geometry(o))
            .reduce(|a, b| a.merge(b));
        match bounds {
            Some(b) => {
                let max_x = f64::from(b.loc.x + b.size.w - 1);
                let max_y = f64::from(b.loc.y + b.size.h - 1);
                (pos.x.clamp(f64::from(b.loc.x), max_x), pos.y.clamp(f64::from(b.loc.y), max_y)).into()
            }
            None => pos,
        }
    }

    fn run_shortcut(&mut self, action: Action, time: u32) {
        tracing::debug!(?action, "shortcut");
        // While locked only the escape hatches work.
        if self.lock.locked && !matches!(action, Action::Quit | Action::SwitchVt(_)) {
            return;
        }
        match action {
            Action::Lock => self.send(helix_comp::Event::Lock),
            Action::Binding(id, pressed) => {
                if let Some(obj) = self.keybindings.object(id) {
                    if pressed {
                        obj.pressed(time);
                    } else {
                        obj.released(time);
                    }
                }
            }
            Action::ShowApplications => self.send(helix_comp::Event::ShowApplications),
            Action::CloseWindow => self.close_focused(),
            Action::ToggleMaximize => self.toggle_maximize_focused(),
            Action::SwitchVt(vt) => self.switch_vt(vt),
            Action::Quit => {
                tracing::info!("quit requested (Ctrl+Alt+Backspace)");
                self.loop_signal.stop();
            }
            Action::RestoreOrMinimize => {
                if let Some(window) = self.top_window() {
                    if self.is_taken_over(&window, super::wm::Takeover::Maximized) {
                        self.restore_window(&window);
                    } else {
                        self.minimize(&window);
                    }
                }
            }
        }
    }

    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        if matches!(
            event,
            InputEvent::Keyboard { .. }
                | InputEvent::PointerMotion { .. }
                | InputEvent::PointerMotionAbsolute { .. }
                | InputEvent::PointerButton { .. }
                | InputEvent::PointerAxis { .. }
        ) {
            self.note_activity();
        }

        match event {
            InputEvent::Keyboard { event, .. } => {
                let serial = SERIAL_COUNTER.next_serial();
                let time = Event::time_msec(&event);
                let pressed = event.state() == KeyState::Pressed;
                let keycode = event.key_code();
                let action = self.seat.get_keyboard().unwrap().input::<Option<Action>, _>(
                    self,
                    keycode,
                    event.state(),
                    serial,
                    time,
                    |state, modifiers, handle| {
                        let mods = Mods {
                            alt: modifiers.alt,
                            logo: modifiers.logo,
                            ctrl: modifiers.ctrl,
                            shift: modifiers.shift,
                        };
                        let sym = handle
                            .raw_latin_sym_or_raw_current_sym()
                            .unwrap_or_else(|| handle.modified_sym());
                        let decision = state.shortcuts.key(keycode.raw(), pressed, mods, sym);
                        if decision.intercept {
                            FilterResult::Intercept(decision.action)
                        } else {
                            FilterResult::Forward
                        }
                    },
                );
                if let Some(Some(action)) = action {
                    self.run_shortcut(action, time);
                }
            }
            InputEvent::PointerMotionAbsolute { event, .. } => {
                let Some(output) = self.space.outputs().next() else {
                    return;
                };
                let Some(output_geo) = self.space.output_geometry(output) else {
                    return;
                };
                let pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();
                self.pointer_to(pos, event.time_msec());
            }
            InputEvent::PointerMotion { event, .. } => {
                // Relative motion (real mice/touchpads on bare hardware).
                let current = self.seat.get_pointer().unwrap().current_location();
                let pos = self.clamp_to_outputs(current + event.delta());
                self.pointer_to(pos, event.time_msec());
            }
            InputEvent::PointerButton { event, .. } => {
                let pointer = self.seat.get_pointer().unwrap();
                let serial = SERIAL_COUNTER.next_serial();
                let state = event.state();

                if state == ButtonState::Pressed && !pointer.is_grabbed() && !self.lock.locked {
                    let pos = pointer.current_location();
                    let super_held = self
                        .seat
                        .get_keyboard()
                        .is_some_and(|k| k.modifier_state().logo);
                    if self.over_panel(pos) {
                        // Clicks on panels don't change window focus.
                    } else if let Some(window) = self.space.element_under(pos).map(|(w, _)| w.clone()) {
                        self.focus_window(&window);
                        // Super+drag moves any window, including X11 windows
                        // without a title bar of their own.
                        if super_held && event.button_code() == BTN_LEFT && !super::wm::is_override_redirect(&window) {
                            self.restore_window(&window);
                            let initial_location = self.space.element_location(&window).unwrap_or_default();
                            let start_data = smithay::input::pointer::GrabStartData {
                                focus: None,
                                button: BTN_LEFT,
                                location: pos,
                            };
                            pointer.set_grab(
                                self,
                                super::grabs::MoveGrab { start_data, window, initial_location },
                                serial,
                                smithay::input::pointer::Focus::Clear,
                            );
                            return;
                        }
                    } else {
                        self.unfocus_all();
                    }
                }

                pointer.button(
                    self,
                    &ButtonEvent {
                        button: event.button_code(),
                        state,
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
            }
            InputEvent::PointerAxis { event, .. } => {
                let source = event.source();
                let amount = |axis| {
                    event
                        .amount(axis)
                        .unwrap_or_else(|| event.amount_v120(axis).unwrap_or(0.0) * 15.0 / 120.0)
                };
                let mut frame = AxisFrame::new(event.time_msec()).source(source);
                for axis in [Axis::Horizontal, Axis::Vertical] {
                    let value = amount(axis);
                    if value != 0.0 {
                        frame = frame.value(axis, value);
                        if let Some(discrete) = event.amount_v120(axis) {
                            frame = frame.v120(axis, discrete as i32);
                        }
                    }
                    if source == AxisSource::Finger && event.amount(axis) == Some(0.0) {
                        frame = frame.stop(axis);
                    }
                }
                let pointer = self.seat.get_pointer().unwrap();
                pointer.axis(self, frame);
                pointer.frame(self);
            }
            _ => {}
        }
    }
}
