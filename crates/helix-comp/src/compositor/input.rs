//! Input dispatch: keyboard, pointer, click-to-focus.

use smithay::backend::input::{
    AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
    KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent, TouchEvent,
};
use smithay::utils::{Logical, Point};
use smithay::backend::input::KeyState;
use smithay::input::keyboard::FilterResult;
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent};
use smithay::utils::SERIAL_COUNTER;

use super::shortcuts::{Action, Mods};

pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;
use super::Helix;

impl Helix {
    pub(super) fn pointer_to(&mut self, pos: Point<f64, Logical>, time: u32) {
        let serial = SERIAL_COUNTER.next_serial();
        let pointer = self.seat.get_pointer().unwrap();
        let frame = if pointer.is_grabbed() { None } else { self.frame_under(pos) };
        let under = if self.frame_hover(frame.as_ref()) { None } else { self.surface_under(pos) };
        pointer.motion(self, under, &MotionEvent { location: pos, serial, time });
        pointer.frame(self);
        self.focus_follows_pointer(pos);
        self.hot_corner(pos);
    }

    /// Toggle the overview when the pointer reaches the hot corner.
    fn hot_corner(&mut self, pos: Point<f64, Logical>) {
        if !self.wm_settings.hot_corners || self.lock.locked || self.seat.get_pointer().is_some_and(|p| p.is_grabbed()) {
            return;
        }
        let Some(primary) = self.primary() else { return };
        let rect = |g: smithay::utils::Rectangle<i32, Logical>| (g.loc.x, g.loc.y, g.size.w, g.size.h);
        let Some(geo) = self.space.output_geometry(&primary) else { return };
        let outputs: Vec<_> = self.space.outputs().filter_map(|o| self.space.output_geometry(o)).map(rect).collect();
        let Some(corner) = helix_comp::hot_corner::corner(rect(geo), &outputs) else { return };
        if !self.hot_corner.moved(corner, (pos.x, pos.y)) {
            return;
        }
        let fullscreen = self
            .space
            .elements()
            .any(|w| self.is_taken_over(w, super::wm::Takeover::Fullscreen) && self.output_of(w).as_ref() == Some(&primary));
        if !fullscreen {
            tracing::debug!(x = pos.x, y = pos.y, "hot corner");
            self.send(helix_comp::Event::ToggleOverview);
        }
    }

    /// Keep the pointer within the outputs' combined bounding box.
    pub(super) fn clamp_to_outputs(&self, pos: Point<f64, Logical>) -> Point<f64, Logical> {
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

    pub(super) fn run_shortcut(&mut self, action: Action, time: u32) {
        tracing::debug!(?action, "shortcut");
        // While locked only the escape hatches work.
        let allowed = match action {
            Action::Quit | Action::SwitchVt(_) | Action::RestoreShortcuts => true,
            Action::Binding(id, _) => self.shortcuts.works_when_locked(id),
            _ => false,
        };
        if self.lock.locked && !allowed {
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
            Action::ToggleOverview => self.send(helix_comp::Event::ToggleOverview),
            Action::RestoreShortcuts => self.restore_shortcuts(),
            Action::CancelFollow => self.cancel_follow(),
            Action::FollowStep(dx, dy) => {
                let pointer = self.seat.get_pointer().unwrap();
                let pos = self.clamp_to_outputs(pointer.current_location() + Point::from((f64::from(dx), f64::from(dy))));
                self.pointer_to(pos, time);
            }
            Action::EndFollow => {
                // The follow grab ends on a press, which it keeps from apps.
                let pointer = self.seat.get_pointer().unwrap();
                let serial = SERIAL_COUNTER.next_serial();
                pointer.button(self, &ButtonEvent { button: BTN_LEFT, state: ButtonState::Pressed, serial, time });
                pointer.frame(self);
            }
            Action::LocatePointer => self.locate_pointer(),
            Action::SwitchVt(vt) => self.switch_vt(vt),
            Action::Quit => {
                tracing::info!("quit requested (Ctrl+Alt+Backspace)");
                self.loop_signal.stop();
            }
        }
    }

    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        use smithay::backend::input::{GestureBeginEvent, GestureEndEvent, GesturePinchUpdateEvent as _, GestureSwipeUpdateEvent as _};
        if matches!(
            event,
            InputEvent::Keyboard { .. }
                | InputEvent::PointerMotion { .. }
                | InputEvent::PointerMotionAbsolute { .. }
                | InputEvent::PointerButton { .. }
                | InputEvent::PointerAxis { .. }
                | InputEvent::TouchDown { .. }
                | InputEvent::TouchMotion { .. }
                | InputEvent::TabletToolAxis { .. }
                | InputEvent::TabletToolProximity { .. }
                | InputEvent::TabletToolTip { .. }
                | InputEvent::TabletToolButton { .. }
        ) {
            self.note_activity();
        }

        match event {
            InputEvent::Keyboard { event, .. } => {
                self.device_key(event.key_code(), event.state(), Event::time_msec(&event));
            }
            InputEvent::PointerMotionAbsolute { event, .. } => {
                let Some(output) = self.space.outputs().next() else {
                    return;
                };
                let Some(output_geo) = self.space.output_geometry(output) else {
                    return;
                };
                let pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();
                self.pointer_moved_to_host(pos, Event::time(&event), event.time_msec());
                self.device_moved(event.time_msec());
            }
            InputEvent::PointerMotion { event, .. } => {
                // Relative motion (real mice/touchpads on bare hardware).
                self.pointer_moved_by(event.delta(), event.delta_unaccel(), Event::time(&event), event.time_msec());
                self.device_moved(event.time_msec());
            }
            InputEvent::GestureSwipeBegin { event } => self.gesture_swipe_begin(event.fingers(), event.time_msec()),
            InputEvent::GestureSwipeUpdate { event } => self.gesture_swipe_update(event.delta(), event.time_msec()),
            InputEvent::GestureSwipeEnd { event } => self.gesture_swipe_end(event.cancelled(), event.time_msec()),
            InputEvent::GesturePinchBegin { event } => {
                let pointer = self.seat.get_pointer().unwrap();
                let e = smithay::input::pointer::GesturePinchBeginEvent { serial: SERIAL_COUNTER.next_serial(), time: event.time_msec(), fingers: event.fingers() };
                pointer.gesture_pinch_begin(self, &e);
            }
            InputEvent::GesturePinchUpdate { event } => {
                let pointer = self.seat.get_pointer().unwrap();
                let e = smithay::input::pointer::GesturePinchUpdateEvent { time: event.time_msec(), delta: event.delta(), scale: event.scale(), rotation: event.rotation() };
                pointer.gesture_pinch_update(self, &e);
            }
            InputEvent::GesturePinchEnd { event } => {
                let pointer = self.seat.get_pointer().unwrap();
                let e = smithay::input::pointer::GesturePinchEndEvent { serial: SERIAL_COUNTER.next_serial(), time: event.time_msec(), cancelled: event.cancelled() };
                pointer.gesture_pinch_end(self, &e);
            }
            InputEvent::GestureHoldBegin { event } => {
                let pointer = self.seat.get_pointer().unwrap();
                let e = smithay::input::pointer::GestureHoldBeginEvent { serial: SERIAL_COUNTER.next_serial(), time: event.time_msec(), fingers: event.fingers() };
                pointer.gesture_hold_begin(self, &e);
            }
            InputEvent::GestureHoldEnd { event } => {
                let pointer = self.seat.get_pointer().unwrap();
                let e = smithay::input::pointer::GestureHoldEndEvent { serial: SERIAL_COUNTER.next_serial(), time: event.time_msec(), cancelled: event.cancelled() };
                pointer.gesture_hold_end(self, &e);
            }
            InputEvent::PointerButton { event, .. } => {
                self.device_button(event.button_code(), event.state(), event.time_msec());
            }
            InputEvent::PointerAxis { event, .. } => {
                let source = event.source();
                let amount = |axis| {
                    event
                        .amount(axis)
                        .unwrap_or_else(|| event.amount_v120(axis).unwrap_or(0.0) * 15.0 / 120.0)
                };
                if self.input_capture_active().is_some() {
                    let discrete = match (event.amount_v120(Axis::Horizontal), event.amount_v120(Axis::Vertical)) {
                        (None, None) => None,
                        (h, v) => Some((h.unwrap_or(0.0) as i32, v.unwrap_or(0.0) as i32)),
                    };
                    self.capture_scroll(amount(Axis::Horizontal), amount(Axis::Vertical), discrete);
                    return;
                }
                let mut frame = AxisFrame::new(event.time_msec()).source(source);
                // Touchpad scrolling speed (`scroll-speed`).
                let factor = match source {
                    AxisSource::Finger => self.input_settings.as_ref().map_or(1.0, |s| s.touchpad.scroll_speed),
                    _ => 1.0,
                };
                for axis in [Axis::Horizontal, Axis::Vertical] {
                    let value = amount(axis) * factor;
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
            InputEvent::TouchDown { event } => {
                let Some(area) = self.touch_area_for(&event.device(), true) else { return };
                let pos = event.position_transformed(area.size) + area.loc.to_f64();
                self.touch_down_in(event.slot(), pos, area, event.time_msec());
            }
            InputEvent::TouchMotion { event } => {
                let Some(area) = self.touch_area_for(&event.device(), false) else { return };
                let pos = event.position_transformed(area.size) + area.loc.to_f64();
                self.touch_motion(event.slot(), pos, event.time_msec());
            }
            InputEvent::TouchUp { event } => self.touch_up(event.slot(), event.time_msec()),
            InputEvent::TouchFrame { .. } => {
                let touch = self.seat.get_touch().unwrap();
                touch.frame(self);
            }
            InputEvent::TouchCancel { .. } => {
                self.touch_gestures.cancel();
                let touch = self.seat.get_touch().unwrap();
                touch.cancel(self);
            }
            InputEvent::TabletToolProximity { event } => self.tablet_proximity::<I>(&event),
            InputEvent::TabletToolAxis { event } => self.tablet_axis::<I>(&event),
            InputEvent::TabletToolTip { event } => self.tablet_tip::<I>(&event),
            InputEvent::TabletToolButton { event } => self.tablet_button::<I>(&event),
            _ => {}
        }
    }

    /// A key went down or up (from a device, or injected).
    pub(super) fn key_input(&mut self, keycode: smithay::input::keyboard::Keycode, key_state: KeyState, time: u32) {
        let serial = SERIAL_COUNTER.next_serial();
        let pressed = key_state == KeyState::Pressed;
        let action = self.seat.get_keyboard().unwrap().input::<Option<Action>, _>(
            self,
            keycode,
            key_state,
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
                if state.follow.is_some() {
                    // Move/Resize from the window menu: keys drive it.
                    if !pressed {
                        return FilterResult::Forward;
                    }
                    use smithay::input::keyboard::Keysym;
                    let step = if mods.ctrl { 1 } else { 10 };
                    let action = match handle.modified_sym() {
                        Keysym::Escape => Some(Action::CancelFollow),
                        Keysym::Left | Keysym::KP_Left => Some(Action::FollowStep(-step, 0)),
                        Keysym::Right | Keysym::KP_Right => Some(Action::FollowStep(step, 0)),
                        Keysym::Up | Keysym::KP_Up => Some(Action::FollowStep(0, -step)),
                        Keysym::Down | Keysym::KP_Down => Some(Action::FollowStep(0, step)),
                        Keysym::Return | Keysym::KP_Enter | Keysym::space => Some(Action::EndFollow),
                        _ => None,
                    };
                    return FilterResult::Intercept(action);
                }
                if state.shortcuts_inhibited() {
                    // The app gets every key but restore-shortcuts and VT
                    // switching.
                    if pressed && state.is_restore_shortcuts(mods, sym) {
                        return FilterResult::Intercept(Some(Action::RestoreShortcuts));
                    }
                    let decision = state.shortcuts.key(keycode.raw(), pressed, mods, sym);
                    if matches!(decision.action, Some(Action::SwitchVt(_))) {
                        return FilterResult::Intercept(decision.action);
                    }
                    return FilterResult::Forward;
                }
                let decision = state.shortcuts.key(keycode.raw(), pressed, mods, sym);
                if decision.intercept {
                    FilterResult::Intercept(decision.action)
                } else {
                    // E.g. the locate-pointer key: the app gets it too.
                    state.wm.pending_action = decision.action;
                    FilterResult::Forward
                }
            },
        );
        if let Some(Some(action)) = action {
            self.run_shortcut(action, time);
        }
        self.run_pending_action(time);
    }

    /// A pointer button went down or up (from a device, or injected).
    pub(super) fn button_input(&mut self, button: u32, state: ButtonState, time: u32) {
        let pointer = self.seat.get_pointer().unwrap();
        let serial = SERIAL_COUNTER.next_serial();
        if state == ButtonState::Released && button == BTN_LEFT {
            self.frame_release(pointer.current_location());
        }
        if state == ButtonState::Pressed && !pointer.is_grabbed() && !self.lock.locked {
            let pos = pointer.current_location();
            if let Some((window, part)) = self.frame_under(pos) {
                if self.frame_press(window, part, button, time, pos) {
                    return;
                }
                pointer.button(self, &ButtonEvent { button, state, serial, time });
                pointer.frame(self);
                return;
            }
            if self.over_panel(pos) {
                // Clicks on panels don't change window focus.
            } else if let Some(window) = self.space.element_under(pos).map(|(w, _)| w.clone()) {
                // `mouse-button-modifier` clicks move, resize or open
                // the window menu, and are not passed to the app.
                if self.modifier_click(&window, button, pos) {
                    return;
                }
                self.focus_window_with(&window, self.wm_settings.raise_on_click);
            } else {
                self.unfocus_all();
            }
        }
        pointer.button(self, &ButtonEvent { button, state, serial, time });
        pointer.frame(self);
    }

    /// Whether a touch event is part of Helix's gesture (not the apps').
    fn touch_gesture(&mut self, verdict: helix_comp::gestures::Verdict) -> bool {
        use helix_comp::gestures::Verdict;
        match verdict {
            Verdict::Pass => false,
            Verdict::Claim => {
                // The apps' touch points end here.
                let touch = self.seat.get_touch().unwrap();
                touch.cancel(self);
                true
            }
            Verdict::Keep => true,
        }
    }

    /// A touch point went down at `pos` (layout coordinates) on a monitor
    /// of `area`.
    pub(super) fn touch_down_in(&mut self, slot: smithay::backend::input::TouchSlot, pos: Point<f64, Logical>, area: smithay::utils::Rectangle<i32, Logical>, time: u32) {
        if !self.lock.locked {
            let rect = (f64::from(area.loc.x), f64::from(area.loc.y), f64::from(area.size.w), f64::from(area.size.h));
            let verdict = self.touch_gestures.down(i32::from(slot), (pos.x, pos.y), rect);
            if self.touch_gesture(verdict) {
                return;
            }
        }
        self.touch_down(slot, pos, time);
    }

    /// A virtual touchscreen's point (remote desktop) went down at `pos`:
    /// the monitor there is its area.
    pub(super) fn virtual_touch_down(&mut self, slot: smithay::backend::input::TouchSlot, pos: Point<f64, Logical>, time: u32) {
        let area = self.output_at(pos).cloned().and_then(|o| self.space.output_geometry(&o));
        match area {
            Some(area) => self.touch_down_in(slot, pos, area, time),
            None => self.touch_down(slot, pos, time),
        }
    }

    /// A touch point went down at `pos` (layout coordinates).
    pub(super) fn touch_down(&mut self, slot: smithay::backend::input::TouchSlot, pos: Point<f64, Logical>, time: u32) {
        if !self.lock.locked {
            // A touch focuses the window it lands on, as a click does.
            if let Some(window) = self.space.element_under(pos).map(|(w, _)| w.clone()) {
                self.focus_window(&window);
            }
        }
        let under = self.surface_under(pos);
        let touch = self.seat.get_touch().unwrap();
        let down = smithay::input::touch::DownEvent { slot, location: pos, serial: SERIAL_COUNTER.next_serial(), time };
        touch.down(self, under, &down);
    }

    pub(super) fn touch_motion(&mut self, slot: smithay::backend::input::TouchSlot, pos: Point<f64, Logical>, time: u32) {
        let verdict = self.touch_gestures.motion(i32::from(slot), (pos.x, pos.y));
        if self.touch_gesture(verdict) {
            return;
        }
        let under = self.surface_under(pos);
        let touch = self.seat.get_touch().unwrap();
        touch.motion(self, under, &smithay::input::touch::MotionEvent { slot, location: pos, time });
    }

    pub(super) fn touch_up(&mut self, slot: smithay::backend::input::TouchSlot, time: u32) {
        let (verdict, gesture) = self.touch_gestures.up(i32::from(slot));
        if let Some(gesture) = gesture {
            self.apply_gesture(gesture);
        }
        if verdict != helix_comp::gestures::Verdict::Pass {
            return;
        }
        let touch = self.seat.get_touch().unwrap();
        touch.up(self, &smithay::input::touch::UpEvent { slot, serial: SERIAL_COUNTER.next_serial(), time });
    }

    /// The monitor touchscreens cover: the built-in panel, else the primary.
    pub(super) fn touch_area(&self) -> Option<smithay::utils::Rectangle<i32, Logical>> {
        let builtin = self.space.outputs().find(|o| is_builtin_connector(&o.name())).cloned();
        let output = builtin.or_else(|| self.primary())?;
        self.space.output_geometry(&output)
    }
}

/// Connectors of built-in panels.
pub fn is_builtin_connector(name: &str) -> bool {
    ["eDP", "LVDS", "DSI"].iter().any(|p| name.starts_with(p))
}

#[cfg(test)]
mod tests {
    #[test]
    fn builtin_connectors() {
        assert!(super::is_builtin_connector("eDP-1"));
        assert!(super::is_builtin_connector("DSI-1"));
        assert!(!super::is_builtin_connector("HDMI-A-1"));
    }
}
