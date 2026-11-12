//! Mouse accessibility (helix-desktop-spec `contracts/gsettings/a11y-mouse.md`):
//! simulated secondary click and dwell click. A pure state machine:
//! pointer events and clock ticks in, [`Out`]s out.

pub const SCHEMA: &str = "org.gnome.desktop.a11y.mouse";
pub const SCHEMAS: [&str; 1] = [SCHEMA];

pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Dir {
    #[default]
    Left,
    Right,
    Up,
    Down,
}

impl Dir {
    fn parse(s: &str) -> Self {
        match s {
            "right" => Dir::Right,
            "up" => Dir::Up,
            "down" => Dir::Down,
            _ => Dir::Left,
        }
    }

    /// The main direction of a move.
    fn of(dx: f64, dy: f64) -> Self {
        if dx.abs() >= dy.abs() {
            if dx < 0.0 { Dir::Left } else { Dir::Right }
        } else if dy < 0.0 {
            Dir::Up
        } else {
            Dir::Down
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub secondary: bool,
    pub secondary_ms: u64,
    pub dwell: bool,
    pub dwell_ms: u64,
    pub threshold: f64,
    pub gesture: bool,
    pub single: Dir,
    pub double: Dir,
    pub drag: Dir,
    pub secondary_dir: Dir,
}

impl Default for Settings {
    fn default() -> Self {
        Self { secondary: false, secondary_ms: 1200, dwell: false, dwell_ms: 1200, threshold: 10.0, gesture: false, single: Dir::Left, double: Dir::Up, drag: Dir::Down, secondary_dir: Dir::Right }
    }
}

/// The current settings.
pub fn read() -> Settings {
    use gio::prelude::*;
    let d = Settings::default();
    let Some(s) = gio::SettingsSchemaSource::default().and_then(|src| src.lookup(SCHEMA, true)).map(|_| gio::Settings::new(SCHEMA)) else { return d };
    let ms = |key: &str| (s.double(key).max(0.0) * 1000.0) as u64;
    Settings {
        secondary: s.boolean("secondary-click-enabled"),
        secondary_ms: ms("secondary-click-time"),
        dwell: s.boolean("dwell-click-enabled"),
        dwell_ms: ms("dwell-time"),
        threshold: f64::from(s.int("dwell-threshold").max(0)),
        gesture: s.string("dwell-mode") == "gesture",
        single: Dir::parse(&s.string("dwell-gesture-single")),
        double: Dir::parse(&s.string("dwell-gesture-double")),
        drag: Dir::parse(&s.string("dwell-gesture-drag")),
        secondary_dir: Dir::parse(&s.string("dwell-gesture-secondary")),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    Button(u32, bool),
    /// Move the pointer here (layout coordinates).
    Warp(f64, f64),
}

#[derive(Debug, Clone, Copy)]
struct Held {
    at: (f64, f64),
    since: u64,
    long: bool,
}

#[derive(Debug, Default)]
pub struct Mouse {
    s: Settings,
    /// A primary press held back (secondary click).
    held: Option<Held>,
    /// The primary press went through and its release must too.
    passed: bool,
    /// Device buttons down.
    down: u32,
    /// Where the pointer came to rest, and since when.
    rest: Option<((f64, f64), u64)>,
    /// It moved since the last dwell click.
    armed: bool,
    /// Waiting for a gesture after a dwell: where, since when.
    gesture: Option<((f64, f64), u64)>,
    /// A dwell drag holds the primary button.
    dragging: bool,
}

fn dist(a: (f64, f64), b: (f64, f64)) -> f64 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

impl Mouse {
    pub fn settings(&self) -> &Settings {
        &self.s
    }

    /// New settings (dwell state starts over).
    pub fn set_settings(&mut self, s: Settings) -> Vec<Out> {
        let mut out = Vec::new();
        if !s.dwell && self.dragging {
            self.dragging = false;
            out.push(Out::Button(BTN_LEFT, false));
        }
        if !s.secondary && self.held.is_some() {
            self.held = None;
        }
        self.gesture = None;
        self.s = s;
        out
    }

    /// Whether ticks matter now.
    pub fn busy(&self) -> bool {
        self.held.is_some() || (self.s.dwell && (self.armed || self.gesture.is_some() || self.dragging))
    }

    /// A device button went down or up at `pos`.
    pub fn button(&mut self, button: u32, pressed: bool, now: u64, pos: (f64, f64)) -> Vec<Out> {
        if pressed {
            self.down += 1;
        } else {
            self.down = self.down.saturating_sub(1);
        }
        // Any click is a fresh start for dwelling.
        self.gesture = None;
        if button != BTN_LEFT || !self.s.secondary {
            if button == BTN_LEFT && !pressed && self.passed {
                self.passed = false;
            }
            return vec![Out::Button(button, pressed)];
        }
        if pressed {
            self.held = Some(Held { at: pos, since: now, long: false });
            return Vec::new();
        }
        if self.passed {
            self.passed = false;
            return vec![Out::Button(BTN_LEFT, false)];
        }
        match self.held.take() {
            Some(h) if h.long => vec![Out::Button(BTN_RIGHT, true), Out::Button(BTN_RIGHT, false)],
            Some(_) => vec![Out::Button(BTN_LEFT, true), Out::Button(BTN_LEFT, false)],
            None => vec![Out::Button(BTN_LEFT, false)],
        }
    }

    /// The pointer moved to `pos` (from a device).
    pub fn moved(&mut self, pos: (f64, f64), now: u64) -> Vec<Out> {
        let mut out = Vec::new();
        let t = self.s.threshold;
        // Secondary click: moving away lets the press through.
        if let Some(h) = self.held {
            if dist(pos, h.at) > t {
                self.held = None;
                self.passed = true;
                out.push(Out::Button(BTN_LEFT, true));
            }
        }
        if !self.s.dwell {
            return out;
        }
        // A gesture after a dwell.
        if let Some((origin, _)) = self.gesture {
            if dist(pos, origin) > t {
                self.gesture = None;
                let dir = Dir::of(pos.0 - origin.0, pos.1 - origin.1);
                let s = &self.s;
                let mut clicks = Vec::new();
                if dir == s.single {
                    clicks = vec![Out::Button(BTN_LEFT, true), Out::Button(BTN_LEFT, false)];
                } else if dir == s.double {
                    clicks = vec![Out::Button(BTN_LEFT, true), Out::Button(BTN_LEFT, false), Out::Button(BTN_LEFT, true), Out::Button(BTN_LEFT, false)];
                } else if dir == s.secondary_dir {
                    clicks = vec![Out::Button(BTN_RIGHT, true), Out::Button(BTN_RIGHT, false)];
                } else if dir == s.drag {
                    self.dragging = true;
                    clicks = vec![Out::Button(BTN_LEFT, true)];
                }
                if !clicks.is_empty() {
                    out.push(Out::Warp(origin.0, origin.1));
                    out.extend(clicks);
                }
                self.rest = Some((origin, now));
                return out;
            }
        }
        match self.rest {
            Some((at, _)) if dist(pos, at) <= t => {}
            _ => {
                if self.rest.is_some() {
                    self.armed = true;
                }
                self.rest = Some((pos, now));
            }
        }
        out
    }

    /// Clock tick.
    pub fn tick(&mut self, now: u64) -> Vec<Out> {
        if let Some(h) = self.held.as_mut() {
            if now.saturating_sub(h.since) >= self.s.secondary_ms {
                h.long = true;
            }
        }
        if !self.s.dwell {
            return Vec::new();
        }
        if let Some((_, since)) = self.gesture {
            if now.saturating_sub(since) >= self.s.dwell_ms {
                self.gesture = None;
            }
            return Vec::new();
        }
        let Some((at, since)) = self.rest else { return Vec::new() };
        let held_down = self.down > 0 && !self.dragging;
        if !self.armed || held_down || now.saturating_sub(since) < self.s.dwell_ms {
            return Vec::new();
        }
        self.armed = false;
        if self.dragging {
            self.dragging = false;
            return vec![Out::Button(BTN_LEFT, false)];
        }
        if self.s.gesture {
            self.gesture = Some((at, now));
            return Vec::new();
        }
        vec![Out::Button(BTN_LEFT, true), Out::Button(BTN_LEFT, false)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(f: impl FnOnce(&mut Settings)) -> Mouse {
        let mut s = Settings::default();
        f(&mut s);
        let mut m = Mouse::default();
        m.set_settings(s);
        m
    }

    #[test]
    fn off_passes_through() {
        let mut m = Mouse::default();
        assert_eq!(m.button(BTN_LEFT, true, 0, (0.0, 0.0)), [Out::Button(BTN_LEFT, true)]);
        assert_eq!(m.button(BTN_LEFT, false, 10, (0.0, 0.0)), [Out::Button(BTN_LEFT, false)]);
        assert!(m.tick(5000).is_empty());
    }

    #[test]
    fn secondary_click() {
        let mut m = with(|s| s.secondary = true);
        // A short click.
        assert!(m.button(BTN_LEFT, true, 0, (5.0, 5.0)).is_empty());
        assert_eq!(m.button(BTN_LEFT, false, 200, (5.0, 5.0)), [Out::Button(BTN_LEFT, true), Out::Button(BTN_LEFT, false)]);
        // Held long: a secondary click on release.
        m.button(BTN_LEFT, true, 1000, (5.0, 5.0));
        m.tick(2300);
        assert_eq!(m.button(BTN_LEFT, false, 2400, (5.0, 5.0)), [Out::Button(BTN_RIGHT, true), Out::Button(BTN_RIGHT, false)]);
        // Dragging: the press goes through when it moves away.
        m.button(BTN_LEFT, true, 3000, (5.0, 5.0));
        assert_eq!(m.moved((50.0, 5.0), 3100), [Out::Button(BTN_LEFT, true)]);
        m.tick(5000);
        assert_eq!(m.button(BTN_LEFT, false, 5100, (50.0, 5.0)), [Out::Button(BTN_LEFT, false)]);
    }

    #[test]
    fn dwell_click() {
        let mut m = with(|s| s.dwell = true);
        m.moved((0.0, 0.0), 0);
        assert!(m.tick(2000).is_empty(), "never moved yet");
        m.moved((100.0, 0.0), 2100);
        m.moved((103.0, 2.0), 2200);
        assert!(m.tick(3000).is_empty());
        assert_eq!(m.tick(3300), [Out::Button(BTN_LEFT, true), Out::Button(BTN_LEFT, false)]);
        assert!(m.tick(6000).is_empty(), "once until it moves again");
        // Not while a button is held.
        m.moved((200.0, 0.0), 6100);
        m.button(BTN_RIGHT, true, 6100, (200.0, 0.0));
        assert!(m.tick(8000).is_empty());
    }

    #[test]
    fn dwell_gestures() {
        let mut m = with(|s| {
            s.dwell = true;
            s.gesture = true;
        });
        m.moved((0.0, 0.0), 0);
        m.moved((100.0, 100.0), 100);
        assert!(m.tick(1400).is_empty(), "waits for a gesture");
        // Right: secondary (defaults), at the resting point.
        assert_eq!(m.moved((130.0, 102.0), 1500), [Out::Warp(100.0, 100.0), Out::Button(BTN_RIGHT, true), Out::Button(BTN_RIGHT, false)]);
        // Down: drag, released by the next dwell.
        m.moved((300.0, 300.0), 1600);
        m.tick(2900);
        assert_eq!(m.moved((300.0, 330.0), 3000), [Out::Warp(300.0, 300.0), Out::Button(BTN_LEFT, true)]);
        m.moved((400.0, 400.0), 3100);
        assert_eq!(m.tick(4400), [Out::Button(BTN_LEFT, false)]);
    }
}
