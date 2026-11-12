//! Keyboard accessibility in the input path (helix-desktop-spec
//! `contracts/gsettings/a11y-keyboard.md`); the logic is
//! [`helix_comp::a11y_keys`].

use helix_comp::a11y_keys::{self, Out};
use helix_comp::a11y_mouse;

/// The shell's click type window.
const CLICK_TYPE_NAMESPACE: &str = "helix-click-type";
use smithay::backend::input::{ButtonState, KeyState};
use smithay::input::keyboard::Keycode;

use super::Helix;

impl Helix {
    fn access_now(&self) -> u64 {
        self.start_time.elapsed().as_millis() as u64
    }

    fn num_lock(&self) -> bool {
        self.seat.get_keyboard().is_some_and(|k| k.modifier_state().num_lock)
    }

    pub fn apply_a11y_settings(&mut self) {
        let settings = a11y_keys::read();
        if &settings != self.access.settings() {
            tracing::info!(?settings, "keyboard accessibility settings");
            self.access.set_settings(settings);
        }
        let mouse = a11y_mouse::read();
        if &mouse != self.mouse_access.settings() {
            tracing::info!(?mouse, "mouse accessibility settings");
            let outs = self.mouse_access.set_settings(mouse);
            let now = self.access_now();
            self.run_mouse_access(outs, now as u32);
        }
    }

    /// A pointer button from an input device.
    pub(super) fn device_button(&mut self, button: u32, state: ButtonState, time: u32) {
        if self.capture_button(button, state == ButtonState::Pressed) {
            return;
        }
        let pos = self.seat.get_pointer().map(|p| p.current_location()).unwrap_or_default();
        let outs = self.mouse_access.button(button, state == ButtonState::Pressed, self.access_now(), (pos.x, pos.y));
        self.run_mouse_access(outs, time);
    }

    /// The pointer moved (from an input device).
    pub(super) fn device_moved(&mut self, time: u32) {
        let pos = self.seat.get_pointer().map(|p| p.current_location()).unwrap_or_default();
        let outs = self.mouse_access.moved((pos.x, pos.y), self.access_now());
        self.run_mouse_access(outs, time);
    }

    fn run_mouse_access(&mut self, outs: Vec<a11y_mouse::Out>, time: u32) {
        for out in outs {
            match out {
                a11y_mouse::Out::Button(button, pressed) => {
                    self.button_input(button, if pressed { ButtonState::Pressed } else { ButtonState::Released }, time);
                }
                a11y_mouse::Out::Warp(x, y) => self.pointer_to((x, y).into(), time),
            }
        }
    }

    /// A key from an input device.
    pub(super) fn device_key(&mut self, keycode: Keycode, state: KeyState, time: u32) {
        if self.capture_key(keycode.raw().saturating_sub(8), state == KeyState::Pressed) {
            return;
        }
        let (now, num_lock) = (self.access_now(), self.num_lock());
        let outs = self.access.key(keycode.raw().saturating_sub(8), state == KeyState::Pressed, now, num_lock);
        self.run_access(outs, time);
    }

    /// The namespace of the shell surface under the pointer, if any.
    fn layer_namespace_under_pointer(&self) -> Option<String> {
        let pos = self.seat.get_pointer()?.current_location();
        let output = self.space.outputs().find(|o| self.space.output_geometry(o).is_some_and(|g| g.to_f64().contains(pos)))?;
        let origin = self.space.output_geometry(output)?.loc.to_f64();
        let map = smithay::desktop::layer_map_for_output(output);
        [smithay::wayland::shell::wlr_layer::Layer::Overlay, smithay::wayland::shell::wlr_layer::Layer::Top]
            .into_iter()
            .find_map(|layer| map.layer_under(layer, pos - origin).map(|l| l.namespace().to_owned()))
    }

    /// Slow keys, mouse key motion, the Shift shortcut, the timeout.
    pub fn a11y_tick(&mut self) {
        let now = self.access_now();
        if self.mouse_access.busy() {
            // Dwells on the click type window are plain clicks.
            let on_chooser = self.layer_namespace_under_pointer().as_deref() == Some(CLICK_TYPE_NAMESPACE);
            self.mouse_access.set_on_chooser(on_chooser);
            let before = self.mouse_access.click_type();
            let outs = self.mouse_access.tick(now);
            let after = self.mouse_access.click_type();
            if after != before {
                let _ = self.events.send(helix_comp::Event::ClickType(after));
            }
            self.run_mouse_access(outs, now as u32);
        }
        if !self.access.busy() {
            return;
        }
        let num_lock = self.num_lock();
        let outs = self.access.tick(now, num_lock);
        self.run_access(outs, now as u32);
    }

    fn run_access(&mut self, outs: Vec<Out>, time: u32) {
        for out in outs {
            match out {
                Out::Key(code, pressed) => {
                    let state = if pressed { KeyState::Pressed } else { KeyState::Released };
                    self.key_input(Keycode::new(code + 8), state, time);
                }
                Out::Beep => helix_settings::sound::play(&helix_settings::sound::Sound::Name("bell".into())),
                Out::Move(dx, dy) => {
                    let delta = (dx, dy).into();
                    self.pointer_moved_by(delta, delta, u64::from(time) * 1000, time);
                }
                Out::Button(button, pressed) => {
                    self.button_input(button, if pressed { ButtonState::Pressed } else { ButtonState::Released }, time);
                }
                Out::Set(key, value) => {
                    tracing::info!(key, value, "keyboard accessibility changed from the keyboard");
                    a11y_keys::store(key, value);
                }
            }
        }
    }
}
