//! Keeps the shell running: the compositor starts it (on a privileged
//! socket) and restarts it when it exits, with backoff, giving up if it
//! keeps crashing.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Restarts counted within this window...
pub const WINDOW: Duration = Duration::from_secs(60);
/// ...may not exceed this, or supervision gives up.
pub const MAX_RESTARTS: usize = 5;
const FIRST_DELAY: Duration = Duration::from_millis(500);
const MAX_DELAY: Duration = Duration::from_secs(8);

/// Restart policy, independent of processes so it can be tested.
#[derive(Debug, Default)]
pub struct RestartPolicy {
    recent: VecDeque<Instant>,
}

impl RestartPolicy {
    /// The shell exited at `now`: how long to wait before restarting it, or
    /// `None` to give up.
    pub fn next_delay(&mut self, now: Instant) -> Option<Duration> {
        while self.recent.front().is_some_and(|t| now.duration_since(*t) > WINDOW) {
            self.recent.pop_front();
        }
        if self.recent.len() >= MAX_RESTARTS {
            return None;
        }
        self.recent.push_back(now);
        let exponent = (self.recent.len() - 1).min(5) as u32;
        Some((FIRST_DELAY * 2u32.pow(exponent)).min(MAX_DELAY))
    }
}

/// The supervised shell process.
#[derive(Debug)]
pub struct Shell {
    pub program: String,
    pub pid: Option<u32>,
    pub policy: RestartPolicy,
}

impl Shell {
    pub fn new(program: String) -> Self {
        Self {
            program,
            pid: None,
            policy: RestartPolicy::default(),
        }
    }

    /// Ask the running shell to exit (on compositor shutdown).
    pub fn terminate(&mut self) {
        if let Some(pid) = self.pid.take() {
            // SAFETY: plain kill(2) on our child's pid.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        let mut p = RestartPolicy::default();
        let t = Instant::now();
        let delays: Vec<_> = (0..5).map(|i| p.next_delay(t + Duration::from_secs(i)).unwrap()).collect();
        assert_eq!(
            delays,
            [500, 1000, 2000, 4000, 8000].map(Duration::from_millis).to_vec()
        );
    }

    #[test]
    fn gives_up_after_too_many_crashes() {
        let mut p = RestartPolicy::default();
        let t = Instant::now();
        for i in 0..MAX_RESTARTS {
            assert!(p.next_delay(t + Duration::from_secs(i as u64)).is_some());
        }
        assert_eq!(p.next_delay(t + Duration::from_secs(10)), None);
    }

    #[test]
    fn a_long_healthy_run_resets_the_count() {
        let mut p = RestartPolicy::default();
        let t = Instant::now();
        for i in 0..MAX_RESTARTS {
            p.next_delay(t + Duration::from_secs(i as u64));
        }
        // Long after the last crash: allowed again, back to the short delay.
        let later = t + WINDOW + Duration::from_secs(30);
        assert_eq!(p.next_delay(later), Some(FIRST_DELAY));
    }
}
