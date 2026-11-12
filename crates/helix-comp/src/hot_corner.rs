//! The hot corner (helix-desktop-spec `contracts/wayland/README.md`,
//! "Overview"): the primary monitor's top-left corner toggles the overview
//! when the pointer pushes into it (or, for pointers that cannot push,
//! reaches it).

/// A rectangle: x, y, width, height (logical).
pub type Rect = (i32, i32, i32, i32);

/// The square the pointer must leave before the corner works again.
pub const RESET: f64 = 32.0;
/// How far (unaccelerated pixels) the pointer must push into the corner.
pub const PRESSURE: f64 = 100.0;
/// Pushes further apart than this (milliseconds) start over.
pub const PRESSURE_TIMEOUT_MS: u64 = 1000;

fn contains(r: Rect, x: f64, y: f64) -> bool {
    x >= f64::from(r.0) && y >= f64::from(r.1) && x < f64::from(r.0 + r.2) && y < f64::from(r.1 + r.3)
}

/// The primary monitor's top-left point, if no monitor lies to its left or
/// above it.
pub fn corner(primary: Rect, outputs: &[Rect]) -> Option<(f64, f64)> {
    let (x, y) = (f64::from(primary.0), f64::from(primary.1));
    let blocked = outputs.iter().any(|o| contains(*o, x - 1.0, y) || contains(*o, x, y - 1.0) || contains(*o, x - 1.0, y - 1.0));
    (!blocked).then_some((x, y))
}

#[derive(Debug, Clone, Copy)]
pub struct HotCorner {
    armed: bool,
    /// Pushed so far, and when last (ms).
    pushed: f64,
    last_push: Option<u64>,
}

impl Default for HotCorner {
    fn default() -> Self {
        Self { armed: true, pushed: 0.0, last_push: None }
    }
}

impl HotCorner {
    /// The pointer is at `pos`; true when the corner fires.
    pub fn moved(&mut self, corner: (f64, f64), pos: (f64, f64)) -> bool {
        let (dx, dy) = (pos.0 - corner.0, pos.1 - corner.1);
        if dx < 1.0 && dy < 1.0 {
            let fire = self.armed;
            self.armed = false;
            return fire;
        }
        if dx >= RESET || dy >= RESET {
            self.armed = true;
        }
        false
    }

    /// The pointer moved from `from` by `unaccel` (unaccelerated, which
    /// the screen edge does not stop) to `pos` at `time_ms`; true when it
    /// has pushed into the corner hard enough. Only motion beyond the
    /// corner counts, not the way there.
    pub fn pushed(&mut self, corner: (f64, f64), from: (f64, f64), pos: (f64, f64), unaccel: (f64, f64), time_ms: u64) -> bool {
        let (dx, dy) = (pos.0 - corner.0, pos.1 - corner.1);
        if dx >= RESET || dy >= RESET {
            self.armed = true;
        }
        if dx >= 1.0 || dy >= 1.0 {
            self.pushed = 0.0;
            self.last_push = None;
            return false;
        }
        let beyond = |from: f64, by: f64, edge: f64| (edge - (from.max(edge) + by)).max(0.0);
        let push = beyond(from.0, unaccel.0, corner.0) + beyond(from.1, unaccel.1, corner.1);
        if push <= 0.0 || !self.armed {
            return false;
        }
        if self.last_push.is_some_and(|t| time_ms.saturating_sub(t) > PRESSURE_TIMEOUT_MS) {
            self.pushed = 0.0;
        }
        self.last_push = Some(time_ms);
        self.pushed += push;
        if self.pushed >= PRESSURE {
            self.armed = false;
            self.pushed = 0.0;
            self.last_push = None;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corners() {
        let primary = (0, 0, 1920, 1080);
        assert_eq!(corner(primary, &[primary]), Some((0.0, 0.0)));
        // A monitor on the left: no corner there.
        assert_eq!(corner(primary, &[primary, (-1280, 0, 1280, 1024)]), None);
        // One on the right or below does not matter.
        assert_eq!(corner(primary, &[primary, (1920, 0, 1280, 1024), (0, 1080, 800, 600)]), Some((0.0, 0.0)));
    }

    #[test]
    fn pressure() {
        let mut h = HotCorner::default();
        let (c, at) = ((0.0, 0.0), (0.0, 0.0));
        assert!(!h.pushed(c, (60.0, 60.0), (5.0, 5.0), (-50.0, -50.0), 0), "not at the corner yet");
        assert!(!h.pushed(c, (300.0, 300.0), at, (-300.0, -300.0), 5), "arriving fast pushes nothing");
        assert!(!h.pushed(c, at, at, (-30.0, -20.0), 10), "50 of 100");
        assert!(!h.pushed(c, at, at, (10.0, 0.0), 20), "moving away pushes nothing");
        assert!(h.pushed(c, at, at, (-40.0, -10.0), 30));
        assert!(!h.pushed(c, at, at, (-200.0, 0.0), 40), "once until it leaves");
        h.pushed(c, at, (40.0, 0.0), (40.0, 0.0), 50);
        // Overshooting on arrival counts what goes past the corner.
        assert!(!h.pushed(c, (40.0, 0.0), at, (-100.0, 0.0), 60), "60 past");
        // Pushes a long time apart start over.
        assert!(!h.pushed(c, at, at, (-60.0, 0.0), 2000));
        assert!(h.pushed(c, at, at, (-60.0, 0.0), 2500));
    }

    #[test]
    fn fires_once_until_left() {
        let mut h = HotCorner::default();
        assert!(!h.moved((0.0, 0.0), (100.0, 100.0)));
        assert!(h.moved((0.0, 0.0), (0.0, 0.5)));
        assert!(!h.moved((0.0, 0.0), (0.0, 0.0)), "stays put: no repeat");
        assert!(!h.moved((0.0, 0.0), (10.0, 10.0)));
        assert!(!h.moved((0.0, 0.0), (0.0, 0.0)), "not far enough away yet");
        h.moved((0.0, 0.0), (40.0, 5.0));
        assert!(h.moved((0.0, 0.0), (0.0, 0.0)));
    }
}
