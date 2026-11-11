//! Workspaces in the compositor (helix-desktop-spec
//! `contracts/gsettings/workspaces.md`): which workspace each window is on,
//! and hiding ("parking") windows that are not on the active one.

use helix_comp::workspaces::{self, Config};
use smithay::desktop::Window;
use smithay::utils::{Logical, Point};

use super::wm::is_override_redirect;
use super::Helix;

/// Per-window workspace state.
#[derive(Debug, Default)]
pub struct WorkspaceState {
    pub layout: workspaces::Workspaces,
    pub config: Config,
    /// The workspace of each window that has been assigned one.
    assigned: Vec<(Window, usize)>,
    /// Windows hidden because their workspace is not active, and where.
    parked: Vec<(Window, Point<i32, Logical>)>,
    /// Most recently focused first.
    history: Vec<Window>,
}

impl WorkspaceState {
    pub fn parked(&self) -> impl Iterator<Item = &Window> {
        self.parked.iter().map(|(w, _)| w)
    }

    pub fn is_parked(&self, window: &Window) -> bool {
        self.parked.iter().any(|(w, _)| w == window)
    }
}

impl Helix {
    /// The workspace `window` is on (windows not assigned yet are on the
    /// active one).
    pub fn workspace_of(&mut self, window: &Window) -> usize {
        let ws = &mut self.workspaces;
        match ws.assigned.iter().find(|(w, _)| w == window) {
            Some((_, n)) => *n,
            None => {
                let active = ws.layout.active;
                ws.assigned.push((window.clone(), active));
                active
            }
        }
    }

    /// Windows the workspaces account for: mapped, minimized and parked.
    fn workspace_windows(&self) -> Vec<Window> {
        self.space
            .elements()
            .chain(self.minimized.iter())
            .chain(self.workspaces.parked())
            .filter(|w| !is_override_redirect(w))
            .cloned()
            .collect()
    }

    /// Shown on every workspace: on a monitor other than the primary one,
    /// with `workspaces-only-on-primary`.
    fn on_all_workspaces(&self, window: &Window) -> bool {
        if !self.workspaces.config.only_on_primary {
            return false;
        }
        let outputs = self.space.outputs_for_element(window);
        let Some(output) = outputs.first() else { return false };
        self.primary_output.as_deref().is_some_and(|primary| output.name() != primary)
    }

    /// Re-count the workspaces after windows or settings changed.
    pub fn tidy_workspaces(&mut self) {
        let windows = self.workspace_windows();
        for w in &windows {
            self.workspace_of(w);
        }
        let ws = &mut self.workspaces;
        ws.assigned.retain(|(w, _)| windows.contains(w));
        let mut numbers: Vec<usize> = ws.assigned.iter().map(|(_, n)| *n).collect();
        let config = ws.config;
        ws.layout.tidy(&config, &mut numbers);
        for ((_, n), new) in ws.assigned.iter_mut().zip(numbers) {
            *n = new;
        }
        // A window whose workspace went away may now be on the active one.
        let active = ws.layout.active;
        let back: Vec<Window> = ws
            .parked
            .iter()
            .filter(|(w, _)| ws.assigned.iter().any(|(a, n)| a == w && *n == active))
            .map(|(w, _)| w.clone())
            .collect();
        for window in back {
            self.unpark(&window);
        }
    }

    /// New settings.
    pub fn apply_workspace_settings(&mut self) {
        let config = workspaces::read();
        if config != self.workspaces.config {
            tracing::info!(?config, "workspace settings");
            self.workspaces.config = config;
            self.tidy_workspaces();
        }
    }

    fn park(&mut self, window: &Window) {
        let location = self.space.element_location(window).unwrap_or_default();
        self.space.unmap_elem(window);
        self.workspaces.parked.push((window.clone(), location));
    }

    fn unpark(&mut self, window: &Window) {
        let Some(pos) = self.workspaces.parked.iter().position(|(w, _)| w == window) else { return };
        let (window, location) = self.workspaces.parked.remove(pos);
        self.space.map_element(window, location, false);
    }

    /// Make workspace `target` the active one.
    pub fn switch_workspace(&mut self, target: usize) {
        self.tidy_workspaces();
        if target >= self.workspaces.layout.count || target == self.workspaces.layout.active {
            return;
        }
        tracing::debug!(target, "switching workspace");
        let visible: Vec<Window> = self.space.elements().filter(|w| !is_override_redirect(w)).cloned().collect();
        for window in visible {
            if self.workspace_of(&window) != target && !self.on_all_workspaces(&window) {
                self.park(&window);
            }
        }
        self.workspaces.layout.active = target;
        let arriving: Vec<Window> = self
            .workspaces
            .parked()
            .filter(|w| self.workspaces.assigned.iter().any(|(a, n)| a == *w && *n == target))
            .cloned()
            .collect();
        for window in arriving {
            self.unpark(&window);
        }
        self.tidy_workspaces();
        let target = self.workspaces.layout.active;
        let next = self
            .workspaces
            .history
            .iter()
            .find(|w| self.space.elements().any(|e| e == *w))
            .cloned()
            .filter(|w| self.workspaces.assigned.iter().any(|(a, n)| a == w && *n == target) || self.on_all_workspaces(w));
        match next.or_else(|| self.top_window()) {
            Some(window) => self.focus_window(&window),
            None => self.unfocus_all(),
        }
        self.sync_window_list();
    }

    /// Put `window` on workspace `target` (hidden unless it is active).
    pub fn move_to_workspace(&mut self, window: &Window, target: usize) {
        self.tidy_workspaces();
        if target >= self.workspaces.layout.count {
            return;
        }
        self.workspace_of(window);
        if let Some(entry) = self.workspaces.assigned.iter_mut().find(|(w, _)| w == window) {
            entry.1 = target;
        }
        let visible = self.space.elements().any(|w| w == window);
        if target != self.workspaces.layout.active && visible && !self.on_all_workspaces(window) {
            self.park(window);
            match self.top_window() {
                Some(top) => self.focus_window(&top),
                None => self.unfocus_all(),
            }
        } else if target == self.workspaces.layout.active && self.workspaces.is_parked(window) {
            self.unpark(window);
        }
        self.tidy_workspaces();
        self.sync_window_list();
    }

    /// `window` is getting focus: switch to its workspace if it is hidden
    /// there, and remember the focus order.
    pub fn workspace_focus(&mut self, window: &Window) {
        let target = self.workspace_of(window);
        if target != self.workspaces.layout.active && !self.on_all_workspaces(window) {
            self.switch_workspace(target);
        }
        self.unpark(window);
        let history = &mut self.workspaces.history;
        history.retain(|w| w != window);
        history.insert(0, window.clone());
    }

    /// `window` is gone.
    pub fn workspace_forget(&mut self, window: &Window) {
        let ws = &mut self.workspaces;
        ws.assigned.retain(|(w, _)| w != window);
        ws.parked.retain(|(w, _)| w != window);
        ws.history.retain(|w| w != window);
        self.tidy_workspaces();
    }
}
