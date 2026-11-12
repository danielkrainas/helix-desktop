//! Animation timing and shapes (helix-desktop-spec
//! `contracts/wayland/README.md`, "Animations"): what windows, shell
//! surfaces and workspaces look like part way through opening, closing,
//! minimizing and switching.

use std::time::Duration;

/// Which animations run (`enable-animations`, `reduced-motion`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    /// No animations.
    Off,
    /// Fades only: nothing moves or changes size.
    Reduced,
    Full,
}

impl Motion {
    pub fn from_settings(enable_animations: bool, reduced_motion: &str) -> Self {
        match (enable_animations, reduced_motion) {
            (false, _) => Motion::Off,
            (true, "reduce") => Motion::Reduced,
            _ => Motion::Full,
        }
    }
}

/// What is animated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A window appears (mapped or brought back from minimized).
    Open,
    /// A window goes away.
    Close,
    /// A window is minimized (towards the dock).
    Minimize,
    /// A shell surface (overview, menus, dialogs) appears or goes away.
    LayerIn,
    LayerOut,
    /// Workspace switch.
    Slide,
}

/// Whether a shell surface fades in and out: everything but the shell's
/// always-there parts (panel, dock, on-screen keyboard, backgrounds).
pub fn layer_animates(namespace: &str) -> bool {
    !matches!(namespace, "helix-panel" | "helix-dock" | "helix-dock-edge" | "helix-osk") && !namespace.starts_with("helix-background")
}

/// `HELIX_ANIMATION_SLOWDOWN`: a factor (1–100) every duration is
/// multiplied by, for testing.
fn slowdown() -> f64 {
    static FACTOR: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
    *FACTOR.get_or_init(|| std::env::var("HELIX_ANIMATION_SLOWDOWN").ok().and_then(|v| v.parse().ok()).map_or(1.0, |f: f64| f.clamp(1.0, 100.0)))
}

pub fn duration(kind: Kind) -> Duration {
    base_duration(kind).mul_f64(slowdown())
}

fn base_duration(kind: Kind) -> Duration {
    Duration::from_millis(match kind {
        Kind::Open => 200,
        Kind::Close => 150,
        Kind::Minimize => 250,
        Kind::LayerIn | Kind::LayerOut => 150,
        Kind::Slide => 250,
    })
}

/// Progress (0–1) after `elapsed`; `None` once finished.
pub fn progress(kind: Kind, elapsed: Duration) -> Option<f64> {
    let total = duration(kind).as_secs_f64();
    let t = elapsed.as_secs_f64() / total;
    (t < 1.0).then_some(t.max(0.0))
}

/// Cubic ease-out: fast start, gentle end.
pub fn ease_out(t: f64) -> f64 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

/// How something looks part way through: its opacity, its scale (about
/// its center, or towards `target` for minimizing) and a sideways shift as
/// a fraction of the monitor's width.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Look {
    pub alpha: f64,
    pub scale: f64,
    /// 0: in place; 1: all the way to the target point.
    pub towards_target: f64,
    pub shift: f64,
}

impl Look {
    pub const STILL: Look = Look { alpha: 1.0, scale: 1.0, towards_target: 0.0, shift: 0.0 };
}

/// The look of `kind` at progress `t` (0–1). For `Slide`, `direction` is
/// +1 when the new workspace comes from the right and `leaving` tells the
/// old workspace's windows from the new one's.
pub fn look(kind: Kind, t: f64, motion: Motion, direction: i32, leaving: bool) -> Look {
    let e = ease_out(t);
    let moves = motion == Motion::Full;
    let mut look = Look::STILL;
    match kind {
        Kind::Open => {
            look.alpha = e;
            if moves {
                look.scale = 0.9 + 0.1 * e;
            }
        }
        Kind::Close => {
            look.alpha = 1.0 - e;
            if moves {
                look.scale = 1.0 - 0.1 * e;
            }
        }
        Kind::Minimize => {
            look.alpha = 1.0 - e;
            if moves {
                look.scale = 1.0 - 0.8 * e;
                look.towards_target = e;
            }
        }
        Kind::LayerIn => look.alpha = e,
        Kind::LayerOut => look.alpha = 1.0 - e,
        Kind::Slide => {
            let d = f64::from(direction.signum());
            if moves {
                // Old windows leave the other way; new ones arrive.
                look.shift = if leaving { -d * e } else { d * (1.0 - e) };
            } else {
                look.alpha = if leaving { 1.0 - e } else { e };
            }
        }
    }
    look
}

/// Where a point of a rectangle `(x, y, w, h)` ends up under `look`: the
/// rectangle scaled about its center, moved `towards_target` of the way
/// from its center to `target`, and shifted by `shift × width_of_monitor`.
pub fn place(rect: (f64, f64, f64, f64), look: &Look, target: (f64, f64), monitor_width: f64) -> (f64, f64, f64, f64) {
    let (x, y, w, h) = rect;
    let (cx, cy) = (x + w / 2.0, y + h / 2.0);
    let (tx, ty) = (cx + (target.0 - cx) * look.towards_target, cy + (target.1 - cy) * look.towards_target);
    let (nw, nh) = (w * look.scale, h * look.scale);
    (tx - nw / 2.0 + look.shift * monitor_width, ty - nh / 2.0, nw, nh)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings() {
        assert_eq!(Motion::from_settings(true, "no-preference"), Motion::Full);
        assert_eq!(Motion::from_settings(true, "reduce"), Motion::Reduced);
        assert_eq!(Motion::from_settings(false, "no-preference"), Motion::Off);
    }

    #[test]
    fn layers() {
        assert!(layer_animates("helix-overview") && layer_animates("helix-window-menu") && layer_animates("some-app"));
        assert!(!layer_animates("helix-panel") && !layer_animates("helix-dock") && !layer_animates("helix-background-1"));
    }

    #[test]
    fn timing() {
        assert_eq!(progress(Kind::Open, Duration::ZERO), Some(0.0));
        assert_eq!(progress(Kind::Open, Duration::from_millis(100)), Some(0.5));
        assert_eq!(progress(Kind::Open, Duration::from_millis(200)), None);
        assert_eq!((ease_out(0.0), ease_out(1.0)), (0.0, 1.0));
        assert!(ease_out(0.5) > 0.5, "eases out");
    }

    #[test]
    fn looks() {
        let start = look(Kind::Open, 0.0, Motion::Full, 0, false);
        assert_eq!((start.alpha, start.scale), (0.0, 0.9));
        assert_eq!(look(Kind::Open, 1.0, Motion::Full, 0, false), Look::STILL);
        let reduced = look(Kind::Open, 0.0, Motion::Reduced, 0, false);
        assert_eq!((reduced.alpha, reduced.scale), (0.0, 1.0), "fade only");
        let min = look(Kind::Minimize, 1.0, Motion::Full, 0, false);
        assert!((min.scale - 0.2).abs() < 1e-9 && min.alpha == 0.0 && min.towards_target == 1.0);
        // Switching to the workspace on the right: old windows go left,
        // new ones come in from the right.
        assert_eq!(look(Kind::Slide, 0.0, Motion::Full, 1, false).shift, 1.0);
        assert_eq!(look(Kind::Slide, 1.0, Motion::Full, 1, true).shift, -1.0);
        assert_eq!(look(Kind::Slide, 1.0, Motion::Full, 1, false).shift, 0.0);
        let faded = look(Kind::Slide, 0.0, Motion::Reduced, 1, false);
        assert_eq!((faded.shift, faded.alpha), (0.0, 0.0), "reduced: a crossfade");
    }

    #[test]
    fn placing() {
        let rect = (100.0, 100.0, 200.0, 100.0);
        assert_eq!(place(rect, &Look::STILL, (0.0, 0.0), 1000.0), rect);
        let half = Look { scale: 0.5, ..Look::STILL };
        assert_eq!(place(rect, &half, (0.0, 0.0), 1000.0), (150.0, 125.0, 100.0, 50.0), "about the center");
        let gone = Look { scale: 0.2, towards_target: 1.0, ..Look::STILL };
        let (x, y, w, h) = place(rect, &gone, (640.0, 790.0), 1000.0);
        assert_eq!((x + w / 2.0, y + h / 2.0), (640.0, 790.0), "centered on the target");
        let shifted = Look { shift: -1.0, ..Look::STILL };
        assert_eq!(place(rect, &shifted, (0.0, 0.0), 1000.0).0, -900.0);
    }
}
