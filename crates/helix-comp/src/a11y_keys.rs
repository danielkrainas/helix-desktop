//! Keyboard accessibility (helix-desktop-spec
//! `contracts/gsettings/a11y-keyboard.md`): sticky, slow, bounce, toggle
//! and mouse keys, applied to device keys before anything else sees them.
//!
//! [`AccessX`] is a pure state machine: keys (evdev codes) and clock ticks
//! go in, [`Out`]s come out for the compositor to act on.

use std::collections::{HashMap, HashSet};

pub const SCHEMA: &str = "org.gnome.desktop.a11y.keyboard";
pub const SCHEMAS: [&str; 1] = [SCHEMA];

// evdev key codes.
const LEFTSHIFT: u32 = 42;
const RIGHTSHIFT: u32 = 54;
const MODIFIERS: [u32; 8] = [LEFTSHIFT, RIGHTSHIFT, 29, 97, 56, 100, 125, 126];
const LOCKS: [u32; 3] = [58, 69, 70];
const KP_SLASH: u32 = 98;
const KP_ASTERISK: u32 = 55;
const KP_MINUS: u32 = 74;
const KP_PLUS: u32 = 78;
const KP_5: u32 = 76;
const KP_0: u32 = 82;
const KP_DOT: u32 = 83;
/// Keypad direction keys: (code, dx, dy).
const DIRECTIONS: [(u32, i32, i32); 8] = [(71, -1, -1), (72, 0, -1), (73, 1, -1), (75, -1, 0), (77, 1, 0), (79, -1, 1), (80, 0, 1), (81, 1, 1)];

pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;

const SHIFT_PRESSES: u32 = 5;
const SHIFT_PRESSES_WITHIN_MS: u64 = 10_000;
const SHIFT_HOLD_MS: u64 = 8_000;

/// `org.gnome.desktop.a11y.keyboard`.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub enable: bool,
    pub feature_beep: bool,
    pub timeout: bool,
    pub timeout_secs: u64,
    pub sticky: bool,
    pub sticky_two_key_off: bool,
    pub sticky_beep: bool,
    pub slow: bool,
    pub slow_delay: u64,
    pub slow_beep_press: bool,
    pub slow_beep_accept: bool,
    pub slow_beep_reject: bool,
    pub bounce: bool,
    pub bounce_delay: u64,
    pub bounce_beep: bool,
    pub toggle: bool,
    pub mouse: bool,
    pub mouse_init_delay: u64,
    pub mouse_accel_time: u64,
    pub mouse_max_speed: f64,
}

impl Default for Settings {
    /// The schema's defaults.
    fn default() -> Self {
        Self {
            enable: false,
            feature_beep: false,
            timeout: false,
            timeout_secs: 200,
            sticky: false,
            sticky_two_key_off: false,
            sticky_beep: false,
            slow: false,
            slow_delay: 300,
            slow_beep_press: false,
            slow_beep_accept: false,
            slow_beep_reject: false,
            bounce: false,
            bounce_delay: 300,
            bounce_beep: false,
            toggle: false,
            mouse: false,
            mouse_init_delay: 300,
            mouse_accel_time: 300,
            mouse_max_speed: 10.0,
        }
    }
}

impl Settings {
    fn any_feature(&self) -> bool {
        self.sticky || self.slow || self.bounce || self.toggle || self.mouse
    }
}

/// The current settings (defaults without the schema).
pub fn read() -> Settings {
    use gio::prelude::*;
    let d = Settings::default();
    let Some(s) = gio::SettingsSchemaSource::default().and_then(|src| src.lookup(SCHEMA, true)).map(|_| gio::Settings::new(SCHEMA)) else {
        return d;
    };
    let ms = |key: &str| u64::try_from(s.int(key)).unwrap_or(0);
    Settings {
        enable: s.boolean("enable"),
        feature_beep: s.boolean("feature-state-change-beep"),
        timeout: s.boolean("timeout-enable"),
        timeout_secs: ms("disable-timeout"),
        sticky: s.boolean("stickykeys-enable"),
        sticky_two_key_off: s.boolean("stickykeys-two-key-off"),
        sticky_beep: s.boolean("stickykeys-modifier-beep"),
        slow: s.boolean("slowkeys-enable"),
        slow_delay: ms("slowkeys-delay"),
        slow_beep_press: s.boolean("slowkeys-beep-press"),
        slow_beep_accept: s.boolean("slowkeys-beep-accept"),
        slow_beep_reject: s.boolean("slowkeys-beep-reject"),
        bounce: s.boolean("bouncekeys-enable"),
        bounce_delay: ms("bouncekeys-delay"),
        bounce_beep: s.boolean("bouncekeys-beep-reject"),
        toggle: s.boolean("togglekeys-enable"),
        mouse: s.boolean("mousekeys-enable"),
        mouse_init_delay: ms("mousekeys-init-delay"),
        mouse_accel_time: ms("mousekeys-accel-time"),
        mouse_max_speed: f64::from(s.int("mousekeys-max-speed").max(0)),
    }
}

/// Write a boolean key (a feature turned on or off from the keyboard).
pub fn store(key: &str, value: bool) {
    use gio::prelude::*;
    if gio::SettingsSchemaSource::default().and_then(|src| src.lookup(SCHEMA, true)).is_some() {
        let _ = gio::Settings::new(SCHEMA).set_boolean(key, value);
        gio::Settings::sync();
    }
}

/// What the compositor should do.
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    /// A key (evdev code) goes down or up.
    Key(u32, bool),
    Beep,
    /// Move the pointer by this many logical pixels.
    Move(f64, f64),
    /// A pointer button goes down or up.
    Button(u32, bool),
    /// Set a boolean key of [`SCHEMA`].
    Set(&'static str, bool),
}

#[derive(Debug, Default)]
struct Sticky {
    latched: Vec<u32>,
    locked: Vec<u32>,
    /// Modifiers physically down.
    held: HashSet<u32>,
    /// A modifier pressed with no other key since.
    lone: Option<u32>,
    /// Other keys down, with the modifiers pressed for them.
    applied: HashMap<u32, Vec<u32>>,
}

#[derive(Debug, Default)]
struct MouseKeys {
    /// Direction keys held: code -> since.
    held: HashMap<u32, u64>,
    button: u32,
    /// Keypad keys whose release is ours.
    swallowed: HashSet<u32>,
    last_tick: u64,
    /// Sub-pixel motion not yet sent.
    carry: (f64, f64),
}

#[derive(Debug, Default)]
pub struct AccessX {
    s: Settings,
    sticky: Sticky,
    /// Slow keys waiting: code -> pressed at.
    pending: HashMap<u32, u64>,
    /// Slow keys accepted and still down.
    accepted: HashSet<u32>,
    /// Bounce keys: code -> last release.
    released_at: HashMap<u32, u64>,
    bounced: HashSet<u32>,
    mouse: MouseKeys,
    /// Shift presses in a row: (count, first at).
    shift_presses: (u32, u64),
    shift_down_since: Option<u64>,
    last_press: u64,
}

impl AccessX {
    pub fn new(settings: Settings) -> Self {
        let mut a = Self::default();
        a.set_settings(settings);
        a
    }

    pub fn settings(&self) -> &Settings {
        &self.s
    }

    /// New settings; turning a feature off lets go of its state.
    pub fn set_settings(&mut self, s: Settings) {
        if !s.sticky {
            self.sticky.latched.clear();
            self.sticky.locked.clear();
        }
        if !s.slow {
            self.pending.clear();
        }
        if !s.mouse {
            self.mouse.held.clear();
        }
        if !s.bounce {
            self.bounced.clear();
        }
        self.s = s;
    }

    /// Whether [`AccessX::tick`] has work to do soon (slow keys waiting,
    /// pointer moving, Shift held for the shortcut, the timeout).
    pub fn busy(&self) -> bool {
        !self.pending.is_empty() || !self.mouse.held.is_empty() || self.shift_down_since.is_some() || (self.s.timeout && self.s.any_feature())
    }

    fn set(&mut self, key: &'static str, value: bool, out: &mut Vec<Out>) {
        out.push(Out::Set(key, value));
        let mut s = self.s.clone();
        match key {
            "stickykeys-enable" => s.sticky = value,
            "slowkeys-enable" => s.slow = value,
            "bouncekeys-enable" => s.bounce = value,
            "togglekeys-enable" => s.toggle = value,
            "mousekeys-enable" => s.mouse = value,
            _ => {}
        }
        self.set_settings(s);
    }

    /// A device key (evdev `code`) went down or up at `now` (ms).
    pub fn key(&mut self, code: u32, pressed: bool, now: u64, num_lock: bool) -> Vec<Out> {
        let mut out = Vec::new();
        if pressed {
            self.last_press = now;
        }
        self.shortcuts(code, pressed, now, &mut out);
        // Bounce keys.
        if pressed {
            if self.s.bounce && self.released_at.get(&code).is_some_and(|t| now.saturating_sub(*t) < self.s.bounce_delay) {
                self.bounced.insert(code);
                if self.s.bounce_beep {
                    out.push(Out::Beep);
                }
                return out;
            }
        } else {
            if self.bounced.remove(&code) {
                return out;
            }
            self.released_at.insert(code, now);
        }
        // Slow keys.
        if self.s.slow && pressed {
            self.pending.insert(code, now);
            if self.s.slow_beep_press {
                out.push(Out::Beep);
            }
            return out;
        }
        if !pressed {
            if self.pending.remove(&code).is_some() {
                if self.s.slow_beep_reject {
                    out.push(Out::Beep);
                }
                return out;
            }
            self.accepted.remove(&code);
        }
        self.accepted_key(code, pressed, now, num_lock, &mut out);
        out
    }

    /// Clock tick at `now` (ms).
    pub fn tick(&mut self, now: u64, num_lock: bool) -> Vec<Out> {
        let mut out = Vec::new();
        // Slow keys held long enough.
        let ready: Vec<u32> = self.pending.iter().filter(|(_, t)| now.saturating_sub(**t) >= self.s.slow_delay).map(|(c, _)| *c).collect();
        for code in ready {
            self.pending.remove(&code);
            self.accepted.insert(code);
            if self.s.slow_beep_accept {
                out.push(Out::Beep);
            }
            self.accepted_key(code, true, now, num_lock, &mut out);
        }
        // Shift held: slow keys on or off.
        if let Some(since) = self.shift_down_since {
            if self.s.enable && now.saturating_sub(since) >= SHIFT_HOLD_MS {
                self.shift_down_since = None;
                let on = !self.s.slow;
                self.set("slowkeys-enable", on, &mut out);
                if self.s.feature_beep {
                    out.push(Out::Beep);
                }
            }
        }
        // The timeout.
        if self.s.timeout && self.s.any_feature() && now.saturating_sub(self.last_press) >= self.s.timeout_secs * 1000 {
            for (key, on) in [
                ("stickykeys-enable", self.s.sticky),
                ("slowkeys-enable", self.s.slow),
                ("bouncekeys-enable", self.s.bounce),
                ("togglekeys-enable", self.s.toggle),
                ("mousekeys-enable", self.s.mouse),
            ] {
                if on {
                    self.set(key, false, &mut out);
                }
            }
            if self.s.feature_beep {
                out.push(Out::Beep);
            }
        }
        self.move_pointer(now, &mut out);
        out
    }

    /// The `enable` shortcuts: Shift five times, Shift held.
    fn shortcuts(&mut self, code: u32, pressed: bool, now: u64, out: &mut Vec<Out>) {
        let shift = code == LEFTSHIFT || code == RIGHTSHIFT;
        if !pressed {
            if shift {
                self.shift_down_since = None;
            }
            return;
        }
        if !shift {
            self.shift_presses = (0, 0);
            self.shift_down_since = None;
            return;
        }
        if self.shift_down_since.is_none() {
            self.shift_down_since = Some(now);
        }
        let (count, first) = self.shift_presses;
        self.shift_presses = if count == 0 || now.saturating_sub(first) > SHIFT_PRESSES_WITHIN_MS { (1, now) } else { (count + 1, first) };
        if self.s.enable && self.shift_presses.0 >= SHIFT_PRESSES {
            self.shift_presses = (0, 0);
            let on = !self.s.sticky;
            self.set("stickykeys-enable", on, out);
            if self.s.feature_beep {
                out.push(Out::Beep);
            }
        }
    }

    /// A key that passed bounce and slow keys.
    fn accepted_key(&mut self, code: u32, pressed: bool, now: u64, num_lock: bool, out: &mut Vec<Out>) {
        if self.mouse_key(code, pressed, now, num_lock, out) {
            return;
        }
        if pressed && self.s.toggle && LOCKS.contains(&code) {
            out.push(Out::Beep);
        }
        self.sticky_key(code, pressed, out);
    }

    /// Mouse keys; true when the key was theirs.
    fn mouse_key(&mut self, code: u32, pressed: bool, now: u64, num_lock: bool, out: &mut Vec<Out>) -> bool {
        if !pressed {
            self.mouse.held.remove(&code);
            return self.mouse.swallowed.remove(&code);
        }
        let keypad = DIRECTIONS.iter().any(|d| d.0 == code) || [KP_SLASH, KP_ASTERISK, KP_MINUS, KP_PLUS, KP_5, KP_0, KP_DOT].contains(&code);
        if !(self.s.mouse && !num_lock && keypad) {
            return false;
        }
        self.mouse.swallowed.insert(code);
        // The left button until another is chosen.
        let b = if self.mouse.button == 0 { BTN_LEFT } else { self.mouse.button };
        match code {
            KP_SLASH => self.mouse.button = BTN_LEFT,
            KP_ASTERISK => self.mouse.button = BTN_MIDDLE,
            KP_MINUS => self.mouse.button = BTN_RIGHT,
            KP_5 => out.extend([Out::Button(b, true), Out::Button(b, false)]),
            KP_PLUS => out.extend([Out::Button(b, true), Out::Button(b, false), Out::Button(b, true), Out::Button(b, false)]),
            KP_0 => out.push(Out::Button(b, true)),
            KP_DOT => out.push(Out::Button(b, false)),
            _ => {
                if let Some((_, dx, dy)) = DIRECTIONS.iter().find(|d| d.0 == code) {
                    if self.mouse.held.is_empty() {
                        self.mouse.last_tick = now;
                        self.mouse.carry = (0.0, 0.0);
                    }
                    self.mouse.held.insert(code, now);
                    out.push(Out::Move(f64::from(*dx), f64::from(*dy)));
                }
            }
        }
        true
    }

    fn move_pointer(&mut self, now: u64, out: &mut Vec<Out>) {
        if self.mouse.held.is_empty() {
            return;
        }
        let dt = now.saturating_sub(self.mouse.last_tick) as f64 / 1000.0;
        self.mouse.last_tick = now;
        let (mut x, mut y) = self.mouse.carry;
        for (code, since) in &self.mouse.held {
            let held = now.saturating_sub(*since);
            if held < self.s.mouse_init_delay {
                continue;
            }
            let ramp = if self.s.mouse_accel_time == 0 { 1.0 } else { ((held - self.s.mouse_init_delay) as f64 / self.s.mouse_accel_time as f64).min(1.0) };
            // At least one pixel per second once moving.
            let speed = (self.s.mouse_max_speed * ramp).max(1.0);
            if let Some((_, dx, dy)) = DIRECTIONS.iter().find(|d| d.0 == *code) {
                x += f64::from(*dx) * speed * dt;
                y += f64::from(*dy) * speed * dt;
            }
        }
        let (wx, wy) = (x.trunc(), y.trunc());
        self.mouse.carry = (x - wx, y - wy);
        if wx != 0.0 || wy != 0.0 {
            out.push(Out::Move(wx, wy));
        }
    }

    fn sticky_key(&mut self, code: u32, pressed: bool, out: &mut Vec<Out>) {
        let st = &mut self.sticky;
        if !self.s.sticky {
            // Let go of modifiers still pressed for a key.
            if !pressed {
                if let Some(mods) = st.applied.remove(&code) {
                    out.push(Out::Key(code, false));
                    out.extend(mods.into_iter().map(|m| Out::Key(m, false)));
                    return;
                }
            }
            out.push(Out::Key(code, pressed));
            return;
        }
        let modifier = MODIFIERS.contains(&code);
        match (modifier, pressed) {
            (true, true) => {
                st.held.insert(code);
                st.lone = st.applied.is_empty().then_some(code);
                out.push(Out::Key(code, true));
            }
            (true, false) => {
                st.held.remove(&code);
                out.push(Out::Key(code, false));
                if st.lone.take() == Some(code) {
                    if let Some(i) = st.locked.iter().position(|m| *m == code) {
                        st.locked.remove(i);
                    } else if let Some(i) = st.latched.iter().position(|m| *m == code) {
                        st.latched.remove(i);
                        st.locked.push(code);
                    } else {
                        st.latched.push(code);
                    }
                    if self.s.sticky_beep {
                        out.push(Out::Beep);
                    }
                }
            }
            (false, true) => {
                st.lone = None;
                if !st.held.is_empty() && self.s.sticky_two_key_off {
                    out.push(Out::Key(code, true));
                    st.applied.insert(code, Vec::new());
                    self.set("stickykeys-enable", false, out);
                    return;
                }
                let mods: Vec<u32> = st.latched.drain(..).chain(st.locked.iter().copied()).filter(|m| !st.held.contains(m)).collect();
                out.extend(mods.iter().map(|m| Out::Key(*m, true)));
                out.push(Out::Key(code, true));
                st.applied.insert(code, mods);
            }
            (false, false) => {
                out.push(Out::Key(code, false));
                if let Some(mods) = st.applied.remove(&code) {
                    out.extend(mods.into_iter().rev().map(|m| Out::Key(m, false)));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: u32 = 30;
    const B: u32 = 48;

    fn with(f: impl FnOnce(&mut Settings)) -> AccessX {
        let mut s = Settings::default();
        f(&mut s);
        AccessX::new(s)
    }

    fn keys(out: &[Out]) -> Vec<(u32, bool)> {
        out.iter().filter_map(|o| if let Out::Key(c, p) = o { Some((*c, *p)) } else { None }).collect()
    }

    #[test]
    fn off_passes_keys() {
        let mut a = AccessX::new(Settings::default());
        assert_eq!(a.key(A, true, 0, false), [Out::Key(A, true)]);
        assert_eq!(a.key(A, false, 10, false), [Out::Key(A, false)]);
    }

    #[test]
    fn sticky_latch_and_lock() {
        let mut a = with(|s| {
            s.sticky = true;
            s.sticky_beep = true;
        });
        // Shift alone: latched (and beeps).
        a.key(LEFTSHIFT, true, 0, false);
        assert_eq!(a.key(LEFTSHIFT, false, 10, false), [Out::Key(LEFTSHIFT, false), Out::Beep]);
        // Next key gets Shift; the one after doesn't.
        assert_eq!(keys(&a.key(A, true, 20, false)), [(LEFTSHIFT, true), (A, true)]);
        assert_eq!(keys(&a.key(A, false, 30, false)), [(A, false), (LEFTSHIFT, false)]);
        assert_eq!(keys(&a.key(B, true, 40, false)), [(B, true)]);
        a.key(B, false, 50, false);
        // Twice: locked, for every key until a third time.
        for t in [60, 80] {
            a.key(29, true, t, false);
            a.key(29, false, t + 5, false);
        }
        for t in [100, 120] {
            assert_eq!(keys(&a.key(A, true, t, false)), [(29, true), (A, true)]);
            a.key(A, false, t + 5, false);
        }
        a.key(29, true, 140, false);
        a.key(29, false, 145, false);
        assert_eq!(keys(&a.key(A, true, 160, false)), [(A, true)]);
    }

    #[test]
    fn sticky_held_combos_and_two_key_off() {
        let mut a = with(|s| s.sticky = true);
        a.key(29, true, 0, false);
        assert_eq!(keys(&a.key(A, true, 10, false)), [(A, true)]);
        a.key(A, false, 20, false);
        a.key(29, false, 30, false);
        // Not latched: a key came in between.
        assert_eq!(keys(&a.key(B, true, 40, false)), [(B, true)]);
        a.key(B, false, 50, false);

        let mut a = with(|s| {
            s.sticky = true;
            s.sticky_two_key_off = true;
        });
        a.key(29, true, 0, false);
        let out = a.key(A, true, 10, false);
        assert!(out.contains(&Out::Set("stickykeys-enable", false)), "{out:?}");
        assert!(!a.settings().sticky);
    }

    #[test]
    fn slow_keys() {
        let mut a = with(|s| {
            s.slow = true;
            s.slow_beep_reject = true;
        });
        assert!(a.key(A, true, 0, false).is_empty());
        assert!(a.tick(200, false).is_empty());
        // Released early: rejected.
        assert_eq!(a.key(A, false, 250, false), [Out::Beep]);
        assert!(a.tick(400, false).is_empty());
        // Held long enough: accepted.
        a.key(B, true, 1000, false);
        assert_eq!(a.tick(1300, false), [Out::Key(B, true)]);
        assert_eq!(a.key(B, false, 1400, false), [Out::Key(B, false)]);
    }

    #[test]
    fn bounce_keys() {
        let mut a = with(|s| s.bounce = true);
        a.key(A, true, 0, false);
        a.key(A, false, 50, false);
        assert!(a.key(A, true, 200, false).is_empty(), "within 300 ms of the release");
        assert!(a.key(A, false, 220, false).is_empty());
        assert_eq!(a.key(A, true, 600, false), [Out::Key(A, true)]);
        assert_eq!(a.key(B, true, 610, false), [Out::Key(B, true)], "other keys are not affected");
    }

    #[test]
    fn toggle_keys_beep() {
        let mut a = with(|s| s.toggle = true);
        assert_eq!(a.key(58, true, 0, false), [Out::Beep, Out::Key(58, true)]);
    }

    #[test]
    fn mouse_keys_default_button() {
        let mut a = AccessX::default();
        a.set_settings(Settings { mouse: true, ..Settings::default() });
        assert_eq!(a.key(KP_5, true, 0, false), [Out::Button(BTN_LEFT, true), Out::Button(BTN_LEFT, false)]);
    }

    #[test]
    fn mouse_keys() {
        let mut a = with(|s| {
            s.mouse = true;
            s.mouse_max_speed = 100.0;
        });
        // Num Lock on: the keypad types.
        assert_eq!(a.key(81, true, 0, true), [Out::Key(81, true)]);
        a.key(81, false, 5, true);
        // Off: one pixel, then nothing until the initial delay.
        assert_eq!(a.key(81, true, 10, false), [Out::Move(1.0, 1.0)]);
        assert!(a.tick(200, false).is_empty());
        // Accelerating to 100 px/s over 300 ms after the 300 ms delay.
        a.tick(610, false);
        let out = a.tick(1110, false);
        assert_eq!(out, [Out::Move(50.0, 50.0)], "full speed for half a second");
        assert!(a.key(81, false, 1120, false).is_empty());
        assert!(a.tick(1200, false).is_empty());
        // Buttons.
        a.key(KP_MINUS, true, 1300, false);
        assert_eq!(a.key(KP_5, true, 1310, false), [Out::Button(BTN_RIGHT, true), Out::Button(BTN_RIGHT, false)]);
        a.key(KP_SLASH, true, 1320, false);
        assert_eq!(a.key(KP_0, true, 1330, false), [Out::Button(BTN_LEFT, true)]);
        assert_eq!(a.key(KP_DOT, true, 1340, false), [Out::Button(BTN_LEFT, false)]);
    }

    #[test]
    fn shift_shortcuts() {
        let mut a = with(|s| {
            s.enable = true;
            s.feature_beep = true;
        });
        let mut out = Vec::new();
        for i in 0..5 {
            out = a.key(LEFTSHIFT, true, i * 100, false);
            a.key(LEFTSHIFT, false, i * 100 + 50, false);
        }
        assert!(out.contains(&Out::Set("stickykeys-enable", true)) && out.contains(&Out::Beep), "{out:?}");
        assert!(a.settings().sticky);
        // Held for 8 seconds: slow keys.
        a.key(RIGHTSHIFT, true, 10_000, false);
        assert!(a.tick(17_000, false).is_empty());
        assert!(a.tick(18_000, false).contains(&Out::Set("slowkeys-enable", true)));
        // Without `enable`, nothing.
        let mut a = AccessX::new(Settings::default());
        for i in 0..5 {
            assert!(!a.key(LEFTSHIFT, true, i * 100, false).iter().any(|o| matches!(o, Out::Set(..))));
            a.key(LEFTSHIFT, false, i * 100 + 50, false);
        }
    }

    #[test]
    fn timeout() {
        let mut a = with(|s| {
            s.timeout = true;
            s.timeout_secs = 10;
            s.bounce = true;
            s.toggle = true;
        });
        a.key(A, true, 0, false);
        assert!(a.tick(9_000, false).is_empty());
        let out = a.tick(10_000, false);
        assert!(out.contains(&Out::Set("bouncekeys-enable", false)) && out.contains(&Out::Set("togglekeys-enable", false)), "{out:?}");
        assert!(!a.busy());
    }
}
