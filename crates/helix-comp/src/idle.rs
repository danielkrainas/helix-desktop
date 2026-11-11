//! IdleMonitor rules, independent of D-Bus and of the compositor.
//!
//! Behavior: `helix-desktop-spec/contracts/dbus/org.gnome.Mutter.IdleMonitor.md`.
//! Times are milliseconds on a monotonic clock supplied by the caller.

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    /// Fires once each time idle time reaches `interval`.
    Idle { interval: u64, fired: bool },
    /// Fires once at the next activity, then is removed.
    UserActive,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Watch {
    kind: Kind,
    owner: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdleError {
    ZeroInterval,
    UnknownWatch(u32),
}

impl std::fmt::Display for IdleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroInterval => write!(f, "idle watch interval must be greater than zero"),
            Self::UnknownWatch(id) => write!(f, "no watch with id {id}"),
        }
    }
}

impl std::error::Error for IdleError {}

#[derive(Debug)]
pub struct IdleCore {
    last_activity: u64,
    watches: BTreeMap<u32, Watch>,
    next_id: u32,
}

impl IdleCore {
    pub fn new(now: u64) -> Self {
        Self {
            last_activity: now,
            watches: BTreeMap::new(),
            next_id: 1,
        }
    }

    pub fn idletime(&self, now: u64) -> u64 {
        now.saturating_sub(self.last_activity)
    }

    fn allocate_id(&mut self) -> u32 {
        loop {
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1).max(1);
            if !self.watches.contains_key(&id) {
                return id;
            }
        }
    }

    pub fn add_idle_watch(&mut self, interval: u64, owner: Option<&str>) -> Result<u32, IdleError> {
        if interval == 0 {
            return Err(IdleError::ZeroInterval);
        }
        let id = self.allocate_id();
        self.watches.insert(
            id,
            Watch {
                kind: Kind::Idle {
                    interval,
                    fired: false,
                },
                owner: owner.map(str::to_owned),
            },
        );
        Ok(id)
    }

    pub fn add_user_active_watch(&mut self, owner: Option<&str>) -> u32 {
        let id = self.allocate_id();
        self.watches.insert(
            id,
            Watch {
                kind: Kind::UserActive,
                owner: owner.map(str::to_owned),
            },
        );
        id
    }

    pub fn remove_watch(&mut self, id: u32) -> Result<(), IdleError> {
        self.watches
            .remove(&id)
            .map(|_| ())
            .ok_or(IdleError::UnknownWatch(id))
    }

    pub fn remove_owner(&mut self, owner: &str) -> usize {
        let before = self.watches.len();
        self.watches.retain(|_, w| w.owner.as_deref() != Some(owner));
        before - self.watches.len()
    }

    /// User activity (or `ResetIdletime`). Returns the user-active watches
    /// that fire; they are removed. Idle watches are re-armed.
    pub fn activity(&mut self, now: u64) -> Vec<u32> {
        self.last_activity = now;
        let mut fired = Vec::new();
        self.watches.retain(|id, w| match &mut w.kind {
            Kind::UserActive => {
                fired.push(*id);
                false
            }
            Kind::Idle { fired, .. } => {
                *fired = false;
                true
            }
        });
        fired
    }

    /// Idle watches whose interval has been reached and that have not fired
    /// since the last activity. They are marked as fired.
    pub fn poll(&mut self, now: u64) -> Vec<u32> {
        let idle = self.idletime(now);
        let mut out = Vec::new();
        for (id, w) in &mut self.watches {
            if let Kind::Idle { interval, fired } = &mut w.kind {
                if !*fired && idle >= *interval {
                    *fired = true;
                    out.push(*id);
                }
            }
        }
        out
    }

    /// Milliseconds until the next idle watch is due, if any is pending.
    pub fn next_due(&self, now: u64) -> Option<u64> {
        let idle = self.idletime(now);
        self.watches
            .values()
            .filter_map(|w| match w.kind {
                Kind::Idle {
                    interval,
                    fired: false,
                } => Some(interval.saturating_sub(idle)),
                _ => None,
            })
            .min()
    }

    pub fn watch_count(&self) -> usize {
        self.watches.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_watch_fires_once_per_idle_period() {
        let mut m = IdleCore::new(0);
        let w = m.add_idle_watch(1000, None).unwrap();
        assert_eq!(m.next_due(0), Some(1000));
        assert!(m.poll(999).is_empty());
        assert_eq!(m.poll(1000), vec![w]);
        assert!(m.poll(5000).is_empty(), "fires only once while idle");
        assert_eq!(m.next_due(5000), None);

        m.activity(6000);
        assert_eq!(m.idletime(6500), 500);
        assert_eq!(m.next_due(6500), Some(500));
        assert_eq!(m.poll(7000), vec![w], "re-armed by activity");
    }

    #[test]
    fn idle_watch_added_late_fires_immediately() {
        let mut m = IdleCore::new(0);
        let w = m.add_idle_watch(100, None).unwrap();
        assert_eq!(m.next_due(500), Some(0));
        assert_eq!(m.poll(500), vec![w]);
    }

    #[test]
    fn user_active_watch_is_one_shot() {
        let mut m = IdleCore::new(0);
        let idle = m.add_idle_watch(10, None).unwrap();
        let active = m.add_user_active_watch(None);
        assert_ne!(idle, active);
        assert!(m.poll(5).is_empty());
        assert_eq!(m.activity(20), vec![active]);
        assert!(m.activity(30).is_empty());
        assert_eq!(m.watch_count(), 1);
    }

    #[test]
    fn remove_and_errors() {
        let mut m = IdleCore::new(0);
        assert_eq!(m.add_idle_watch(0, None), Err(IdleError::ZeroInterval));
        let w = m.add_idle_watch(10, None).unwrap();
        m.remove_watch(w).unwrap();
        assert_eq!(m.remove_watch(w), Err(IdleError::UnknownWatch(w)));
        assert!(m.poll(100).is_empty());
    }

    #[test]
    fn owner_cleanup() {
        let mut m = IdleCore::new(0);
        m.add_idle_watch(10, Some(":1.1")).unwrap();
        m.add_user_active_watch(Some(":1.1"));
        let kept = m.add_idle_watch(10, Some(":1.2")).unwrap();
        assert_eq!(m.remove_owner(":1.1"), 2);
        assert_eq!(m.poll(10), vec![kept]);
    }
}
