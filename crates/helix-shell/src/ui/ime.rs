//! The session's input method (helix-desktop-spec
//! `contracts/gsettings/input-settings.md`, "Input methods"): while an
//! `ibus` source is active and a text field has focus, keys go to IBus
//! (`crate::ibus`), its text goes to the app over `zwp_input_method_v2`,
//! keys it does not use go on through `zwp_virtual_keyboard_v1`, and its
//! candidates are drawn in an input popup.

use std::collections::VecDeque;
use std::os::fd::{AsFd, OwnedFd};

use smithay_client_toolkit::reexports::client::protocol::wl_seat::WlSeat;
use smithay_client_toolkit::reexports::client::Proxy;
use wayland_protocols_misc::zwp_input_method_v2::client::{
    zwp_input_method_keyboard_grab_v2::{self, ZwpInputMethodKeyboardGrabV2},
    zwp_input_method_manager_v2::ZwpInputMethodManagerV2,
    zwp_input_method_v2::{self, ZwpInputMethodV2},
    zwp_input_popup_surface_v2::{self, ZwpInputPopupSurfaceV2},
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1, zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use xkbcommon::xkb;

use super::*;
use crate::ibus::{ImEvent, ImRequest, RELEASE_MASK};

const POPUP_ROW_H: i32 = 30;

/// Input method state.
pub(super) struct Ime {
    im: ZwpInputMethodV2,
    vk: ZwpVirtualKeyboardV1,
    grab: Option<ZwpInputMethodKeyboardGrabV2>,
    /// What the text field said before the last `done`.
    pending_active: bool,
    active: bool,
    /// Number of `done` events (the serial for `commit`).
    serial: u32,
    /// The active `ibus` source's engine.
    engine: Option<String>,
    context: xkb::Context,
    xkb: Option<xkb::State>,
    /// Keys waiting for IBus's answer: (seq, time, evdev key, pressed).
    waiting: VecDeque<(u64, u32, u32, bool)>,
    seq: u64,
    popup: Option<(wl_surface::WlSurface, ZwpInputPopupSurfaceV2)>,
    candidates: Option<(Vec<String>, usize)>,
    /// The virtual keyboard has a keymap.
    vk_keymap: bool,
}

/// The IBus engine of the active source, if it is an `ibus` one.
fn active_engine() -> Option<String> {
    crate::input_sources::active().filter(|(kind, _)| kind == "ibus").map(|(_, id)| id)
}

/// The X modifier mask IBus expects.
fn ibus_mask(state: &xkb::State) -> u32 {
    let on = |name: &str| state.mod_name_is_active(name, xkb::STATE_MODS_EFFECTIVE);
    let mut mask = 0;
    for (name, bit) in [(xkb::MOD_NAME_SHIFT, 1), (xkb::MOD_NAME_CAPS, 2), (xkb::MOD_NAME_CTRL, 4), (xkb::MOD_NAME_ALT, 8), (xkb::MOD_NAME_LOGO, 64)] {
        if on(name) {
            mask |= bit;
        }
    }
    mask
}

/// Byte offset of character `chars` in `text`.
pub fn byte_offset(text: &str, chars: u32) -> u32 {
    text.char_indices().nth(chars as usize).map_or(text.len(), |(i, _)| i) as u32
}

fn now_ms() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u32)
        .unwrap_or(0)
}

impl Ui {
    /// Become the input method on `seat` (if the compositor lets us).
    pub(super) fn start_ime(&mut self, seat: &WlSeat) {
        if self.ime.is_some() {
            return;
        }
        let (Some(im_manager), Some(vk_manager)) = (&self.im_manager, &self.vk_manager) else { return };
        let im = im_manager.get_input_method(seat, &self.qh, ());
        let vk = vk_manager.create_virtual_keyboard(seat, &self.qh, ());
        tracing::info!("input method ready");
        self.ime = Some(Ime {
            im,
            vk,
            grab: None,
            pending_active: false,
            active: false,
            serial: 0,
            engine: None,
            context: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
            xkb: None,
            waiting: VecDeque::new(),
            seq: 0,
            popup: None,
            candidates: None,
            vk_keymap: false,
        });
    }

    /// Follow the active input source: tell IBus the engine, and take or
    /// release the keyboard.
    pub(super) fn ime_sync(&mut self) {
        let engine = active_engine();
        let Some(ime) = self.ime.as_mut() else { return };
        if ime.engine != engine {
            ime.engine = engine.clone();
            let _ = self.requests.send(UiRequest::Ime(ImRequest::Engine(engine.clone())));
            if engine.is_some() && ime.active {
                let _ = self.requests.send(UiRequest::Ime(ImRequest::FocusIn));
            }
        }
        let want_grab = ime.active && ime.engine.is_some();
        match (want_grab, ime.grab.is_some()) {
            (true, false) => ime.grab = Some(ime.im.grab_keyboard(&self.qh, ())),
            (false, true) => {
                if let Some(grab) = ime.grab.take() {
                    grab.release();
                }
                ime.xkb = None;
                // Keys still waiting go through.
                for (_, time, key, pressed) in ime.waiting.drain(..) {
                    ime.vk.key(time, key, u32::from(pressed));
                }
            }
            _ => {}
        }
    }

    /// A text field has focus (the input method is active).
    pub(super) fn ime_typing(&self) -> bool {
        self.ime.as_ref().is_some_and(|i| i.active)
    }

    /// Type `text` into the focused text field.
    pub(super) fn ime_commit_text(&mut self, text: &str) {
        let Some(ime) = self.ime.as_mut().filter(|i| i.active) else { return };
        ime.im.commit_string(text.to_owned());
        ime.im.commit(ime.serial);
    }

    /// Press and release evdev key `code` on the virtual keyboard (giving
    /// it the `us` keymap first if it has none).
    pub(super) fn ime_tap_key(&mut self, code: u32) {
        let Some(ime) = self.ime.as_mut() else { return };
        if !ime.vk_keymap {
            let keymap = xkb::Keymap::new_from_names(&ime.context, "evdev", "pc105", "us", "", None, xkb::KEYMAP_COMPILE_NO_FLAGS);
            let Some(text) = keymap.map(|k| k.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)) else { return };
            let Some(mut file) = crate::screenshot::memfd() else { return };
            let mut bytes = text.into_bytes();
            bytes.push(0);
            if std::io::Write::write_all(&mut file, &bytes).is_err() {
                return;
            }
            ime.vk.keymap(1, file.as_fd(), bytes.len() as u32);
            ime.vk_keymap = true;
        }
        let time = now_ms();
        ime.vk.key(time, code, 1);
        ime.vk.key(time, code, 0);
    }

    pub(super) fn ime_event(&mut self, event: ImEvent) {
        let Some(ime) = self.ime.as_mut() else { return };
        match event {
            ImEvent::KeyHandled { seq, handled } => {
                while let Some(&(s, time, key, pressed)) = ime.waiting.front() {
                    if s > seq {
                        break;
                    }
                    ime.waiting.pop_front();
                    if s == seq && !handled {
                        ime.vk.key(time, key, u32::from(pressed));
                    }
                }
            }
            ImEvent::Forward { keycode, pressed } => ime.vk.key(now_ms(), keycode, u32::from(pressed)),
            ImEvent::Commit(text) if ime.active => {
                ime.im.commit_string(text);
                ime.im.commit(ime.serial);
            }
            ImEvent::Preedit { text, cursor } if ime.active => {
                let at = byte_offset(&text, cursor) as i32;
                ime.im.set_preedit_string(text, at, at);
                ime.im.commit(ime.serial);
            }
            ImEvent::Candidates(candidates) => {
                ime.candidates = candidates;
                self.draw_candidates();
            }
            _ => {}
        }
    }

    fn draw_candidates(&mut self) {
        let Some(ime) = self.ime.as_mut() else { return };
        let Some(candidates) = ime.candidates.clone().filter(|(c, _)| !c.is_empty()) else {
            if let Some((surface, _)) = &ime.popup {
                surface.attach(None, 0, 0);
                surface.commit();
            }
            return;
        };
        if ime.popup.is_none() {
            let surface = self.compositor.create_surface(&self.qh);
            let popup = ime.im.get_input_popup_surface(&surface, &self.qh, ());
            ime.popup = Some((surface, popup));
        }
        let (list, selected) = candidates;
        let text = self.text.as_ref();
        let labels: Vec<String> = list.iter().enumerate().map(|(i, c)| format!("{} {c}", i + 1)).collect();
        let widths: Vec<i32> = labels.iter().map(|l| text.map_or(l.chars().count() as f32 * 9.0, |t| t.width(l, 15.0)) as i32 + 16).collect();
        let w = widths.iter().sum::<i32>().max(40) + 8;
        let h = POPUP_ROW_H + 8;
        let scale = self.ime.as_ref().and_then(|i| i.popup.as_ref()).map_or(1.0, |(surface, _)| self.scales.of(surface));
        let (pw, ph) = (physical(w as usize, scale) as i32, physical(h as usize, scale) as i32);
        let Ok((buffer, data)) = self.pool.create_buffer(pw, ph, pw * 4, wl_shm::Format::Argb8888) else { return };
        let mut canvas = Canvas::scaled(data, w as usize, h as usize, scale);
        canvas.fill(BANNER_BORDER);
        canvas.rect(1, 1, w - 2, h - 2, BANNER_BG);
        let mut x = 4;
        for (i, (label_text, width)) in labels.iter().zip(&widths).enumerate() {
            if i == selected {
                canvas.rect(x, 4, *width, POPUP_ROW_H, ACCENT);
            }
            let base = baseline(text, 4, POPUP_ROW_H, 15.0);
            label(text, &mut canvas, label_text, (x + 8) as f32, base, 15.0, FG, *width as f32);
            x += width;
        }
        let Some(ime) = self.ime.as_ref() else { return };
        let Some((surface, _)) = &ime.popup else { return };
        self.scales.prepare(&self.qh, surface, w, h);
        if buffer.attach_to(surface).is_ok() {
            surface.commit();
        }
    }
}

impl Dispatch<ZwpInputMethodManagerV2, ()> for Ui {
    fn event(_: &mut Self, _: &ZwpInputMethodManagerV2, _: <ZwpInputMethodManagerV2 as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for Ui {
    fn event(_: &mut Self, _: &ZwpVirtualKeyboardManagerV1, _: <ZwpVirtualKeyboardManagerV1 as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwpVirtualKeyboardV1, ()> for Ui {
    fn event(_: &mut Self, _: &ZwpVirtualKeyboardV1, _: <ZwpVirtualKeyboardV1 as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwpInputPopupSurfaceV2, ()> for Ui {
    fn event(
        ui: &mut Self,
        _: &ZwpInputPopupSurfaceV2,
        event: zwp_input_popup_surface_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_input_popup_surface_v2::Event::TextInputRectangle { x, y, width, height } = event {
            let _ = ui.requests.send(UiRequest::Ime(ImRequest::Cursor { x, y, width, height }));
        }
    }
}

impl Dispatch<ZwpInputMethodV2, ()> for Ui {
    fn event(ui: &mut Self, _: &ZwpInputMethodV2, event: zwp_input_method_v2::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let Some(ime) = ui.ime.as_mut() else { return };
        match event {
            zwp_input_method_v2::Event::Activate => ime.pending_active = true,
            zwp_input_method_v2::Event::Deactivate => ime.pending_active = false,
            zwp_input_method_v2::Event::Done => {
                ime.serial = ime.serial.wrapping_add(1);
                let was = ime.active;
                ime.active = ime.pending_active;
                if ime.active != was && ime.engine.is_some() {
                    let request = if ime.active { ImRequest::FocusIn } else { ImRequest::FocusOut };
                    let _ = ui.requests.send(UiRequest::Ime(request));
                }
                if !ime.active {
                    ime.candidates = None;
                }
                ui.ime_sync();
                ui.draw_candidates();
                ui.osk_sync();
            }
            zwp_input_method_v2::Event::Unavailable => {
                tracing::warn!("another input method is running; IBus input is off");
                ui.ime = None;
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpInputMethodKeyboardGrabV2, ()> for Ui {
    fn event(
        ui: &mut Self,
        _: &ZwpInputMethodKeyboardGrabV2,
        event: zwp_input_method_keyboard_grab_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(ime) = ui.ime.as_mut() else { return };
        match event {
            zwp_input_method_keyboard_grab_v2::Event::Keymap { format, fd, size } => {
                // The app gets the same keymap from the virtual keyboard.
                ime.vk.keymap(u32::from(format), fd.as_fd(), size);
                ime.vk_keymap = true;
                let fd: OwnedFd = fd;
                // SAFETY: the compositor sent a keymap of `size` bytes.
                let keymap = unsafe {
                    xkb::Keymap::new_from_fd(&ime.context, fd, size as usize, xkb::KEYMAP_FORMAT_TEXT_V1, xkb::KEYMAP_COMPILE_NO_FLAGS)
                };
                ime.xkb = keymap.ok().flatten().map(|k| xkb::State::new(&k));
            }
            zwp_input_method_keyboard_grab_v2::Event::Modifiers { mods_depressed, mods_latched, mods_locked, group, .. } => {
                if let Some(state) = ime.xkb.as_mut() {
                    state.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                }
                ime.vk.modifiers(mods_depressed, mods_latched, mods_locked, group);
            }
            zwp_input_method_keyboard_grab_v2::Event::Key { time, key, state, .. } => {
                let pressed = matches!(state, WEnum::Value(smithay_client_toolkit::reexports::client::protocol::wl_keyboard::KeyState::Pressed));
                let Some(xkb_state) = ime.xkb.as_ref() else {
                    ime.vk.key(time, key, u32::from(pressed));
                    return;
                };
                let keyval = xkb_state.key_get_one_sym(xkb::Keycode::new(key + 8)).raw();
                let mut mask = ibus_mask(xkb_state);
                if !pressed {
                    mask |= RELEASE_MASK;
                }
                ime.seq += 1;
                ime.waiting.push_back((ime.seq, time, key, pressed));
                let request = ImRequest::Key { seq: ime.seq, keyval, keycode: key, state: mask };
                let _ = ui.requests.send(UiRequest::Ime(request));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn preedit_cursor_bytes() {
        assert_eq!(super::byte_offset("你好", 1), 3);
        assert_eq!(super::byte_offset("ab", 5), 2, "past the end");
        assert_eq!(super::byte_offset("", 0), 0);
    }
}
