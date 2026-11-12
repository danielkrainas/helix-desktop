//! Keyboard accessibility in the input path (helix-desktop-spec
//! `contracts/gsettings/a11y-keyboard.md`); the logic is
//! [`helix_comp::a11y_keys`].

use helix_comp::a11y_keys::{self, Out};
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
    }

    /// A key from an input device.
    pub(super) fn device_key(&mut self, keycode: Keycode, state: KeyState, time: u32) {
        let (now, num_lock) = (self.access_now(), self.num_lock());
        let outs = self.access.key(keycode.raw().saturating_sub(8), state == KeyState::Pressed, now, num_lock);
        self.run_access(outs, time);
    }

    /// Slow keys, mouse key motion, the Shift shortcut, the timeout.
    pub fn a11y_tick(&mut self) {
        if !self.access.busy() {
            return;
        }
        let (now, num_lock) = (self.access_now(), self.num_lock());
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
