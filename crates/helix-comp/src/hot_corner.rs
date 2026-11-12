//! The hot corner (helix-desktop-spec `contracts/wayland/README.md`,
//! "Overview"): the primary monitor's top-left corner toggles the overview.

/// A rectangle: x, y, width, height (logical).
pub type Rect = (i32, i32, i32, i32);

/// The square the pointer must leave before the corner works again.
pub const RESET: f64 = 32.0;

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
}

impl Default for HotCorner {
    fn default() -> Self {
        Self { armed: true }
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
