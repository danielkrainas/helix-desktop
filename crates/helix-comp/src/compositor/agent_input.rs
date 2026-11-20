//! Agent Access's synthetic input (helixdesktop/spec
//! `contracts/agent-access/README.md`, "Screen and input methods"): the
//! shell's `helix_agent_input_v1` runs, each into one window.
//!
//! The compositor, not the shell, decides at the moment of every event:
//! the session is unlocked, no Agent Access prompt is shown, the window is
//! mapped and has the keyboard focus, and a pointer event's point is
//! inside the window with the window's own surface there. Events go
//! straight to the client, never through the compositor's key bindings,
//! the shell's keys, click-to-focus or window moving, so an agent cannot
//! switch VTs, lock the screen, open the overview or move to another
//! window by key or click.
//!
//! Keys (`key`) are those of the user's layout, through the seat, so apps
//! see real keycodes; text (`text`) is typed with a keymap made for it, as
//! a virtual keyboard does (wtype), so any character can be typed whatever
//! the layout, Caps Lock or held keys.
//!
//! The hard stop: input from anything else (the machine's devices, a
//! remote desktop session, EIS, the headless test socket) stops every run
//! at once and refuses new ones for [`COOLDOWN`].

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use helix_protocols::server::helix_agent_input_v1::{self, HelixAgentInputV1, Reason};
use smithay::backend::input::{Axis, AxisSource, ButtonState, KeyState};
use smithay::desktop::Window;
use smithay::input::keyboard::{xkb, FilterResult, KeymapFile, Keycode, ModifiersState};
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::RegistrationToken;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource};
use smithay::utils::{Logical, Point, Rectangle, SERIAL_COUNTER};
use smithay::wayland::seat::WaylandFocus;

use super::Helix;

/// How long new runs are refused after the user's input stopped one.
pub const COOLDOWN: Duration = Duration::from_secs(2);
/// Between two events of a run (and two characters of a text).
const STEP: Duration = Duration::from_millis(4);
/// Pixels per wheel step.
const STEP_PX: f64 = 15.0;
/// The most different characters one made keymap holds.
const KEYMAP_KEYS: usize = 200;
/// evdev code of the left Shift key.
const KEY_LEFTSHIFT: u32 = 42;

/// One request of a run.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Motion(f64, f64),
    Button(u32, bool),
    Scroll(i32, i32),
    Key(u32, bool),
    Text(String),
}

/// A text being typed: its keymaps, each with the characters typed with it.
struct Typing {
    chunks: VecDeque<Chunk>,
}

struct Chunk {
    keymap: KeymapFile,
    /// Keycodes (of `keymap`) in the order typed.
    keys: VecDeque<Keycode>,
}

/// One run: its window, what is left to do, what it holds down.
pub struct Run {
    object: HelixAgentInputV1,
    window: Window,
    queue: VecDeque<Step>,
    typing: Option<Typing>,
    /// Seat keycodes it holds down, in order.
    keys: Vec<Keycode>,
    buttons: Vec<u32>,
    timer: Option<RegistrationToken>,
    stopped: bool,
}

/// The runs, and the cooldown after a hard stop.
#[derive(Default)]
pub struct AgentInputState {
    runs: Vec<Run>,
    cooldown_until: Option<Instant>,
}

/// The made-up keymap's key names.
fn key_name(code: u32) -> String {
    format!("<H{code}>")
}

/// A keymap whose keys `9..` give `syms` (one level, no modifiers).
pub fn text_keymap(syms: &[xkb::Keysym]) -> Option<KeymapFile> {
    let mut keycodes = String::new();
    let mut symbols = String::new();
    for (i, sym) in syms.iter().enumerate() {
        let code = 9 + i as u32;
        keycodes.push_str(&format!("    {} = {code};\n", key_name(code)));
        let name = xkb::keysym_get_name(*sym);
        if name.is_empty() || name == "NoSymbol" {
            return None;
        }
        symbols.push_str(&format!("    key {} {{ [ {name} ] }};\n", key_name(code)));
    }
    let text = format!(
        "xkb_keymap {{\n  xkb_keycodes \"helix-agent\" {{\n    minimum = 8;\n    maximum = 255;\n{keycodes}  }};\n  xkb_types \"helix-agent\" {{ include \"complete\" }};\n  xkb_compatibility \"helix-agent\" {{ include \"complete\" }};\n  xkb_symbols \"helix-agent\" {{\n{symbols}  }};\n}};\n"
    );
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = xkb::Keymap::new_from_string(&context, text, xkb::KEYMAP_FORMAT_TEXT_V1, xkb::KEYMAP_COMPILE_NO_FLAGS)?;
    Some(KeymapFile::new(&keymap))
}

/// The keysym a character of a text is typed as.
pub fn char_keysym(c: char) -> Option<xkb::Keysym> {
    match c {
        '\n' => Some(xkb::Keysym::Return),
        '\t' => Some(xkb::Keysym::Tab),
        c if c.is_control() => None,
        c => {
            let sym = xkb::utf32_to_keysym(c as u32);
            (sym.raw() != 0).then_some(sym)
        }
    }
}

/// `text` cut into runs of at most `max` different characters, as the
/// keysyms of each run and the indexes into them in typing order.
pub fn chunks(text: &str, max: usize) -> Option<Vec<(Vec<xkb::Keysym>, Vec<usize>)>> {
    let mut out: Vec<(Vec<xkb::Keysym>, Vec<usize>)> = Vec::new();
    for c in text.chars() {
        let sym = char_keysym(c)?;
        let fits = out.last().is_some_and(|(syms, _)| syms.contains(&sym) || syms.len() < max);
        if !fits {
            out.push((Vec::new(), Vec::new()));
        }
        let (syms, order) = out.last_mut()?;
        let at = match syms.iter().position(|s| *s == sym) {
            Some(at) => at,
            None => {
                syms.push(sym);
                syms.len() - 1
            }
        };
        order.push(at);
    }
    Some(out)
}

fn typing(text: &str) -> Option<Typing> {
    let mut out = VecDeque::new();
    for (syms, order) in chunks(text, KEYMAP_KEYS)? {
        let keymap = text_keymap(&syms)?;
        out.push_back(Chunk { keymap, keys: order.into_iter().map(|i| Keycode::new(9 + i as u32)).collect() });
    }
    Some(Typing { chunks: out })
}

/// What a check of the moment found wrong.
type Check = Result<(), Reason>;

impl Helix {
    fn run_index(&self, object: &HelixAgentInputV1) -> Option<usize> {
        self.agent_input.runs.iter().position(|r| r.object == *object)
    }

    /// A new run into window `id` (its window list id).
    fn agent_input_start(&mut self, object: HelixAgentInputV1, id: u32) {
        let window = self.window_by_list_id(id);
        let cooling = self.agent_input.cooldown_until.is_some_and(|t| Instant::now() < t);
        let reason = match &window {
            _ if cooling => Some(Reason::Cooldown),
            None => Some(Reason::Gone),
            Some(_) => None,
        };
        let Some(window) = window else {
            object.stopped(Reason::Gone);
            return;
        };
        let mut run = Run { object: object.clone(), window, queue: VecDeque::new(), typing: None, keys: Vec::new(), buttons: Vec::new(), timer: None, stopped: false };
        if let Some(reason) = reason {
            run.stopped = true;
            object.stopped(reason);
        }
        tracing::info!(window = id, "agent input started");
        self.agent_input.runs.push(run);
    }

    /// Whether an agent's input is going on (a run not stopped).
    pub fn agent_input_running(&self) -> bool {
        self.agent_input.runs.iter().any(|r| !r.stopped)
    }

    /// Input from anything but an agent: stop every run (the hard stop).
    pub fn agent_input_interrupted(&mut self) {
        if !self.agent_input_running() {
            return;
        }
        tracing::info!("agent input stopped: the user used an input device");
        self.agent_input.cooldown_until = Some(Instant::now() + COOLDOWN);
        let objects: Vec<_> = self.agent_input.runs.iter().filter(|r| !r.stopped).map(|r| r.object.clone()).collect();
        for object in objects {
            self.agent_input_stop(&object, Reason::UserInput);
        }
    }

    /// End run `object` with `reason`, releasing what it holds.
    fn agent_input_stop(&mut self, object: &HelixAgentInputV1, reason: Reason) {
        let Some(i) = self.run_index(object) else { return };
        let run = &mut self.agent_input.runs[i];
        if run.stopped {
            return;
        }
        run.stopped = true;
        run.queue.clear();
        run.typing = None;
        let timer = run.timer.take();
        if let Some(timer) = timer {
            self.loop_handle.remove(timer);
        }
        if object.is_alive() {
            object.stopped(reason);
        }
        tracing::info!(?reason, "agent input stopped");
        self.agent_input_release(object);
    }

    /// Let go of the keys and buttons run `object` holds.
    fn agent_input_release(&mut self, object: &HelixAgentInputV1) {
        let Some(i) = self.run_index(object) else { return };
        let keys = std::mem::take(&mut self.agent_input.runs[i].keys);
        let buttons = std::mem::take(&mut self.agent_input.runs[i].buttons);
        let time = self.now_ms();
        for key in keys.into_iter().rev() {
            self.agent_seat_key(key, KeyState::Released, time);
        }
        if let Some(pointer) = self.seat.get_pointer() {
            for button in buttons {
                pointer.button(self, &ButtonEvent { button, state: ButtonState::Released, serial: SERIAL_COUNTER.next_serial(), time });
                pointer.frame(self);
            }
        }
    }

    /// A seat key straight to the client: no key bindings, no grabs of
    /// the compositor's.
    fn agent_seat_key(&mut self, keycode: Keycode, state: KeyState, time: u32) {
        let Some(keyboard) = self.seat.get_keyboard() else { return };
        let serial = SERIAL_COUNTER.next_serial();
        keyboard.input::<(), _>(self, keycode, state, serial, time, |_, _, _| FilterResult::Forward);
    }

    /// The window `surface` belongs to (through subsurfaces and popups).
    pub fn window_owning(&self, surface: &WlSurface) -> Option<Window> {
        let root = |s: &WlSurface| {
            let mut s = s.clone();
            while let Some(parent) = smithay::wayland::compositor::get_parent(&s) {
                s = parent;
            }
            s
        };
        let mut top = root(surface);
        if let Some(popup) = self.popups.find_popup(&top) {
            if let Ok(owner) = smithay::desktop::find_popup_root_surface(&popup) {
                top = root(&owner);
            }
        }
        self.window_for_surface(&top)
    }

    /// The window with the keyboard focus (also through its popups).
    fn agent_focus_window(&self) -> Option<Window> {
        let focus = self.seat.get_keyboard()?.current_focus()?;
        if let Some(window) = self.window_for_focus(&focus) {
            return Some(window);
        }
        let surface = focus.wl_surface()?.into_owned();
        self.window_owning(&surface)
    }

    /// The checks of the moment for run `i` (not stopped).
    fn agent_check(&self, i: usize) -> Check {
        let run = &self.agent_input.runs[i];
        if self.lock.locked {
            return Err(Reason::Locked);
        }
        if self.protected_prompt_shown() {
            return Err(Reason::Prompt);
        }
        if !smithay::utils::IsAlive::alive(&run.window) || self.space.element_location(&run.window).is_none() {
            return Err(Reason::Gone);
        }
        if self.agent_focus_window().as_ref() != Some(&run.window) {
            return Err(Reason::Focus);
        }
        Ok(())
    }

    /// Whether `pos` (global) is inside run `i`'s window with its own
    /// surface there; the surface and its position.
    fn agent_point(&self, i: usize, pos: Point<f64, Logical>) -> Result<(WlSurface, Point<f64, Logical>), Reason> {
        let window = &self.agent_input.runs[i].window;
        let location = self.space.element_location(window).ok_or(Reason::Gone)?;
        let rect = Rectangle::new(location, window.geometry().size).to_f64();
        if !rect.contains(pos) || self.frame_under(pos).is_some() {
            return Err(Reason::Outside);
        }
        let under = self.surface_under(pos).ok_or(Reason::Outside)?;
        if self.window_owning(&under.0).as_ref() != Some(window) {
            return Err(Reason::Outside);
        }
        Ok(under)
    }

    /// Move the pointer to `pos` over run `i`'s window.
    fn agent_motion(&mut self, i: usize, pos: Point<f64, Logical>, time: u32) -> Check {
        let under = self.agent_point(i, pos)?;
        let pointer = self.seat.get_pointer().ok_or(Reason::Invalid)?;
        pointer.motion(self, Some(under), &MotionEvent { location: pos, serial: SERIAL_COUNTER.next_serial(), time });
        pointer.frame(self);
        Ok(())
    }

    /// Carry out one step of run `object`: `Ok(true)` when its request is
    /// done.
    fn agent_step(&mut self, object: &HelixAgentInputV1) -> Result<bool, Reason> {
        let Some(i) = self.run_index(object) else { return Err(Reason::Gone) };
        self.agent_check(i)?;
        let time = self.now_ms();
        // A text: one character at a time, with its own keymap.
        if let Some(mut typing) = self.agent_input.runs[i].typing.take() {
            if self.text_input_secret() {
                return Err(Reason::Password);
            }
            while typing.chunks.front().is_some_and(|c| c.keys.is_empty()) {
                typing.chunks.pop_front();
            }
            let Some(chunk) = typing.chunks.front_mut() else { return Ok(true) };
            let Some(key) = chunk.keys.pop_front() else { return Ok(true) };
            let keyboard = self.seat.get_keyboard().ok_or(Reason::Invalid)?;
            keyboard.input_foreign(self, &chunk.keymap, ModifiersState::default(), key, KeyState::Pressed, time);
            keyboard.input_foreign(self, &chunk.keymap, ModifiersState::default(), key, KeyState::Released, time);
            let finished = typing.chunks.iter().all(|c| c.keys.is_empty());
            if !finished {
                self.agent_input.runs[i].typing = Some(typing);
            }
            return Ok(finished);
        }
        let Some(step) = self.agent_input.runs[i].queue.front().cloned() else { return Ok(false) };
        match step {
            Step::Motion(x, y) => {
                let location = self.space.element_location(&self.agent_input.runs[i].window).ok_or(Reason::Gone)?;
                let pos = location.to_f64() + Point::from((x, y));
                self.agent_motion(i, pos, time)?;
            }
            Step::Button(button, down) => {
                let pos = self.seat.get_pointer().ok_or(Reason::Invalid)?.current_location();
                self.agent_motion(i, pos, time)?;
                let pointer = self.seat.get_pointer().ok_or(Reason::Invalid)?;
                let state = if down { ButtonState::Pressed } else { ButtonState::Released };
                let held = &mut self.agent_input.runs[i].buttons;
                if down {
                    held.push(button);
                } else if let Some(at) = held.iter().position(|b| *b == button) {
                    held.remove(at);
                } else {
                    // Not held by the run: nothing to let go of.
                    return Ok(true);
                }
                pointer.button(self, &ButtonEvent { button, state, serial: SERIAL_COUNTER.next_serial(), time });
                pointer.frame(self);
            }
            Step::Scroll(x, y) => {
                let pos = self.seat.get_pointer().ok_or(Reason::Invalid)?.current_location();
                self.agent_motion(i, pos, time)?;
                let pointer = self.seat.get_pointer().ok_or(Reason::Invalid)?;
                let mut frame = AxisFrame::new(time).source(AxisSource::Wheel);
                for (axis, steps) in [(Axis::Horizontal, x), (Axis::Vertical, y)] {
                    if steps != 0 {
                        frame = frame.value(axis, f64::from(steps) * STEP_PX).v120(axis, steps * 120);
                    }
                }
                pointer.axis(self, frame);
                pointer.frame(self);
            }
            Step::Key(keysym, down) => {
                if self.text_input_secret() {
                    return Err(Reason::Password);
                }
                let (keycode, shift) = self.key_for_keysym(keysym).ok_or(Reason::NoKey)?;
                let shift_key = Keycode::new(KEY_LEFTSHIFT + 8);
                if down {
                    if shift {
                        self.agent_seat_key(shift_key, KeyState::Pressed, time);
                        self.agent_input.runs[i].keys.push(shift_key);
                    }
                    self.agent_seat_key(keycode, KeyState::Pressed, time);
                    self.agent_input.runs[i].keys.push(keycode);
                } else {
                    let held = &mut self.agent_input.runs[i].keys;
                    let Some(at) = held.iter().rposition(|k| *k == keycode) else { return Ok(true) };
                    held.remove(at);
                    self.agent_seat_key(keycode, KeyState::Released, time);
                    if shift {
                        let held = &mut self.agent_input.runs[i].keys;
                        if let Some(at) = held.iter().rposition(|k| *k == shift_key) {
                            held.remove(at);
                            self.agent_seat_key(shift_key, KeyState::Released, time);
                        }
                    }
                }
            }
            Step::Text(text) => {
                if text.is_empty() {
                    return Ok(true);
                }
                // Typed in the next steps; the request is answered when
                // its last character is.
                self.agent_input.runs[i].typing = Some(typing(&text).ok_or(Reason::Invalid)?);
                if let Some(Step::Text(t)) = self.agent_input.runs[i].queue.front_mut() {
                    t.clear();
                }
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Whether the focused app has a password or PIN field focused (its
    /// text input's content purpose).
    fn text_input_secret(&self) -> bool {
        use smithay::wayland::text_input::TextInputSeat;
        self.seat.text_input().focused_secret()
    }

    /// Run `object`'s next step now, and schedule the one after.
    fn agent_tick(&mut self, object: &HelixAgentInputV1) {
        let Some(i) = self.run_index(object) else { return };
        self.agent_input.runs[i].timer = None;
        if self.agent_input.runs[i].stopped {
            return;
        }
        match self.agent_step(object) {
            Ok(done) => {
                let Some(i) = self.run_index(object) else { return };
                if done {
                    self.agent_input.runs[i].queue.pop_front();
                    object.done();
                }
                if !self.agent_input.runs[i].queue.is_empty() || self.agent_input.runs[i].typing.is_some() {
                    self.agent_schedule(object);
                }
            }
            Err(reason) => self.agent_input_stop(object, reason),
        }
        let _ = self.display_handle.flush_clients();
    }

    fn agent_schedule(&mut self, object: &HelixAgentInputV1) {
        let Some(i) = self.run_index(object) else { return };
        if self.agent_input.runs[i].timer.is_some() || self.agent_input.runs[i].stopped {
            return;
        }
        let target = object.clone();
        let token = self.loop_handle.insert_source(Timer::from_duration(STEP), move |_, _, state: &mut Helix| {
            state.agent_tick(&target);
            TimeoutAction::Drop
        });
        match token {
            Ok(token) => self.agent_input.runs[i].timer = Some(token),
            Err(err) => {
                tracing::warn!(%err, "cannot time agent input");
                self.agent_input_stop(object, Reason::Invalid);
            }
        }
    }

    fn agent_request(&mut self, object: &HelixAgentInputV1, step: Step) {
        let Some(i) = self.run_index(object) else { return };
        if self.agent_input.runs[i].stopped {
            return;
        }
        self.agent_input.runs[i].queue.push_back(step);
        self.agent_schedule(object);
    }

    /// The run object is gone: let go of what it holds.
    fn agent_input_end(&mut self, object: &HelixAgentInputV1) {
        let Some(i) = self.run_index(object) else { return };
        if let Some(timer) = self.agent_input.runs[i].timer.take() {
            self.loop_handle.remove(timer);
        }
        if !self.agent_input.runs[i].stopped {
            self.agent_input_release(object);
        }
        if let Some(i) = self.run_index(object) {
            self.agent_input.runs.remove(i);
        }
        tracing::info!("agent input ended");
    }

    /// Called from the manager's `agent_input` request.
    pub(super) fn agent_input_new(&mut self, data_init: &mut DataInit<'_, Self>, id: smithay::reexports::wayland_server::New<HelixAgentInputV1>, window: u32) {
        let object = data_init.init(id, ());
        self.agent_input_start(object, window);
    }
}

impl Dispatch<HelixAgentInputV1, ()> for Helix {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &HelixAgentInputV1,
        request: helix_agent_input_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        use helix_agent_input_v1::Request;
        let state_of = |s: smithay::reexports::wayland_server::WEnum<helix_agent_input_v1::State>| matches!(s, smithay::reexports::wayland_server::WEnum::Value(helix_agent_input_v1::State::Down));
        let step = match request {
            Request::Motion { x, y } => Step::Motion(x, y),
            Request::Button { button, state: s } => Step::Button(button, state_of(s)),
            Request::Scroll { steps_x, steps_y } => Step::Scroll(steps_x, steps_y),
            Request::Key { keysym, state: s } => Step::Key(keysym, state_of(s)),
            Request::Text { text } => Step::Text(text),
            Request::Cancel => {
                state.agent_input_stop(resource, Reason::Cancelled);
                let _ = state.display_handle.flush_clients();
                return;
            }
            Request::Destroy => return,
            _ => return,
        };
        state.agent_request(resource, step);
    }

    fn destroyed(state: &mut Self, _client: smithay::reexports::wayland_server::backend::ClientId, resource: &HelixAgentInputV1, _data: &()) {
        state.agent_input_end(resource);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_in_chunks() {
        let out = chunks("abca", 2).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0, [xkb::Keysym::a, xkb::Keysym::b]);
        assert_eq!(out[0].1, [0, 1]);
        assert_eq!(out[1].0, [xkb::Keysym::c, xkb::Keysym::a]);
        assert_eq!(out[1].1, [0, 1]);
        assert!(chunks("bell\u{7}", 10).is_none(), "control characters are not typed");
        let out = chunks("a\nb\t€", 10).unwrap();
        assert_eq!(out[0].0, [xkb::Keysym::a, xkb::Keysym::Return, xkb::Keysym::b, xkb::Keysym::Tab, xkb::utf32_to_keysym('€' as u32)]);
    }

    #[test]
    fn keymaps_for_any_character() {
        let syms: Vec<_> = "héllo €😀ж".chars().map(|c| char_keysym(c).unwrap()).collect();
        let keymap = text_keymap(&syms);
        // xkeyboard-config must be installed for "complete".
        if std::path::Path::new("/usr/share/X11/xkb/types/complete").exists() {
            assert!(keymap.is_some());
        }
    }
}
