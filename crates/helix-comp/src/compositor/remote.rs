//! Remote desktop input (helix-desktop-spec
//! `org.gnome.Mutter.RemoteDesktop.md`): input injected over D-Bus or by
//! EIS clients, applied as if from a device.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;

use helix_comp::remote_desktop::RemoteInput;
use reis::calloop::{EisRequestSource, EisRequestSourceEvent};
use reis::eis;
use reis::request::{Connection, DeviceCapability, EisRequest};
use smithay::backend::input::{Axis, AxisSource, ButtonState, KeyState, TouchSlot};
use smithay::input::keyboard::{xkb, Keycode};
use smithay::input::pointer::AxisFrame;
use smithay::reexports::calloop::RegistrationToken;
use smithay::utils::{Logical, Point};

use super::Helix;

/// evdev code of the left Shift key.
const KEY_LEFTSHIFT: u32 = 42;
/// Pixels per discrete scroll step.
const STEP_PX: f64 = 15.0;

/// EIS connections, by remote desktop session.
#[derive(Default)]
pub struct RemoteState {
    eis: HashMap<u32, Vec<EisEntry>>,
}

/// An EIS client's event source and state.
type EisEntry = (RegistrationToken, Rc<RefCell<EisClient>>);

/// One EIS client's devices.
#[derive(Default)]
struct EisClient {
    connection: Option<Connection>,
    seat: Option<reis::request::Seat>,
    devices: Vec<reis::request::Device>,
    /// Keymaps sent to the client (kept open until it has them).
    keymaps: Vec<OwnedFd>,
}

/// Add a keyboard device with `keymap` (a memfd and its size) to `seat`.
fn add_keyboard(seat: &reis::request::Seat, keymap: Option<&(OwnedFd, u32)>) -> reis::request::Device {
    let raw = keymap.map(|(fd, size)| (std::os::fd::AsRawFd::as_raw_fd(fd), *size));
    let device = seat.add_device(Some("Helix remote keyboard"), eis::device::DeviceType::Virtual, DeviceCapability::Keyboard.into(), move |device| {
        if let (Some(keyboard), Some((fd, size))) = (device.interface::<eis::Keyboard>(), raw) {
            // SAFETY: the memfd stays open in `EisClient::keymaps`.
            let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
            keyboard.keymap(eis::keyboard::KeymapType::Xkb, size, fd);
        }
    });
    device.resumed();
    device
}

fn slot(id: u32) -> TouchSlot {
    TouchSlot::from(Some(id))
}

impl Helix {
    fn now_ms(&self) -> u32 {
        self.start_time.elapsed().as_millis() as u32
    }

    /// Apply input from a remote desktop session.
    pub fn remote_input(&mut self, input: RemoteInput) {
        self.note_activity();
        let time = self.now_ms();
        match input {
            RemoteInput::Key { keycode, pressed } => {
                let state = if pressed { KeyState::Pressed } else { KeyState::Released };
                self.key_input(Keycode::new(keycode + 8), state, time);
            }
            RemoteInput::Keysym { keysym, pressed } => {
                let Some((keycode, shift)) = self.key_for_keysym(keysym) else {
                    tracing::debug!(keysym, "no key for keysym");
                    return;
                };
                let state = if pressed { KeyState::Pressed } else { KeyState::Released };
                if shift && pressed {
                    self.key_input(Keycode::new(KEY_LEFTSHIFT + 8), KeyState::Pressed, time);
                }
                self.key_input(keycode, state, time);
                if shift && !pressed {
                    self.key_input(Keycode::new(KEY_LEFTSHIFT + 8), KeyState::Released, time);
                }
            }
            RemoteInput::Button { button, pressed } => {
                let state = if pressed { ButtonState::Pressed } else { ButtonState::Released };
                self.button_input(button, state, time);
            }
            RemoteInput::Motion { dx, dy } => {
                let delta = Point::from((dx, dy));
                self.pointer_moved_by(delta, delta, u64::from(time) * 1000, time);
            }
            RemoteInput::MotionAbsolute { stream, x, y } => {
                if let Some(pos) = self.stream_point(stream, x, y) {
                    let pos = self.clamp_to_outputs(pos);
                    self.pointer_to(pos, time);
                    self.activate_constraint_under();
                }
            }
            RemoteInput::Axis { dx, dy, finish, source } => {
                let source = match source {
                    1 => AxisSource::Wheel,
                    2 => AxisSource::Continuous,
                    _ => AxisSource::Finger,
                };
                let mut frame = AxisFrame::new(time).source(source);
                for (axis, value) in [(Axis::Horizontal, dx), (Axis::Vertical, dy)] {
                    if value != 0.0 {
                        frame = frame.value(axis, value);
                    }
                    if finish {
                        frame = frame.stop(axis);
                    }
                }
                let pointer = self.seat.get_pointer().unwrap();
                pointer.axis(self, frame);
                pointer.frame(self);
            }
            RemoteInput::AxisDiscrete { axis, steps } => {
                let axis = if axis == 1 { Axis::Horizontal } else { Axis::Vertical };
                let frame = AxisFrame::new(time)
                    .source(AxisSource::Wheel)
                    .value(axis, f64::from(steps) * STEP_PX)
                    .v120(axis, steps * 120);
                let pointer = self.seat.get_pointer().unwrap();
                pointer.axis(self, frame);
                pointer.frame(self);
            }
            RemoteInput::TouchDown { stream, slot: id, x, y } => {
                if let Some(pos) = self.stream_point(stream, x, y) {
                    self.touch_down(slot(id), self.clamp_to_outputs(pos), time);
                    self.seat.get_touch().unwrap().frame(self);
                }
            }
            RemoteInput::TouchMotion { stream, slot: id, x, y } => {
                if let Some(pos) = self.stream_point(stream, x, y) {
                    self.touch_motion(slot(id), self.clamp_to_outputs(pos), time);
                    self.seat.get_touch().unwrap().frame(self);
                }
            }
            RemoteInput::TouchUp { slot: id } => {
                self.touch_up(slot(id), time);
                self.seat.get_touch().unwrap().frame(self);
            }
        }
    }

    /// The key (and whether Shift is needed) that gives `keysym` in the
    /// active layout, at its first or second level.
    fn key_for_keysym(&mut self, keysym: u32) -> Option<(Keycode, bool)> {
        let keyboard = self.seat.get_keyboard()?;
        keyboard.with_xkb_state(self, |context| {
            let xkb = context.xkb().lock().ok()?;
            let layout = xkb.active_layout();
            // SAFETY: only read from the keymap while the lock is held.
            let keymap = unsafe { xkb.keymap() };
            let wanted = xkb::Keysym::new(keysym);
            let (min, max) = (keymap.min_keycode().raw(), keymap.max_keycode().raw());
            for level in 0..2u32 {
                for code in min..=max {
                    let keycode = Keycode::new(code);
                    if keymap.key_get_syms_by_level(keycode, layout.0, level).contains(&wanted) {
                        return Some((keycode, level == 1));
                    }
                }
            }
            None
        })
    }

    /// Give a remote desktop session's EIS client a socket.
    pub fn connect_eis(&mut self, session: u32, devices: u32) -> Result<OwnedFd, String> {
        let (server, client) = UnixStream::pair().map_err(|e| e.to_string())?;
        let context = eis::Context::new(server).map_err(|e| e.to_string())?;
        let source = EisRequestSource::new(context, 1);
        let state = Rc::new(RefCell::new(EisClient::default()));
        let client_state = state.clone();
        let token = self
            .loop_handle
            .insert_source(source, move |event, connection, helix| {
                let action = match event {
                    Ok(event) => helix.eis_event(&mut client_state.borrow_mut(), connection, event, devices),
                    Err(err) => {
                        tracing::warn!(%err, "EIS client error");
                        smithay::reexports::calloop::PostAction::Remove
                    }
                };
                let _ = connection.flush();
                Ok(action)
            })
            .map_err(|e| e.to_string())?;
        self.remote.eis.entry(session).or_default().push((token, state));
        tracing::info!(session, devices, "EIS client connected");
        Ok(client.into())
    }

    /// A remote desktop session ended: drop its EIS clients.
    pub fn end_remote(&mut self, session: u32) {
        for (token, _) in self.remote.eis.remove(&session).unwrap_or_default() {
            self.loop_handle.remove(token);
        }
        self.end_remote_share(session);
    }

    /// The keymap changed: replace EIS keyboards with ones that have it.
    pub fn refresh_eis_keyboards(&mut self) {
        let keymap = self.keymap_text().as_deref().and_then(memfd_keymap);
        for (_, client) in self.remote.eis.values().flatten() {
            let Ok(mut client) = client.try_borrow_mut() else { continue };
            let Some(seat) = client.seat.clone() else { continue };
            let old: Vec<_> = client.devices.iter().filter(|d| d.has_capability(DeviceCapability::Keyboard)).cloned().collect();
            if old.is_empty() {
                continue;
            }
            for device in &old {
                device.remove();
            }
            client.devices.retain(|d| !d.has_capability(DeviceCapability::Keyboard));
            let device = add_keyboard(&seat, keymap.as_ref());
            client.devices.push(device);
            if let Some(fd) = keymap.as_ref().and_then(|(fd, _)| fd.try_clone().ok()) {
                client.keymaps.push(fd);
            }
            if let Some(connection) = &client.connection {
                let _ = connection.flush();
            }
        }
    }

    fn eis_event(
        &mut self,
        client: &mut EisClient,
        connection: &Connection,
        event: EisRequestSourceEvent,
        allowed: u32,
    ) -> smithay::reexports::calloop::PostAction {
        use smithay::reexports::calloop::PostAction;
        let mut offered = enumflags2::BitFlags::<DeviceCapability>::empty();
        if allowed & 1 != 0 {
            offered |= DeviceCapability::Keyboard;
        }
        if allowed & 2 != 0 {
            offered |= DeviceCapability::Pointer | DeviceCapability::PointerAbsolute | DeviceCapability::Button | DeviceCapability::Scroll;
        }
        if allowed & 4 != 0 {
            offered |= DeviceCapability::Touch;
        }
        match event {
            EisRequestSourceEvent::Connected => {
                if connection.context_type() != eis::handshake::ContextType::Sender {
                    tracing::warn!("EIS client is not a sender");
                    return PostAction::Remove;
                }
                client.seat = Some(connection.add_seat(Some("default"), offered));
                client.connection = Some(connection.clone());
            }
            EisRequestSourceEvent::Request(request) => return self.eis_request(client, connection, request, offered),
        }
        PostAction::Continue
    }

    fn eis_request(
        &mut self,
        client: &mut EisClient,
        connection: &Connection,
        request: EisRequest,
        offered: enumflags2::BitFlags<DeviceCapability>,
    ) -> smithay::reexports::calloop::PostAction {
        use smithay::reexports::calloop::PostAction;
        let _ = connection;
        match request {
            EisRequest::Disconnect => return PostAction::Remove,
            EisRequest::Bind(bind) => {
                let wanted = bind.capabilities & offered;
                let regions: Vec<(i32, i32, i32, i32, f64)> = self
                    .space
                    .outputs()
                    .filter_map(|o| {
                        let g = self.space.output_geometry(o)?;
                        Some((g.loc.x, g.loc.y, g.size.w, g.size.h, o.current_scale().fractional_scale()))
                    })
                    .collect();
                let keymap = self.keymap_text().as_deref().and_then(memfd_keymap);
                if wanted.contains(DeviceCapability::Keyboard) {
                    client.devices.push(add_keyboard(&bind.seat, keymap.as_ref()));
                }
                let mut add = |name: &str, caps: enumflags2::BitFlags<DeviceCapability>| {
                    let regions = regions.clone();
                    let device = bind.seat.add_device(Some(name), eis::device::DeviceType::Virtual, caps, move |device| {
                        if caps.intersects(DeviceCapability::PointerAbsolute | DeviceCapability::Touch) {
                            for (x, y, w, h, scale) in &regions {
                                device.device().region(*x as u32, *y as u32, *w as u32, *h as u32, *scale as f32);
                            }
                        }
                    });
                    device.resumed();
                    client.devices.push(device);
                };
                if wanted.contains(DeviceCapability::Pointer) {
                    add("Helix remote pointer", DeviceCapability::Pointer | DeviceCapability::Button | DeviceCapability::Scroll);
                }
                if wanted.contains(DeviceCapability::PointerAbsolute) {
                    add("Helix remote absolute pointer", DeviceCapability::PointerAbsolute | DeviceCapability::Button | DeviceCapability::Scroll);
                }
                if wanted.contains(DeviceCapability::Touch) {
                    add("Helix remote touchscreen", DeviceCapability::Touch.into());
                }
                if let Some((fd, _)) = keymap {
                    client.keymaps.push(fd);
                }
            }
            EisRequest::PointerMotion(m) => self.remote_input(RemoteInput::Motion { dx: f64::from(m.dx), dy: f64::from(m.dy) }),
            EisRequest::PointerMotionAbsolute(m) => {
                let pos = self.clamp_to_outputs((f64::from(m.dx_absolute), f64::from(m.dy_absolute)).into());
                let time = self.now_ms();
                self.note_activity();
                self.pointer_to(pos, time);
                self.activate_constraint_under();
            }
            EisRequest::Button(b) => {
                let pressed = b.state == eis::button::ButtonState::Press;
                self.remote_input(RemoteInput::Button { button: b.button, pressed });
            }
            EisRequest::ScrollDelta(s) => {
                self.remote_input(RemoteInput::Axis { dx: f64::from(s.dx), dy: f64::from(s.dy), finish: false, source: 0 })
            }
            EisRequest::ScrollStop(_) | EisRequest::ScrollCancel(_) => {
                self.remote_input(RemoteInput::Axis { dx: 0.0, dy: 0.0, finish: true, source: 0 })
            }
            EisRequest::ScrollDiscrete(s) => {
                // libei steps are in 120ths of a detent.
                let time = self.now_ms();
                let mut frame = AxisFrame::new(time).source(AxisSource::Wheel);
                for (axis, v120) in [(Axis::Horizontal, s.discrete_dx), (Axis::Vertical, s.discrete_dy)] {
                    if v120 != 0 {
                        frame = frame.value(axis, f64::from(v120) / 120.0 * STEP_PX).v120(axis, v120);
                    }
                }
                self.note_activity();
                let pointer = self.seat.get_pointer().unwrap();
                pointer.axis(self, frame);
                pointer.frame(self);
            }
            EisRequest::KeyboardKey(k) => {
                let pressed = k.state == eis::keyboard::KeyState::Press;
                self.remote_input(RemoteInput::Key { keycode: k.key, pressed });
            }
            EisRequest::TouchDown(t) => {
                let pos = self.clamp_to_outputs((f64::from(t.x), f64::from(t.y)).into());
                let time = self.now_ms();
                self.note_activity();
                self.touch_down(slot(t.touch_id), pos, time);
            }
            EisRequest::TouchMotion(t) => {
                let pos = self.clamp_to_outputs((f64::from(t.x), f64::from(t.y)).into());
                let time = self.now_ms();
                self.touch_motion(slot(t.touch_id), pos, time);
            }
            EisRequest::TouchUp(t) => {
                let time = self.now_ms();
                self.touch_up(slot(t.touch_id), time);
            }
            EisRequest::TouchCancel(t) => {
                let time = self.now_ms();
                self.touch_up(slot(t.touch_id), time);
            }
            EisRequest::Frame(_) => {
                if let Some(touch) = self.seat.get_touch() {
                    touch.frame(self);
                }
            }
            _ => {}
        }
        smithay::reexports::calloop::PostAction::Continue
    }

    /// The active XKB keymap as text.
    fn keymap_text(&mut self) -> Option<String> {
        let keyboard = self.seat.get_keyboard()?;
        keyboard.with_xkb_state(self, |context| {
            let xkb = context.xkb().lock().ok()?;
            // SAFETY: read while the lock is held.
            let keymap = unsafe { xkb.keymap() };
            Some(keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1))
        })
    }

    /// Where (layout coordinates) a point in screen cast stream `stream`'s
    /// logical coordinates is.
    fn stream_point(&self, stream: u32, x: f64, y: f64) -> Option<Point<f64, Logical>> {
        let origin = self.stream_origin(stream)?;
        Some(origin + Point::from((x, y)))
    }
}

/// A sealed memfd holding `keymap` (NUL-terminated), and its size.
fn memfd_keymap(keymap: &str) -> Option<(OwnedFd, u32)> {
    use std::io::Write;
    use std::os::fd::FromRawFd;
    let mut bytes = keymap.as_bytes().to_vec();
    bytes.push(0);
    // SAFETY: memfd_create returns a new fd we own.
    let fd = unsafe { libc::memfd_create(c"helix-keymap".as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
    if fd < 0 {
        return None;
    }
    // SAFETY: fd is new and valid.
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    file.write_all(&bytes).ok()?;
    // SAFETY: plain fcntl on our fd.
    unsafe {
        libc::fcntl(fd, libc::F_ADD_SEALS, libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE);
    }
    Some((file.into(), bytes.len() as u32))
}
