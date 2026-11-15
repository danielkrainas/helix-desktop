//! Notices when the UI thread is busy too long: while it is, the shell
//! does not read its Wayland socket, and a compositor drops a client whose
//! socket stays full. The UI marks what it is doing with [`scope`]; work
//! that takes over [`SLOW`] is logged (a warning now and then, the details
//! at debug level), and a thread reports work still running after
//! [`STUCK`].

use std::borrow::Cow;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// One piece of UI work taking longer than this is logged.
pub const SLOW: Duration = Duration::from_millis(50);

/// Work still running after this is reported while it runs.
pub const STUCK: Duration = Duration::from_secs(1);

/// Warnings at most this often (the rest at debug level).
const WARN_EVERY: Duration = Duration::from_secs(10);

/// Steps shorter than this are not listed in the details.
const STEP: Duration = Duration::from_millis(5);

#[derive(Default)]
struct State {
    /// The thread watched (the UI's), once [`start`] ran.
    thread: Option<std::thread::ThreadId>,
    /// What runs now, outermost first, and since when.
    stack: Vec<(Cow<'static, str>, Instant)>,
    /// Nested steps of the current outermost work that took a while.
    steps: Vec<(Cow<'static, str>, Duration)>,
    /// The outermost work's start that was reported as stuck.
    stuck_reported: Option<Instant>,
    limiter: Limiter,
}

static STATE: Mutex<State> = Mutex::new(State { thread: None, stack: Vec::new(), steps: Vec::new(), stuck_reported: None, limiter: Limiter { last: None, quiet: 0 } });

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Watch the calling thread (the UI's) from now on.
pub fn start() {
    state().thread = Some(std::thread::current().id());
    let spawned = std::thread::Builder::new().name("ui-watchdog".into()).spawn(|| loop {
        std::thread::sleep(STUCK / 4);
        let mut s = state();
        let Some((what, since)) = s.stack.first().cloned() else { continue };
        if since.elapsed() >= STUCK && s.stuck_reported != Some(since) {
            s.stuck_reported = Some(since);
            let doing: Vec<&str> = s.stack.iter().map(|(w, _)| w.as_ref()).collect();
            let doing = doing.join(" > ");
            drop(s);
            tracing::warn!(what = %what, doing, ms = since.elapsed().as_millis() as u64, "UI thread still busy; the shell is not reading its Wayland socket");
        }
    });
    if let Err(err) = spawned {
        tracing::warn!(%err, "cannot start the UI watchdog");
    }
}

/// Marks UI work until dropped.
#[must_use]
pub struct Scope {
    active: bool,
}

/// `what` runs on the UI thread until the scope is dropped.
pub fn scope(what: &'static str) -> Scope {
    enter(Cow::Borrowed(what))
}

/// Like [`scope`], for a UI event: named by its variant (as `Debug`
/// prints it, up to its fields).
pub fn event_scope(event: &impl std::fmt::Debug) -> Scope {
    let mut name = NameOnly(String::from("event "));
    let _ = write!(name, "{event:?}");
    enter(Cow::Owned(name.0))
}

fn enter(what: Cow<'static, str>) -> Scope {
    let mut s = state();
    if s.thread != Some(std::thread::current().id()) {
        return Scope { active: false };
    }
    s.stack.push((what, Instant::now()));
    Scope { active: true }
}

impl Drop for Scope {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let mut s = state();
        let Some((what, since)) = s.stack.pop() else { return };
        let took = since.elapsed();
        if !s.stack.is_empty() {
            if took >= STEP {
                s.steps.push((what, took));
            }
            return;
        }
        let steps = std::mem::take(&mut s.steps);
        if took < SLOW {
            return;
        }
        let warn = s.limiter.warn(Instant::now());
        drop(s);
        let ms = took.as_millis() as u64;
        let details: Vec<String> = steps.iter().map(|(w, d)| format!("{w} {} ms", d.as_millis())).collect();
        let details = details.join(", ");
        match warn {
            Some(quiet) => tracing::warn!(what = %what, ms, quiet, "UI thread was busy; the shell did not read its Wayland socket meanwhile"),
            None => tracing::debug!(what = %what, ms, "UI thread was busy"),
        }
        if !details.is_empty() {
            tracing::debug!(what = %what, steps = %details, "where the UI thread's time went");
        }
    }
}

/// Lets a warning through at most every [`WARN_EVERY`], counting the
/// ones held back.
#[derive(Default)]
struct Limiter {
    last: Option<Instant>,
    quiet: u32,
}

impl Limiter {
    /// `Some(held back since the last one)` if a warning may be logged
    /// at `now`.
    fn warn(&mut self, now: Instant) -> Option<u32> {
        if self.last.is_some_and(|t| now.duration_since(t) < WARN_EVERY) {
            self.quiet += 1;
            return None;
        }
        self.last = Some(now);
        Some(std::mem::take(&mut self.quiet))
    }
}

/// Keeps the leading identifier written to it (a `Debug` variant name).
struct NameOnly(String);

impl std::fmt::Write for NameOnly {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        for c in s.chars() {
            if c.is_alphanumeric() || c == '_' {
                self.0.push(c);
            } else {
                // Stop formatting: the rest are fields.
                return Err(std::fmt::Error);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    #[allow(dead_code)]
    enum Event {
        Lock,
        Background { big: Vec<u8> },
        Overview(bool),
    }

    #[test]
    fn events_are_named_by_their_variant() {
        let name = |e: &Event| {
            let mut n = NameOnly(String::new());
            let _ = write!(n, "{e:?}");
            n.0
        };
        assert_eq!(name(&Event::Lock), "Lock");
        assert_eq!(name(&Event::Background { big: vec![0; 1000] }), "Background");
        assert_eq!(name(&Event::Overview(true)), "Overview");
    }

    #[test]
    fn warnings_are_rate_limited() {
        let mut limiter = Limiter::default();
        let t0 = Instant::now();
        assert_eq!(limiter.warn(t0), Some(0));
        assert_eq!(limiter.warn(t0 + Duration::from_secs(1)), None);
        assert_eq!(limiter.warn(t0 + Duration::from_secs(2)), None);
        assert_eq!(limiter.warn(t0 + WARN_EVERY + Duration::from_secs(1)), Some(2), "and how many were held back");
    }

    #[test]
    fn scopes_off_the_watched_thread_do_nothing() {
        // No `start` in this test thread: nothing is recorded.
        let outer = scope("outer");
        assert!(!outer.active);
        drop(outer);
        assert!(state().stack.is_empty());
    }
}
