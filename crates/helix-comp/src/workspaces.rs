//! Workspaces (helix-desktop-spec `contracts/gsettings/workspaces.md`):
//! how many there are and which is active, given the settings and which
//! workspace each window is on. The compositor shows and hides windows.

use gio::prelude::*;

pub const MUTTER: &str = "org.gnome.mutter";
pub const WM_PREFERENCES: &str = "org.gnome.desktop.wm.preferences";
pub const SCHEMAS: [&str; 2] = [MUTTER, WM_PREFERENCES];

/// The settings that shape the workspaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    pub dynamic: bool,
    /// `num-workspaces` (used when not dynamic).
    pub count: usize,
    pub only_on_primary: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self { dynamic: false, count: 4, only_on_primary: false }
    }
}

fn settings_with(schema: &str, key: &str) -> Option<gio::Settings> {
    let source = gio::SettingsSchemaSource::default()?;
    source.lookup(schema, true)?.has_key(key).then(|| gio::Settings::new(schema))
}

/// The current settings (defaults where a schema or key is missing).
pub fn read() -> Config {
    let default = Config::default();
    Config {
        dynamic: settings_with(MUTTER, "dynamic-workspaces").map_or(default.dynamic, |s| s.boolean("dynamic-workspaces")),
        count: settings_with(WM_PREFERENCES, "num-workspaces")
            .map_or(default.count, |s| s.int("num-workspaces").clamp(1, 36) as usize),
        only_on_primary: settings_with(MUTTER, "workspaces-only-on-primary")
            .map_or(default.only_on_primary, |s| s.boolean("workspaces-only-on-primary")),
    }
}

/// How many workspaces there are and which one is active (0-based).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Workspaces {
    pub count: usize,
    pub active: usize,
}

impl Default for Workspaces {
    fn default() -> Self {
        Self { count: 1, active: 0 }
    }
}

impl Workspaces {
    /// Bring the workspaces in line with `config`, given the workspace of
    /// every window (`windows`, renumbered in place when workspaces go away).
    pub fn tidy(&mut self, config: &Config, windows: &mut [usize]) {
        if !config.dynamic {
            self.count = config.count.max(1);
            for w in windows.iter_mut() {
                *w = (*w).min(self.count - 1);
            }
            self.active = self.active.min(self.count - 1);
            return;
        }
        let occupied = |i: usize, windows: &[usize]| windows.contains(&i);
        let last = self.count.max(1) - 1;
        // Keep occupied workspaces, the active one and the last one.
        let kept: Vec<usize> = (0..=last.max(self.active))
            .filter(|&i| occupied(i, windows) || i == self.active || i == last)
            .collect();
        let new_index = |old: usize| kept.iter().position(|&k| k == old);
        for w in windows.iter_mut() {
            *w = new_index(*w).unwrap_or(kept.len() - 1);
        }
        self.active = new_index(self.active).unwrap_or(0);
        self.count = kept.len();
        // Exactly one empty workspace at the end.
        if occupied(self.count - 1, windows) {
            self.count += 1;
        }
    }

    /// The workspace `delta` away from the active one, if it exists.
    pub fn neighbour(&self, delta: isize) -> Option<usize> {
        let target = self.active as isize + delta;
        (0..self.count as isize).contains(&target).then_some(target as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATIC: Config = Config { dynamic: false, count: 4, only_on_primary: false };
    const DYNAMIC: Config = Config { dynamic: true, count: 4, only_on_primary: false };

    #[test]
    fn static_count_follows_the_setting() {
        let mut ws = Workspaces::default();
        let mut windows = [0, 2];
        ws.tidy(&STATIC, &mut windows);
        assert_eq!(ws, Workspaces { count: 4, active: 0 });
        ws.active = 3;
        let mut windows = [0, 3, 2];
        ws.tidy(&Config { count: 2, ..STATIC }, &mut windows);
        assert_eq!(ws, Workspaces { count: 2, active: 1 });
        assert_eq!(windows, [0, 1, 1], "windows on removed workspaces go to the last one");
    }

    #[test]
    fn dynamic_keeps_one_empty_workspace_at_the_end() {
        let mut ws = Workspaces::default();
        ws.tidy(&DYNAMIC, &mut []);
        assert_eq!(ws, Workspaces { count: 1, active: 0 }, "nothing open: one empty workspace");
        let mut windows = [0];
        ws.tidy(&DYNAMIC, &mut windows);
        assert_eq!(ws.count, 2, "an empty one is added after the occupied last one");
        // A window moved to the empty last workspace: another is added.
        let mut windows = [0, 1];
        ws.tidy(&DYNAMIC, &mut windows);
        assert_eq!(ws.count, 3);
        // Workspace 1 becomes empty while 0 is active: it goes away.
        let mut windows = [0, 0];
        ws.tidy(&DYNAMIC, &mut windows);
        assert_eq!(ws, Workspaces { count: 2, active: 0 });
    }

    #[test]
    fn dynamic_keeps_the_active_empty_workspace_and_renumbers() {
        // Windows on 0 and 2; 1 is empty and active.
        let mut ws = Workspaces { count: 4, active: 1 };
        let mut windows = [0, 2];
        ws.tidy(&DYNAMIC, &mut windows);
        assert_eq!(ws, Workspaces { count: 4, active: 1 });
        assert_eq!(windows, [0, 2]);
        // Switching away from it removes it; workspace 2 becomes 1.
        ws.active = 0;
        ws.tidy(&DYNAMIC, &mut windows);
        assert_eq!(ws, Workspaces { count: 3, active: 0 });
        assert_eq!(windows, [0, 1]);
        ws.active = 1;
        ws.tidy(&DYNAMIC, &mut windows);
        assert_eq!(ws, Workspaces { count: 3, active: 1 });
    }

    #[test]
    fn neighbours() {
        let ws = Workspaces { count: 3, active: 0 };
        assert_eq!(ws.neighbour(-1), None);
        assert_eq!(ws.neighbour(1), Some(1));
        assert_eq!(Workspaces { count: 3, active: 2 }.neighbour(1), None);
    }
}
