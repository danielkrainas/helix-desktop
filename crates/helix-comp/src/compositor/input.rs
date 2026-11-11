//! Input dispatch: keyboard, pointer, click-to-focus.

use smithay::backend::input::{
    AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
    KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent,
};
use smithay::backend::input::KeyState;
use smithay::input::keyboard::FilterResult;
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent};
use smithay::utils::SERIAL_COUNTER;

use super::shortcuts::{Action, Mods};
use super::Helix;

impl Helix {
    fn run_shortcut(&mut self, action: Action) {
        tracing::debug!(?action, "shortcut");
        match action {
            Action::ShowApplications => self.send(helix_comp::Event::ShowApplications),
            Action::CycleWindows => self.cycle_windows(),
            Action::CloseWindow => self.close_focused(),
            Action::ToggleMaximize => self.toggle_maximize_focused(),
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
                    self.run_shortcut(action);
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
                let serial = SERIAL_COUNTER.next_serial();
                let pointer = self.seat.get_pointer().unwrap();
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
                let pointer = self.seat.get_pointer().unwrap();
                let serial = SERIAL_COUNTER.next_serial();
                let state = event.state();

                if state == ButtonState::Pressed && !pointer.is_grabbed() {
                    let pos = pointer.current_location();
                    if self.over_panel(pos) {
                        // Clicks on panels don't change window focus.
                    } else if let Some(window) = self.space.element_under(pos).map(|(w, _)| w.clone()) {
                        self.focus_window(&window);
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
