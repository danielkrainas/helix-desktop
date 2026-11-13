//! Input capture in the compositor (helix-desktop-spec
//! `contracts/dbus/org.helix.InputCapture.xml`): barriers, the capture
//! itself, and the EIS client the captured input goes to.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::rc::Rc;

use helix_comp::input_capture::{self as ic, Barrier, CaptureRequest, CaptureSignal, Zone};
use helix_comp::Event;
use reis::calloop::{EisRequestSource, EisRequestSourceEvent};
use reis::eis;
use reis::request::{Connection, DeviceCapability, EisRequest};
use smithay::input::pointer::CursorImageStatus;
use smithay::reexports::calloop::{PostAction, RegistrationToken};
use smithay::utils::{Logical, Point};

use super::Helix;

/// evdev codes of the modifier keys.
const KEY_LEFTCTRL: u32 = 29;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_RIGHTSHIFT: u32 = 54;
const KEY_LEFTALT: u32 = 56;
const KEY_RIGHTCTRL: u32 = 97;
const KEY_RIGHTALT: u32 = 100;
const KEY_LEFTMETA: u32 = 125;
const KEY_RIGHTMETA: u32 = 126;

/// The modifiers held, from the evdev codes of the keys down.
pub fn held_modifiers(held: &std::collections::HashSet<u32>) -> helix_comp::wm_settings::Modifiers {
    let any = |a: u32, b: u32| held.contains(&a) || held.contains(&b);
    helix_comp::wm_settings::Modifiers {
        alt: any(KEY_LEFTALT, KEY_RIGHTALT),
        logo: any(KEY_LEFTMETA, KEY_RIGHTMETA),
        ctrl: any(KEY_LEFTCTRL, KEY_RIGHTCTRL),
        shift: any(KEY_LEFTSHIFT, KEY_RIGHTSHIFT),
    }
}

#[derive(Default)]
struct CaptureEis {
    connection: Option<Connection>,
    seat: Option<reis::request::Seat>,
    devices: Vec<reis::request::Device>,
    keymaps: Vec<OwnedFd>,
    emulating: bool,
}

struct CaptureSession {
    capabilities: u32,
    barriers: Vec<Barrier>,
    enabled: bool,
    eis: Vec<(RegistrationToken, Rc<RefCell<CaptureEis>>)>,
}

/// The capture going on.
struct Active {
    session: u32,
    activation: u32,
    /// The pointer image before (it is hidden while captured).
    cursor: CursorImageStatus,
    /// Keys held during the capture (evdev codes).
    held: HashSet<u32>,
}

#[derive(Default)]
pub struct InputCaptureState {
    sessions: HashMap<u32, CaptureSession>,
    active: Option<Active>,
    next_activation: u32,
    zone_set: u32,
    zones: Vec<Zone>,
}

impl Helix {
    /// The capture going on: (session, activation).
    pub(super) fn input_capture_active(&self) -> Option<(u32, u32)> {
        self.input_capture.active.as_ref().map(|a| (a.session, a.activation))
    }

    /// The zones: every monitor's logical area.
    fn capture_zones(&self) -> Vec<Zone> {
        let mut zones: Vec<Zone> = self
            .space
            .outputs()
            .filter_map(|o| self.space.output_geometry(o))
            .map(|g| (g.size.w as u32, g.size.h as u32, g.loc.x, g.loc.y))
            .collect();
        zones.sort_by_key(|z| (z.2, z.3));
        zones
    }

    /// Monitors changed: a new zone set, and barriers must be set again.
    pub fn capture_zones_changed(&mut self) {
        let zones = self.capture_zones();
        if zones == self.input_capture.zones {
            return;
        }
        self.input_capture.zones = zones;
        self.input_capture.zone_set = self.input_capture.zone_set.wrapping_add(1);
        for session in self.input_capture.sessions.values_mut() {
            session.barriers.clear();
        }
        if !self.input_capture.sessions.is_empty() {
            self.send(Event::Capture(CaptureSignal::ZonesChanged { zone_set: self.input_capture.zone_set }));
        }
    }

    pub fn capture_request(&mut self, request: CaptureRequest) {
        if self.input_capture.zones.is_empty() {
            self.input_capture.zones = self.capture_zones();
        }
        match request {
            CaptureRequest::Create { session, capabilities } => {
                self.input_capture.sessions.insert(session, CaptureSession { capabilities, barriers: Vec::new(), enabled: false, eis: Vec::new() });
            }
            CaptureRequest::Zones { reply } => {
                let _ = reply.send((self.input_capture.zone_set, self.input_capture.zones.clone()));
            }
            CaptureRequest::Barriers { session, zone_set, barriers, reply } => {
                let current = zone_set == self.input_capture.zone_set;
                let zones = self.input_capture.zones.clone();
                let (good, failed): (Vec<Barrier>, Vec<Barrier>) = barriers.into_iter().partition(|(id, pos)| current && *id != 0 && ic::on_zone_edge(&zones, *pos));
                if let Some(s) = self.input_capture.sessions.get_mut(&session) {
                    tracing::info!(session, set = good.len(), refused = failed.len(), "pointer barriers");
                    s.barriers = good;
                }
                let _ = reply.send(failed.into_iter().map(|(id, _)| id).collect());
            }
            CaptureRequest::Enable { session } => {
                if let Some(s) = self.input_capture.sessions.get_mut(&session) {
                    s.enabled = true;
                }
            }
            CaptureRequest::Disable { session } => {
                if let Some(s) = self.input_capture.sessions.get_mut(&session) {
                    s.enabled = false;
                }
                if self.input_capture.active.as_ref().is_some_and(|a| a.session == session) {
                    self.capture_deactivate(None);
                }
            }
            CaptureRequest::Release { session, activation, position } => {
                if self.input_capture.active.as_ref().is_some_and(|a| a.session == session && a.activation == activation) {
                    self.capture_deactivate(position.map(Point::from));
                }
            }
            CaptureRequest::ConnectEis { session, reply } => {
                let _ = reply.send(self.capture_connect_eis(session));
            }
            CaptureRequest::Close { session } => {
                self.end_remote_share(helix_comp::input_capture::clipboard_id(session));
                if self.input_capture.active.as_ref().is_some_and(|a| a.session == session) {
                    self.capture_deactivate(None);
                }
                if let Some(s) = self.input_capture.sessions.remove(&session) {
                    for (token, _) in s.eis {
                        self.loop_handle.remove(token);
                    }
                }
            }
        }
    }

    fn capture_connect_eis(&mut self, session: u32) -> Result<OwnedFd, String> {
        let capabilities = self.input_capture.sessions.get(&session).ok_or("no such session")?.capabilities;
        let (server, client) = UnixStream::pair().map_err(|e| e.to_string())?;
        let context = eis::Context::new(server).map_err(|e| e.to_string())?;
        let source = EisRequestSource::new(context, 1);
        let state = Rc::new(RefCell::new(CaptureEis::default()));
        let client_state = state.clone();
        let token = self
            .loop_handle
            .insert_source(source, move |event, connection, helix| {
                let action = match event {
                    Ok(event) => helix.capture_eis_event(session, capabilities, &client_state, connection, event),
                    Err(err) => {
                        tracing::warn!(%err, "input capture EIS client error");
                        PostAction::Remove
                    }
                };
                if action == PostAction::Remove {
                    helix.capture_eis_gone(session);
                }
                let _ = connection.flush();
                Ok(action)
            })
            .map_err(|e| e.to_string())?;
        if let Some(s) = self.input_capture.sessions.get_mut(&session) {
            s.eis.push((token, state));
        }
        tracing::info!(session, "input capture EIS client connected");
        Ok(client.into())
    }

    fn capture_eis_event(&mut self, session: u32, capabilities: u32, client: &Rc<RefCell<CaptureEis>>, connection: &Connection, event: EisRequestSourceEvent) -> PostAction {
        let mut offered = enumflags2::BitFlags::<DeviceCapability>::empty();
        if capabilities & 1 != 0 {
            offered |= DeviceCapability::Keyboard;
        }
        if capabilities & 2 != 0 {
            offered |= DeviceCapability::Pointer | DeviceCapability::Button | DeviceCapability::Scroll;
        }
        match event {
            EisRequestSourceEvent::Connected => {
                if connection.context_type() != eis::handshake::ContextType::Receiver {
                    tracing::warn!("input capture EIS client is not a receiver");
                    return PostAction::Remove;
                }
                let mut client = client.borrow_mut();
                client.seat = Some(connection.add_seat(Some("default"), offered));
                client.connection = Some(connection.clone());
            }
            EisRequestSourceEvent::Request(EisRequest::Disconnect) => return PostAction::Remove,
            EisRequestSourceEvent::Request(EisRequest::Bind(bind)) => {
                let wanted = bind.capabilities & offered;
                let mut client = client.borrow_mut();
                if wanted.contains(DeviceCapability::Keyboard) {
                    let keymap = self.keymap_text().as_deref().and_then(super::remote::memfd_keymap);
                    client.devices.push(super::remote::add_keyboard(&bind.seat, keymap.as_ref()));
                    if let Some(fd) = keymap.and_then(|(fd, _)| fd.try_clone().ok()) {
                        client.keymaps.push(fd);
                    }
                }
                let pointer = wanted & (DeviceCapability::Pointer | DeviceCapability::Button | DeviceCapability::Scroll);
                if !pointer.is_empty() {
                    let device = bind.seat.add_device(Some("Helix captured pointer"), eis::device::DeviceType::Virtual, pointer, |_| {});
                    device.resumed();
                    client.devices.push(device);
                }
                // Bound during a capture: emulating at once.
                if let Some(active) = self.input_capture.active.as_ref().filter(|a| a.session == session) {
                    for device in &client.devices {
                        device.start_emulating(active.activation);
                    }
                    client.emulating = true;
                }
            }
            EisRequestSourceEvent::Request(_) => {}
        }
        PostAction::Continue
    }

    /// An EIS client left: a capture for nobody ends.
    fn capture_eis_gone(&mut self, session: u32) {
        if self.input_capture.active.as_ref().is_some_and(|a| a.session == session) {
            tracing::info!(session, "input capture client gone; releasing");
            self.capture_deactivate(None);
        }
    }

    fn capture_clients(&self, session: u32) -> Vec<Rc<RefCell<CaptureEis>>> {
        self.input_capture.sessions.get(&session).map(|s| s.eis.iter().map(|(_, c)| c.clone()).collect()).unwrap_or_default()
    }

    /// A pointer move from `from` to `to` (before keeping it on the
    /// monitors): whether it crossed an enabled session's barrier, which
    /// starts a capture.
    pub(super) fn capture_crossing(&mut self, from: Point<f64, Logical>, to: Point<f64, Logical>, at: Point<f64, Logical>) -> bool {
        if self.input_capture.active.is_some() {
            return false;
        }
        let hit = self
            .input_capture
            .sessions
            .iter()
            .filter(|(_, s)| s.enabled && !s.eis.is_empty())
            .find_map(|(id, s)| ic::crossed(&s.barriers, (from.x, from.y), (to.x, to.y)).map(|b| (*id, b)));
        let Some((session, barrier)) = hit else { return false };
        self.input_capture.next_activation = self.input_capture.next_activation.wrapping_add(1);
        let activation = self.input_capture.next_activation;
        let cursor = std::mem::replace(&mut self.cursor_status, CursorImageStatus::Hidden);
        self.input_capture.active = Some(Active { session, activation, cursor, held: HashSet::new() });
        for client in self.capture_clients(session) {
            let mut client = client.borrow_mut();
            for device in &client.devices {
                device.start_emulating(activation);
            }
            client.emulating = true;
            if let Some(c) = &client.connection {
                let _ = c.flush();
            }
        }
        tracing::info!(session, barrier, activation, "input captured");
        self.send(Event::Capture(CaptureSignal::Activated { session, activation, barrier, position: (at.x, at.y) }));
        true
    }

    /// End the capture; the pointer goes to `position` (kept on the
    /// monitors) if given.
    pub(super) fn capture_deactivate(&mut self, position: Option<Point<f64, Logical>>) {
        let Some(active) = self.input_capture.active.take() else { return };
        for client in self.capture_clients(active.session) {
            let mut client = client.borrow_mut();
            if client.emulating {
                for device in &client.devices {
                    device.stop_emulating();
                }
                client.emulating = false;
            }
            if let Some(c) = &client.connection {
                let _ = c.flush();
            }
        }
        self.cursor_status = active.cursor;
        let time = self.start_time.elapsed().as_millis() as u32;
        if let Some(pos) = position {
            let pos = self.clamp_to_outputs(pos);
            self.pointer_to(pos, time);
        }
        let at = self.seat.get_pointer().map(|p| p.current_location()).unwrap_or_default();
        tracing::info!(session = active.session, activation = active.activation, "input released");
        self.send(Event::Capture(CaptureSignal::Deactivated { session: active.session, activation: active.activation, position: (at.x, at.y) }));
    }

    /// Send to the capturing session's devices with `capability`.
    fn capture_send(&self, capability: DeviceCapability, f: impl Fn(&reis::request::Device)) {
        let Some(active) = &self.input_capture.active else { return };
        let time = self.start_time.elapsed().as_micros() as u64;
        for client in self.capture_clients(active.session) {
            let client = client.borrow();
            for device in client.devices.iter().filter(|d| d.has_capability(capability)) {
                f(device);
                device.frame(time);
            }
            if let Some(c) = &client.connection {
                let _ = c.flush();
            }
        }
    }

    /// Pointer motion while captured.
    pub(super) fn capture_motion(&mut self, delta: Point<f64, Logical>) -> bool {
        if self.input_capture.active.is_none() {
            return false;
        }
        self.capture_send(DeviceCapability::Pointer, |d| {
            if let Some(p) = d.interface::<eis::Pointer>() {
                p.motion_relative(delta.x as f32, delta.y as f32);
            }
        });
        true
    }

    /// A button while captured.
    pub(super) fn capture_button(&mut self, button: u32, pressed: bool) -> bool {
        if self.input_capture.active.is_none() {
            return false;
        }
        let state = if pressed { eis::button::ButtonState::Press } else { eis::button::ButtonState::Released };
        self.capture_send(DeviceCapability::Button, |d| {
            if let Some(b) = d.interface::<eis::Button>() {
                b.button(button, state);
            }
        });
        true
    }

    /// Scrolling while captured (`discrete`: in 120ths of a wheel step).
    pub(super) fn capture_scroll(&mut self, dx: f64, dy: f64, discrete: Option<(i32, i32)>) -> bool {
        if self.input_capture.active.is_none() {
            return false;
        }
        self.capture_send(DeviceCapability::Scroll, |d| {
            if let Some(s) = d.interface::<eis::Scroll>() {
                match discrete {
                    Some((x, y)) => s.scroll_discrete(x, y),
                    None => s.scroll(dx as f32, dy as f32),
                }
            }
        });
        true
    }

    /// Is `key` (evdev code), with `mods` held, `cancel-input-capture`?
    fn is_cancel_capture(&mut self, key: u32, mods: helix_comp::wm_settings::Modifiers) -> bool {
        let Some(keyboard) = self.seat.get_keyboard() else { return false };
        // The key's symbol without modifiers, in the active layout.
        let sym = keyboard.with_xkb_state(self, |context| {
            let xkb = context.xkb().lock().ok()?;
            let layout = xkb.active_layout();
            // SAFETY: only read from the keymap while the lock is held.
            let keymap = unsafe { xkb.keymap() };
            keymap.key_get_syms_by_level(smithay::input::keyboard::Keycode::new(key + 8), layout.0, 0).first().copied()
        });
        self.wm_settings.cancel_input_capture.iter().any(|(m, name)| {
            *m == mods
                && match super::keybindings::key_from_name(name) {
                    Some(super::shortcuts::Key::Sym(s)) => Some(s) == sym,
                    Some(super::shortcuts::Key::Code(c)) => c == key + 8,
                    None => false,
                }
        })
    }

    /// A key (evdev code) while captured. `cancel-input-capture`
    /// (Super+Shift+Escape) ends the capture (the session is disabled),
    /// so a client cannot keep the input.
    pub(super) fn capture_key(&mut self, key: u32, pressed: bool) -> bool {
        let Some(active) = self.input_capture.active.as_mut() else { return false };
        if pressed {
            active.held.insert(key);
        } else {
            active.held.remove(&key);
        }
        let mods = held_modifiers(&active.held);
        if pressed && self.is_cancel_capture(key, mods) {
            let Some(active) = self.input_capture.active.as_mut() else { return false };
            let session = active.session;
            // The client saw the modifiers go down: they go up again.
            let others: Vec<u32> = active.held.iter().copied().filter(|k| *k != key).collect();
            for k in others {
                self.capture_send(DeviceCapability::Keyboard, |d| {
                    if let Some(kb) = d.interface::<eis::Keyboard>() {
                        kb.key(k, eis::keyboard::KeyState::Released);
                    }
                });
            }
            tracing::info!(session, "input capture ended with cancel-input-capture");
            self.capture_deactivate(None);
            if let Some(s) = self.input_capture.sessions.get_mut(&session) {
                s.enabled = false;
            }
            self.send(Event::Capture(CaptureSignal::Disabled { session }));
            return true;
        }
        let state = if pressed { eis::keyboard::KeyState::Press } else { eis::keyboard::KeyState::Released };
        self.capture_send(DeviceCapability::Keyboard, |d| {
            if let Some(kb) = d.interface::<eis::Keyboard>() {
                kb.key(key, state);
            }
        });
        true
    }
}
