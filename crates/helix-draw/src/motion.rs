//! Movement over time for surfaces that draw themselves frame by frame
//! (the lock and login screens): how much motion the user allows, values
//! easing from one place to another, the spinner and the shake.

use std::time::{Duration, Instant};

/// How much may move (helixdesktop/spec `a11y-interface.md`):
/// `org.gnome.desktop.interface enable-animations` and
/// `org.gnome.desktop.a11y.interface reduced-motion`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Motion {
    /// Animations on.
    #[default]
    Full,
    /// `reduced-motion` `reduce`: fades only; nothing slides, shakes or
    /// spins.
    Reduced,
    /// `enable-animations` off: changes are immediate.
    None,
}

impl Motion {
    /// From the two settings.
    pub fn from_settings(enable_animations: bool, reduce: bool) -> Self {
        match (enable_animations, reduce) {
            (false, _) => Self::None,
            (true, true) => Self::Reduced,
            (true, false) => Self::Full,
        }
    }

    /// Things may fade.
    pub fn fades(self) -> bool {
        self != Self::None
    }

    /// Things may move (slide, shake, spin).
    pub fn moves(self) -> bool {
        self == Self::Full
    }
}

/// Fast at first, slowing to a stop.
pub fn ease_out(t: f64) -> f64 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

/// Slow, fast, slow.
pub fn ease_in_out(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 { 4.0 * t * t * t } else { 1.0 - (-2.0 * t + 2.0).powi(3) / 2.0 }
}

/// A value going from `from` to `to` over `duration`, eased out.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tween {
    from: f64,
    to: f64,
    at: Instant,
    duration: Duration,
}

impl Tween {
    /// A value standing at `value`.
    pub fn still(value: f64) -> Self {
        Self { from: value, to: value, at: Instant::now(), duration: Duration::ZERO }
    }

    /// From `from` to `to`, starting now.
    pub fn new(from: f64, to: f64, duration: Duration) -> Self {
        Self { from, to, at: Instant::now(), duration }
    }

    /// Where it is at `now`.
    pub fn value_at(&self, now: Instant) -> f64 {
        if self.duration.is_zero() {
            return self.to;
        }
        let t = now.saturating_duration_since(self.at).as_secs_f64() / self.duration.as_secs_f64();
        self.from + (self.to - self.from) * ease_out(t)
    }

    pub fn value(&self) -> f64 {
        self.value_at(Instant::now())
    }

    /// Where it is going.
    pub fn target(&self) -> f64 {
        self.to
    }

    pub fn done_at(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.at) >= self.duration
    }

    pub fn done(&self) -> bool {
        self.done_at(Instant::now())
    }

    /// Go on to `to` from where it is now (over `duration`; at once when
    /// zero). Going where it already goes changes nothing.
    pub fn go(&mut self, to: f64, duration: Duration) {
        if to == self.to {
            return;
        }
        *self = Self::new(self.value(), to, duration);
    }
}

/// How long a [`shake`] lasts.
pub const SHAKE: Duration = Duration::from_millis(360);

/// The sideways offset (logical units) of a field shaken `elapsed` ago:
/// three swings dying out, 0 after [`SHAKE`].
pub fn shake(elapsed: Duration) -> f64 {
    let t = elapsed.as_secs_f64() / SHAKE.as_secs_f64();
    if t >= 1.0 {
        return 0.0;
    }
    10.0 * (1.0 - t).powi(2) * (t * 3.0 * std::f64::consts::TAU).sin()
}

/// One turn of a spinner.
pub const SPIN: Duration = Duration::from_millis(1000);

/// A spinner's angle (radians) `elapsed` after it started.
pub fn spin(elapsed: Duration) -> f64 {
    (elapsed.as_secs_f64() / SPIN.as_secs_f64()).fract() * std::f64::consts::TAU
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tweens() {
        let t = Tween::new(0.0, 1.0, Duration::from_millis(100));
        let start = t.at;
        assert_eq!(t.value_at(start), 0.0);
        let mid = t.value_at(start + Duration::from_millis(50));
        assert!(mid > 0.5 && mid < 1.0, "eased out: {mid}");
        assert_eq!(t.value_at(start + Duration::from_millis(100)), 1.0);
        assert!(t.done_at(start + Duration::from_millis(100)) && !t.done_at(start));
        // Immediate without a duration.
        let mut s = Tween::still(0.0);
        s.go(1.0, Duration::ZERO);
        assert_eq!(s.value(), 1.0);
        assert!(s.done());
    }

    #[test]
    fn shaking_dies_out() {
        assert_eq!(shake(Duration::ZERO), 0.0);
        let peak = (0..36).map(|ms| shake(Duration::from_millis(ms * 10)).abs()).fold(0.0, f64::max);
        assert!(peak > 6.0 && peak <= 10.0, "{peak}");
        assert_eq!(shake(SHAKE), 0.0);
        assert!(shake(Duration::from_millis(330)).abs() < 1.0);
    }

    #[test]
    fn settings() {
        assert_eq!(Motion::from_settings(false, true), Motion::None);
        assert_eq!(Motion::from_settings(true, true), Motion::Reduced);
        assert!(Motion::Reduced.fades() && !Motion::Reduced.moves());
        assert!(!Motion::None.fades());
    }
}
