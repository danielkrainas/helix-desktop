//! Helix's own touch and touchpad gestures (helix-desktop-spec
//! `contracts/wayland/README.md`, "Gestures"): swipes with three or more
//! fingers and swipes in from a screen edge switch workspaces and open or
//! close the overview.
//!
//! Not yet tried on a real touchscreen: the recognizer is checked by unit
//! tests with made-up touch points.

use std::collections::HashMap;

/// How far (logical pixels) a swipe goes to count.
pub const SWIPE_DISTANCE: f64 = 100.0;
/// A touch starting this close (logical pixels) to a monitor edge may be
/// an edge swipe.
pub const EDGE_MARGIN: f64 = 16.0;
/// How far (logical pixels) an edge swipe goes inwards before it is
/// Helix's.
pub const EDGE_DISTANCE: f64 = 48.0;
/// Touch points needed for a multi-finger swipe.
pub const TOUCH_FINGERS: usize = 3;

/// What a finished gesture does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gesture {
    /// Go this many workspaces on (negative: back).
    Workspace(i32),
    /// Up: open the overview, or from it the app grid.
    OverviewUp,
    /// Down: close the overview.
    OverviewDown,
}

/// The gesture a swipe that moved `(dx, dy)` makes: sideways switches
/// workspace the way the content is pulled (fingers moving left show the
/// next one), up and down open and close the overview.
pub fn swipe(dx: f64, dy: f64) -> Option<Gesture> {
    if dx.abs() >= SWIPE_DISTANCE && dx.abs() > dy.abs() {
        return Some(Gesture::Workspace(if dx < 0.0 { 1 } else { -1 }));
    }
    if dy.abs() >= SWIPE_DISTANCE && dy.abs() > dx.abs() {
        return Some(if dy < 0.0 { Gesture::OverviewUp } else { Gesture::OverviewDown });
    }
    None
}

/// The workspace `gesture` leads to from `active` (of `count`), if any.
pub fn workspace_after(gesture: Gesture, active: usize, count: usize) -> Option<usize> {
    let Gesture::Workspace(step) = gesture else { return None };
    let target = active as i64 + i64::from(step);
    (0..count as i64).contains(&target).then_some(target as usize)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

/// The edge of `area` (x, y, w, h) a touch at `pos` starts at, if any.
pub fn edge_at(pos: (f64, f64), area: (f64, f64, f64, f64)) -> Option<Edge> {
    let (x, y, w, h) = area;
    let (px, py) = (pos.0 - x, pos.1 - y);
    if px < 0.0 || py < 0.0 || px > w || py > h {
        return None;
    }
    // The nearest edge, when within the margin.
    [(px, Edge::Left), (w - px, Edge::Right), (py, Edge::Top), (h - py, Edge::Bottom)]
        .into_iter()
        .filter(|(d, _)| *d <= EDGE_MARGIN)
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, e)| e)
}

/// What an edge swipe does.
pub fn edge_gesture(edge: Edge) -> Gesture {
    match edge {
        Edge::Bottom => Gesture::OverviewUp,
        Edge::Top => Gesture::OverviewDown,
        Edge::Left => Gesture::Workspace(-1),
        Edge::Right => Gesture::Workspace(1),
    }
}

/// How far a touch went inwards from `edge`, and sideways.
fn inwards(edge: Edge, d: (f64, f64)) -> (f64, f64) {
    match edge {
        Edge::Left => (d.0, d.1.abs()),
        Edge::Right => (-d.0, d.1.abs()),
        Edge::Top => (d.1, d.0.abs()),
        Edge::Bottom => (-d.1, d.0.abs()),
    }
}

/// What to do with a touch event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Not a gesture (yet): pass it to apps.
    Pass,
    /// A gesture starts: cancel the apps' touch points, keep this event.
    Claim,
    /// Part of Helix's gesture: keep it.
    Keep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Claimed {
    Fingers,
    Edge(Edge),
}

/// A touch point's start and latest position.
type Track = ((f64, f64), (f64, f64));

/// Follows touch points and recognizes gestures.
#[derive(Debug, Default)]
pub struct Recognizer {
    /// Each point's start and latest position.
    points: HashMap<i32, Track>,
    /// The single touch started at an edge.
    edge: Option<Edge>,
    claimed: Option<Claimed>,
    /// Movement of lifted points, summed, and how many.
    moved: (f64, f64, u32),
}

impl Recognizer {
    /// A touch point went down on a monitor of `area` (x, y, w, h).
    pub fn down(&mut self, slot: i32, pos: (f64, f64), area: (f64, f64, f64, f64)) -> Verdict {
        self.points.insert(slot, (pos, pos));
        if self.claimed.is_some() {
            return Verdict::Keep;
        }
        if self.points.len() == 1 {
            self.edge = edge_at(pos, area);
        } else {
            self.edge = None;
        }
        if self.points.len() >= TOUCH_FINGERS {
            self.claimed = Some(Claimed::Fingers);
            return Verdict::Claim;
        }
        Verdict::Pass
    }

    pub fn motion(&mut self, slot: i32, pos: (f64, f64)) -> Verdict {
        let Some(point) = self.points.get_mut(&slot) else { return Verdict::Pass };
        point.1 = pos;
        let start = point.0;
        if self.claimed.is_some() {
            return Verdict::Keep;
        }
        if let (Some(edge), 1) = (self.edge, self.points.len()) {
            let (inward, sideways) = inwards(edge, (pos.0 - start.0, pos.1 - start.1));
            if inward >= EDGE_DISTANCE && inward > sideways {
                self.claimed = Some(Claimed::Edge(edge));
                return Verdict::Claim;
            }
        }
        Verdict::Pass
    }

    /// A touch point lifted: the verdict, and the gesture once the last
    /// point of Helix's gesture lifts.
    pub fn up(&mut self, slot: i32) -> (Verdict, Option<Gesture>) {
        let Some((start, end)) = self.points.remove(&slot) else { return (Verdict::Pass, None) };
        let Some(claimed) = self.claimed else {
            if self.points.is_empty() {
                self.edge = None;
            }
            return (Verdict::Pass, None);
        };
        self.moved.0 += end.0 - start.0;
        self.moved.1 += end.1 - start.1;
        self.moved.2 += 1;
        if !self.points.is_empty() {
            return (Verdict::Keep, None);
        }
        let n = f64::from(self.moved.2.max(1));
        let gesture = match claimed {
            Claimed::Fingers => swipe(self.moved.0 / n, self.moved.1 / n),
            Claimed::Edge(edge) => Some(edge_gesture(edge)),
        };
        *self = Self::default();
        (Verdict::Keep, gesture)
    }

    /// The touches were cancelled: start over.
    pub fn cancel(&mut self) {
        *self = Self::default();
    }

    /// Whether Helix is in the middle of a gesture.
    pub fn active(&self) -> bool {
        self.claimed.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: (f64, f64, f64, f64) = (0.0, 0.0, 1280.0, 800.0);

    #[test]
    fn swipes() {
        assert_eq!(swipe(-150.0, 20.0), Some(Gesture::Workspace(1)), "left: the next workspace");
        assert_eq!(swipe(150.0, -20.0), Some(Gesture::Workspace(-1)));
        assert_eq!(swipe(30.0, -140.0), Some(Gesture::OverviewUp));
        assert_eq!(swipe(30.0, 140.0), Some(Gesture::OverviewDown));
        assert_eq!(swipe(90.0, 0.0), None, "too short");
        assert_eq!(swipe(120.0, 120.0), None, "neither way more");
        assert_eq!(workspace_after(Gesture::Workspace(1), 0, 2), Some(1));
        assert_eq!(workspace_after(Gesture::Workspace(1), 1, 2), None, "none past the last");
        assert_eq!(workspace_after(Gesture::Workspace(-1), 0, 2), None);
        assert_eq!(workspace_after(Gesture::OverviewUp, 0, 2), None);
    }

    #[test]
    fn edges() {
        assert_eq!(edge_at((5.0, 400.0), AREA), Some(Edge::Left));
        assert_eq!(edge_at((1275.0, 400.0), AREA), Some(Edge::Right));
        assert_eq!(edge_at((640.0, 790.0), AREA), Some(Edge::Bottom));
        assert_eq!(edge_at((640.0, 2.0), AREA), Some(Edge::Top));
        assert_eq!(edge_at((3.0, 10.0), AREA), Some(Edge::Left), "the nearer edge in a corner");
        assert_eq!(edge_at((640.0, 400.0), AREA), None);
        assert_eq!(edge_at((640.0, 790.0), (0.0, 0.0, 1280.0, 700.0)), None, "outside the monitor");
    }

    #[test]
    fn three_finger_swipe() {
        let mut r = Recognizer::default();
        assert_eq!(r.down(0, (400.0, 400.0), AREA), Verdict::Pass);
        assert_eq!(r.down(1, (450.0, 400.0), AREA), Verdict::Pass);
        assert_eq!(r.motion(0, (390.0, 400.0)), Verdict::Pass, "two fingers are the app's");
        assert_eq!(r.down(2, (500.0, 400.0), AREA), Verdict::Claim, "the third finger makes it Helix's");
        for (slot, x) in [(0, 390.0), (1, 450.0), (2, 500.0)] {
            assert_eq!(r.motion(slot, (x - 200.0, 410.0)), Verdict::Keep);
        }
        assert_eq!(r.up(0), (Verdict::Keep, None));
        assert_eq!(r.up(1), (Verdict::Keep, None));
        assert_eq!(r.up(2), (Verdict::Keep, Some(Gesture::Workspace(1))));
        assert!(!r.active());
        // Up with four fingers.
        for slot in 0..4 {
            r.down(slot, (300.0 + f64::from(slot) * 40.0, 500.0), AREA);
        }
        for slot in 0..4 {
            r.motion(slot, (300.0 + f64::from(slot) * 40.0, 330.0));
        }
        let ends: Vec<_> = (0..4).map(|s| r.up(s).1).collect();
        assert_eq!(ends, [None, None, None, Some(Gesture::OverviewUp)]);
    }

    #[test]
    fn edge_swipes() {
        let mut r = Recognizer::default();
        assert_eq!(r.down(0, (640.0, 795.0), AREA), Verdict::Pass);
        assert_eq!(r.motion(0, (642.0, 770.0)), Verdict::Pass, "not far enough yet");
        assert_eq!(r.motion(0, (645.0, 740.0)), Verdict::Claim);
        assert_eq!(r.motion(0, (650.0, 600.0)), Verdict::Keep);
        assert_eq!(r.up(0), (Verdict::Keep, Some(Gesture::OverviewUp)));
        // From the right edge: the next workspace.
        r.down(5, (1278.0, 300.0), AREA);
        assert_eq!(r.motion(5, (1200.0, 310.0)), Verdict::Claim);
        assert_eq!(r.up(5), (Verdict::Keep, Some(Gesture::Workspace(1))));
        // Along the edge, not inwards: the app's.
        r.down(1, (3.0, 300.0), AREA);
        assert_eq!(r.motion(1, (20.0, 500.0)), Verdict::Pass);
        assert_eq!(r.up(1), (Verdict::Pass, None));
        // Away from the edges: the app's.
        r.down(2, (600.0, 400.0), AREA);
        assert_eq!(r.motion(2, (600.0, 200.0)), Verdict::Pass);
        assert_eq!(r.up(2), (Verdict::Pass, None));
        // A second finger stops an edge swipe.
        r.down(3, (640.0, 795.0), AREA);
        r.down(4, (700.0, 500.0), AREA);
        assert_eq!(r.motion(3, (640.0, 600.0)), Verdict::Pass);
        r.up(3);
        r.up(4);
        assert!(!r.active());
    }

    #[test]
    fn cancelled() {
        let mut r = Recognizer::default();
        for slot in 0..3 {
            r.down(slot, (100.0, 100.0), AREA);
        }
        assert!(r.active());
        r.cancel();
        assert!(!r.active());
        assert_eq!(r.up(0), (Verdict::Pass, None));
    }
}
