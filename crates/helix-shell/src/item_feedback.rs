//! How the app grid's icons and the dock's items answer hovering,
//! selection, pressing and launching (helix-desktop-spec
//! `contracts/wayland/README.md`, "Item feedback"). Pure timing: callers
//! paint what [`Feedback::look`] and [`Feedback::ghosts`] say, and keep
//! redrawing while [`Feedback::moving`] is true.
//!
//! Each item has a target look (lit: hovered or selected; pressed); when
//! what it should look like changes, it moves from where it is now to the
//! new target.

use std::time::{Duration, Instant};

/// The icon's size while lit (hovered or selected), and while pressed.
pub const HOVER_SCALE: f64 = 1.08;
pub const PRESS_SCALE: f64 = 0.92;
/// How far the launch copy grows.
pub const GHOST_SCALE: f64 = 1.8;

const FADE_IN: f64 = 0.12;
const FADE_OUT: f64 = 0.2;
const EASE: f64 = 0.12;
const SPRING: f64 = 0.3;
const GHOST: f64 = 0.25;
/// How long the launcher stays for the launch copy.
pub const LAUNCH_LINGER: Duration = Duration::from_millis(250);

/// `org.gnome.desktop.interface enable-animations` (true without it).
pub fn animations_enabled() -> bool {
    use gio::prelude::*;
    const SCHEMA: &str = "org.gnome.desktop.interface";
    if !helix_settings::has_schema(SCHEMA) {
        return true;
    }
    let s = gio::Settings::new(SCHEMA);
    !s.settings_schema().is_some_and(|k| k.has_key("enable-animations")) || s.boolean("enable-animations")
}

/// What one item looks like at a moment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Look {
    /// The hover/selection highlight, 0 (none) to 1.
    pub highlight: f64,
    /// Pressed: the darker highlight.
    pub pressed: bool,
    /// The icon's size, 1 = normal.
    pub scale: f64,
}

/// An item on its way to a new look.
#[derive(Debug, Clone)]
struct Move<K> {
    key: K,
    at: Instant,
    from_scale: f64,
    from_highlight: f64,
    /// Released from pressed: springs back with an overshoot.
    spring: bool,
}

/// The feedback state of one surface's items (keyed by `K`).
#[derive(Debug, Clone)]
pub struct Feedback<K> {
    /// Whether changes move (`enable-animations`).
    pub animate: bool,
    hover: Option<K>,
    focus: Option<K>,
    press: Option<K>,
    moves: Vec<Move<K>>,
    ghosts: Vec<(K, Instant)>,
}

impl<K> Default for Feedback<K> {
    fn default() -> Self {
        Self { animate: true, hover: None, focus: None, press: None, moves: Vec::new(), ghosts: Vec::new() }
    }
}

fn ease_out(p: f64) -> f64 {
    1.0 - (1.0 - p.clamp(0.0, 1.0)).powi(3)
}

/// Ease-out that overshoots a little before settling.
fn ease_out_back(p: f64) -> f64 {
    let q = p.clamp(0.0, 1.0) - 1.0;
    1.0 + 2.0 * q.powi(3) + q.powi(2)
}

fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

impl<K: Clone + PartialEq> Feedback<K> {
    /// Feedback that moves when `animate`.
    pub fn new(animate: bool) -> Self {
        Self { animate, ..Self::default() }
    }

    fn lit(&self, key: &K) -> bool {
        self.hover.as_ref() == Some(key) || self.focus.as_ref() == Some(key)
    }

    fn pressed(&self, key: &K) -> bool {
        self.press.as_ref() == Some(key)
    }

    fn target(&self, key: &K) -> (f64, f64) {
        let scale = if self.pressed(key) {
            PRESS_SCALE
        } else if self.lit(key) {
            HOVER_SCALE
        } else {
            1.0
        };
        (scale, if self.lit(key) { 1.0 } else { 0.0 })
    }

    fn elapsed(&self, at: Instant, now: Instant, secs: f64) -> f64 {
        if !self.animate {
            return 1.0;
        }
        (now.saturating_duration_since(at).as_secs_f64() / secs).min(1.0)
    }

    /// Scale and highlight of `key` at `now`.
    fn current(&self, key: &K, now: Instant) -> (f64, f64) {
        let (scale, highlight) = self.target(key);
        let Some(m) = self.moves.iter().find(|m| m.key == *key) else { return (scale, highlight) };
        let fade = if highlight > m.from_highlight { FADE_IN } else { FADE_OUT };
        let h = lerp(m.from_highlight, highlight, self.elapsed(m.at, now, fade));
        let s = if m.spring {
            lerp(m.from_scale, scale, ease_out_back(self.elapsed(m.at, now, SPRING)))
        } else {
            lerp(m.from_scale, scale, ease_out(self.elapsed(m.at, now, EASE)))
        };
        (s, h)
    }

    /// Change what `keys` should look like with `change`, moving each from
    /// where it is now.
    fn change(&mut self, keys: &[Option<K>], now: Instant, spring: bool, change: impl FnOnce(&mut Self)) {
        let before: Vec<(K, (f64, f64))> = keys.iter().flatten().map(|k| (k.clone(), self.current(k, now))).collect();
        change(self);
        for (key, (from_scale, from_highlight)) in before {
            self.moves.retain(|m| m.key != key);
            if (from_scale, from_highlight) != self.target(&key) {
                self.moves.push(Move { key, at: now, from_scale, from_highlight, spring });
            }
        }
    }

    /// The item under the pointer.
    pub fn hovered(&self) -> Option<&K> {
        self.hover.as_ref()
    }

    /// The pointer is now over `key` (or nothing). Leaving a pressed item
    /// unpresses it.
    pub fn set_hover(&mut self, key: Option<K>, now: Instant) {
        if self.hover == key {
            return;
        }
        let keys = [self.hover.clone(), key.clone(), self.press.clone()];
        self.change(&keys, now, false, |f| {
            if f.press.is_some() && f.press != key {
                f.press = None;
            }
            f.hover = key;
        });
    }

    /// The keyboard selection is now `key` (or nothing).
    pub fn set_focus(&mut self, key: Option<K>, now: Instant) {
        if self.focus == key {
            return;
        }
        let keys = [self.focus.clone(), key.clone()];
        self.change(&keys, now, false, |f| f.focus = key);
    }

    /// A button went down on `key`.
    pub fn press(&mut self, key: K, now: Instant) {
        let keys = [self.press.clone(), Some(key.clone())];
        self.change(&keys, now, false, |f| f.press = Some(key));
    }

    /// The button came up (or the press was given up): the icon springs
    /// back.
    pub fn release(&mut self, now: Instant) {
        let keys = [self.press.clone()];
        self.change(&keys, now, true, |f| f.press = None);
    }

    /// `key` launched or activated something: a growing, fading copy of
    /// its icon (only with animations).
    pub fn launch(&mut self, key: K, now: Instant) {
        if self.animate {
            self.ghosts.push((key, now));
        }
    }

    /// Forget everything (the items went away).
    pub fn reset(&mut self) {
        *self = Self { animate: self.animate, ..Self::default() };
    }

    /// How `key` looks at `now`.
    pub fn look(&self, key: &K, now: Instant) -> Look {
        let (scale, highlight) = self.current(key, now);
        Look { highlight, pressed: self.pressed(key), scale }
    }

    /// The launch copies still showing: item and progress (0 to 1).
    pub fn ghosts(&self, now: Instant) -> Vec<(K, f64)> {
        self.ghosts
            .iter()
            .filter_map(|(k, at)| {
                let p = now.saturating_duration_since(*at).as_secs_f64() / GHOST;
                (p < 1.0).then(|| (k.clone(), p))
            })
            .collect()
    }

    /// Whether anything is still changing (keep redrawing); drops what
    /// has finished.
    pub fn moving(&mut self, now: Instant) -> bool {
        let animate = self.animate;
        let done = |at: Instant, secs: f64| !animate || now.saturating_duration_since(at).as_secs_f64() >= secs;
        self.moves.retain(|m| !done(m.at, FADE_OUT.max(SPRING)));
        self.ghosts.retain(|(_, at)| !done(*at, GHOST));
        !self.moves.is_empty() || !self.ghosts.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    #[test]
    fn hover_fades_and_grows() {
        let t0 = Instant::now();
        let mut f: Feedback<u32> = Feedback::default();
        f.set_hover(Some(1), t0);
        let start = f.look(&1, t0);
        assert_eq!((start.highlight, start.scale), (0.0, 1.0));
        let mid = f.look(&1, at(t0, 60));
        assert!(mid.highlight > 0.3 && mid.highlight < 0.7, "{mid:?}");
        assert!(mid.scale > 1.0 && mid.scale < HOVER_SCALE);
        assert!(f.moving(at(t0, 60)));
        let end = f.look(&1, at(t0, 500));
        assert_eq!((end.highlight, end.scale), (1.0, HOVER_SCALE));
        assert!(!f.moving(at(t0, 500)));
        // Leaving fades out from where it was.
        f.set_hover(None, at(t0, 500));
        let leaving = f.look(&1, at(t0, 600));
        assert!(leaving.highlight > 0.0 && leaving.highlight < 1.0, "{leaving:?}");
        assert!(f.moving(at(t0, 600)));
        assert_eq!(f.look(&1, at(t0, 800)).highlight, 0.0);
        assert!(!f.moving(at(t0, 900)));
    }

    #[test]
    fn press_and_spring_back() {
        let t0 = Instant::now();
        let mut f: Feedback<u32> = Feedback::default();
        f.set_hover(Some(1), t0);
        f.press(1, at(t0, 300));
        let pressed = f.look(&1, at(t0, 500));
        assert!(pressed.pressed);
        assert!((pressed.scale - PRESS_SCALE).abs() < 1e-9);
        f.release(at(t0, 500));
        assert!(!f.look(&1, at(t0, 510)).pressed);
        // Overshoots the hovered size on the way back, then settles.
        let peak = (0..300).step_by(10).map(|ms| f.look(&1, at(t0, 500 + ms)).scale).fold(0.0, f64::max);
        assert!(peak > HOVER_SCALE, "springs past: {peak}");
        assert!((f.look(&1, at(t0, 900)).scale - HOVER_SCALE).abs() < 1e-9);
        assert!(!f.moving(at(t0, 900)));
    }

    #[test]
    fn leaving_a_pressed_item_unpresses_it() {
        let t0 = Instant::now();
        let mut f: Feedback<u32> = Feedback::default();
        f.set_hover(Some(1), t0);
        f.press(1, t0);
        f.set_hover(Some(2), at(t0, 50));
        assert!(!f.look(&1, at(t0, 60)).pressed);
        assert!(f.look(&2, at(t0, 60)).highlight > 0.0);
    }

    #[test]
    fn without_animations_everything_is_immediate() {
        let t0 = Instant::now();
        let mut f: Feedback<u32> = Feedback { animate: false, ..Feedback::default() };
        f.set_hover(Some(1), t0);
        assert_eq!(f.look(&1, t0), Look { highlight: 1.0, pressed: false, scale: HOVER_SCALE });
        f.press(1, t0);
        assert_eq!(f.look(&1, t0), Look { highlight: 1.0, pressed: true, scale: PRESS_SCALE });
        f.release(t0);
        f.launch(1, t0);
        assert!(f.ghosts(t0).is_empty(), "no launch copy");
        assert!(!f.moving(t0));
        assert_eq!(f.look(&1, t0).scale, HOVER_SCALE);
        f.set_hover(None, t0);
        assert_eq!(f.look(&1, t0), Look { highlight: 0.0, pressed: false, scale: 1.0 });
    }

    #[test]
    fn keyboard_selection_lights_like_hover() {
        let t0 = Instant::now();
        let mut f: Feedback<u32> = Feedback::default();
        f.set_focus(Some(3), t0);
        assert!(f.look(&3, at(t0, 60)).highlight > 0.0);
        assert_eq!(f.look(&3, at(t0, 400)), Look { highlight: 1.0, pressed: false, scale: HOVER_SCALE });
        f.set_focus(Some(4), at(t0, 400));
        assert!(f.look(&3, at(t0, 450)).highlight < 1.0, "the old one fades");
        assert_eq!(f.look(&3, at(t0, 800)).highlight, 0.0);
        assert_eq!(f.look(&4, at(t0, 800)).highlight, 1.0);
        // Hovered and selected at once: leaving with the pointer keeps it lit.
        f.set_hover(Some(4), at(t0, 800));
        f.set_hover(None, at(t0, 900));
        assert_eq!(f.look(&4, at(t0, 1200)).highlight, 1.0);
    }

    #[test]
    fn launch_ghosts_expire() {
        let t0 = Instant::now();
        let mut f: Feedback<&str> = Feedback::default();
        f.launch("app", t0);
        assert_eq!(f.ghosts(at(t0, 125)), [("app", 0.5)]);
        assert!(f.moving(at(t0, 125)));
        assert!(f.ghosts(at(t0, 300)).is_empty());
        assert!(!f.moving(at(t0, 300)));
    }
}
