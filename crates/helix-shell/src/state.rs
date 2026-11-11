//! Shell state independent of D-Bus: overview and accelerator grabs.

use std::collections::BTreeMap;

use crate::accel::Accelerator;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grab {
    pub accelerator: Accelerator,
    pub mode_flags: u32,
    pub grab_flags: u32,
    pub owner: Option<String>,
}

#[derive(Debug, Default)]
pub struct ShellState {
    overview_active: bool,
    grabs: BTreeMap<u32, Grab>,
    next_action: u32,
}

impl ShellState {
    pub fn new() -> Self {
        Self {
            next_action: 1,
            ..Self::default()
        }
    }

    pub fn overview_active(&self) -> bool {
        self.overview_active
    }

    /// Returns true if the value changed.
    pub fn set_overview_active(&mut self, active: bool) -> bool {
        std::mem::replace(&mut self.overview_active, active) != active
    }

    /// Register a grab. Returns the action id, or 0 if the accelerator is
    /// invalid or already grabbed (per the contract).
    pub fn grab(&mut self, accelerator: &str, mode_flags: u32, grab_flags: u32, owner: Option<&str>) -> u32 {
        let Some(accelerator) = Accelerator::parse(accelerator) else {
            return 0;
        };
        if self.grabs.values().any(|g| g.accelerator == accelerator) {
            return 0;
        }
        let action = loop {
            let id = self.next_action;
            self.next_action = self.next_action.wrapping_add(1).max(1);
            if !self.grabs.contains_key(&id) {
                break id;
            }
        };
        self.grabs.insert(
            action,
            Grab {
                accelerator,
                mode_flags,
                grab_flags,
                owner: owner.map(str::to_owned),
            },
        );
        action
    }

    pub fn ungrab(&mut self, action: u32) -> bool {
        self.grabs.remove(&action).is_some()
    }

    pub fn grabs(&self) -> impl Iterator<Item = (&u32, &Grab)> {
        self.grabs.iter()
    }

    pub fn remove_owner(&mut self, owner: &str) -> usize {
        let before = self.grabs.len();
        self.grabs.retain(|_, g| g.owner.as_deref() != Some(owner));
        before - self.grabs.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grab_lifecycle() {
        let mut s = ShellState::new();
        let a = s.grab("<Super>a", 1, 0, Some(":1.1"));
        assert_ne!(a, 0);
        assert_eq!(s.grab("<super>A", 1, 0, Some(":1.2")), 0, "already taken");
        assert_eq!(s.grab("<Nope>a", 1, 0, None), 0, "invalid");
        let b = s.grab("<Super>b", 1, 0, Some(":1.2"));
        assert_ne!(a, b);
        assert!(s.ungrab(a));
        assert!(!s.ungrab(a));
        assert_ne!(s.grab("<Super>a", 1, 0, None), 0, "free again");
        assert_eq!(s.remove_owner(":1.2"), 1);
        assert_eq!(s.grabs().count(), 1);
    }

    #[test]
    fn overview_changes() {
        let mut s = ShellState::new();
        assert!(!s.overview_active());
        assert!(s.set_overview_active(true));
        assert!(!s.set_overview_active(true));
        assert!(s.overview_active());
    }
}
