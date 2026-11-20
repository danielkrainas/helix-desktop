//! Kinetic (momentum) scrolling for touchpads (helixdesktop/spec
//! `contracts/dbus/org.freedesktop.impl.portal.md`, "FileChooser"):
//! finger scrolling is followed while the fingers move; when they lift
//! (the axis stop the compositor sends with the last finger event), the
//! list goes on at the speed it had and slows down, as GTK's kinetic
//! scrolling does (an exponential slow-down, `GtkKineticScrolling`'s
//! friction of 4 per second).
//!
//! A list keeps one [`Kinetic`], hands it each finger scroll
//! ([`Kinetic::track`]) and the stop ([`Kinetic::release`]), and while
//! it is [`Kinetic::active`] asks it every frame how far to go
//! ([`Kinetic::step`]). Anything else the user does (a wheel notch, a
//! click, a key, touching again) calls [`Kinetic::cancel`].

use std::collections::VecDeque;

/// How fast speed falls off: v(t) = v₀·e^(−FRICTION·t), so a fling goes
/// v₀ / FRICTION pixels in all.
pub const FRICTION: f64 = 4.0;
/// Below this speed (px/s) a fling does not start, and one going stops.
pub const MIN_START: f64 = 120.0;
pub const MIN_SPEED: f64 = 15.0;
/// The fastest fling (px/s).
pub const MAX_SPEED: f64 = 12_000.0;
/// Only the finger movement this recent counts for the speed (ms), and a
/// stop this long after the last movement means the fingers rested
/// before lifting: no fling.
const WINDOW_MS: u32 = 100;
const RESTED_MS: u32 = 60;
/// The time a single event's movement is taken to have lasted when there
/// is nothing before it (ms; touchpads send about every 8–12 ms).
const EVENT_MS: u32 = 10;
/// How often a fling moves the list (ms).
pub const FRAME_MS: u64 = 16;

/// One list's finger scrolling and fling.
#[derive(Debug, Clone, Default)]
pub struct Kinetic {
    /// Recent finger movements: (time in ms, pixels).
    samples: VecDeque<(u32, f64)>,
    /// The fling's speed (px/s); 0 when not flinging.
    velocity: f64,
}

impl Kinetic {
    /// The fingers moved the list by `delta` pixels at `time` (ms, the
    /// event's time).
    pub fn track(&mut self, time: u32, delta: f64) {
        self.velocity = 0.0;
        if self.samples.back().is_some_and(|(t, _)| time < *t || time - *t > WINDOW_MS) {
            self.samples.clear();
        }
        self.samples.push_back((time, delta));
        while self.samples.front().is_some_and(|(t, _)| time - *t > WINDOW_MS) {
            self.samples.pop_front();
        }
    }

    /// The speed the fingers had when they lifted at `time` (px/s): the
    /// movement of the last [`WINDOW_MS`] over the time it took.
    pub fn speed_at(&self, time: u32) -> f64 {
        let Some(&(last, _)) = self.samples.back() else { return 0.0 };
        if time.wrapping_sub(last) > RESTED_MS && time >= last {
            return 0.0;
        }
        let first = self.samples.front().map_or(last, |s| s.0);
        let moved: f64 = self.samples.iter().map(|s| s.1).sum();
        // The first movement took about as long as the others did.
        let n = self.samples.len() as u32;
        let each = if n > 1 { ((last - first) / (n - 1)).max(1) } else { EVENT_MS };
        let span = f64::from(last - first + each) / 1000.0;
        (moved / span).clamp(-MAX_SPEED, MAX_SPEED)
    }

    /// The fingers lifted at `time`: whether a fling starts.
    pub fn release(&mut self, time: u32) -> bool {
        let v = self.speed_at(time);
        self.samples.clear();
        self.velocity = if v.abs() >= MIN_START { v } else { 0.0 };
        self.active()
    }

    /// Start a fling at `velocity` px/s (as [`Self::release`] would).
    pub fn fling(&mut self, velocity: f64) {
        self.samples.clear();
        self.velocity = velocity.clamp(-MAX_SPEED, MAX_SPEED);
    }

    /// Stop any fling.
    pub fn cancel(&mut self) {
        self.samples.clear();
        self.velocity = 0.0;
    }

    pub fn active(&self) -> bool {
        self.velocity != 0.0
    }

    pub fn velocity(&self) -> f64 {
        self.velocity
    }

    /// How far the fling goes in the next `dt` seconds (pixels), slowing
    /// down; `None` once it has stopped.
    pub fn step(&mut self, dt: f64) -> Option<f64> {
        if !self.active() {
            return None;
        }
        let decay = (-FRICTION * dt.max(0.0)).exp();
        let moved = self.velocity / FRICTION * (1.0 - decay);
        self.velocity *= decay;
        if self.velocity.abs() < MIN_SPEED {
            self.velocity = 0.0;
        }
        Some(moved)
    }

    /// How far a fling at `velocity` goes in all (pixels).
    pub fn distance(velocity: f64) -> f64 {
        velocity / FRICTION
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speed_from_the_last_movements() {
        let mut k = Kinetic::default();
        // 30 px every 10 ms: 3000 px/s.
        for i in 0..8 {
            k.track(1000 + i * 10, 30.0);
        }
        assert!((k.speed_at(1075) - 3000.0).abs() < 1.0, "{}", k.speed_at(1075));
        // Older movements drop out of the window.
        let mut slow_then_fast = Kinetic::default();
        slow_then_fast.track(0, 1.0);
        slow_then_fast.track(500, 1.0);
        for i in 1..=5 {
            slow_then_fast.track(500 + i * 10, 20.0);
        }
        assert!((slow_then_fast.speed_at(550) - 101.0 / 0.06).abs() < 1.0, "{}", slow_then_fast.speed_at(550));
        // Fingers that rested before lifting: no fling.
        assert_eq!(k.speed_at(1070 + RESTED_MS + 1), 0.0);
        assert!(!k.clone().release(1070 + RESTED_MS + 1));
        // Upwards too, and capped.
        let mut up = Kinetic::default();
        up.track(0, -500.0);
        up.track(1, -500.0);
        assert_eq!(up.speed_at(1), -MAX_SPEED);
    }

    #[test]
    fn slows_down_and_stops() {
        let mut k = Kinetic::default();
        for i in 0..5 {
            k.track(i * 10, 20.0);
        }
        assert!(k.release(45));
        let v0 = k.velocity();
        assert!((v0 - 2000.0).abs() < 1.0);
        // Frame by frame, it covers v₀ / FRICTION (less what is left below
        // the stopping speed), never turning back, ever slower.
        let mut gone = 0.0;
        let mut last = f64::MAX;
        let mut frames = 0;
        while let Some(d) = k.step(FRAME_MS as f64 / 1000.0) {
            assert!(d > 0.0 && d <= last, "frame {frames}: {d} after {last}");
            last = d;
            gone += d;
            frames += 1;
        }
        let all = Kinetic::distance(v0);
        assert!(gone <= all && gone > all - MIN_SPEED / FRICTION - 1.0, "{gone} of {all}");
        assert!((60..200).contains(&frames), "about a second and a half: {frames} frames");
        assert!(!k.active() && k.step(0.016).is_none());
        // The same distance whatever the frame rate.
        let (mut a, mut b) = (Kinetic::default(), Kinetic::default());
        a.fling(1500.0);
        b.fling(1500.0);
        let (mut da, mut db) = (0.0, 0.0);
        for _ in 0..10 {
            da += a.step(0.032).unwrap();
        }
        for _ in 0..20 {
            db += b.step(0.016).unwrap();
        }
        assert!((da - db).abs() < 1e-6, "{da} vs {db}");
        // Slow movements do not fling; a new touch or a click stops one.
        let mut slow = Kinetic::default();
        slow.track(0, 0.5);
        slow.track(10, 0.5);
        assert!(!slow.release(12));
        let mut c = Kinetic::default();
        c.fling(-3000.0);
        assert!(c.step(0.016).unwrap() < 0.0, "upwards");
        c.track(500, 4.0);
        assert!(!c.active(), "fingers down again");
        c.fling(3000.0);
        c.cancel();
        assert!(!c.active());
    }
}
