//! Open windows as the compositor reports them (`helix_window_list_v1`),
//! most recently focused first, and the switchers' entries and selection
//! (helix-desktop-spec `contracts/wayland/README.md`, "Window switching").

use gio::prelude::*;

const KEYBINDINGS: &str = "org.gnome.desktop.wm.keybindings";
const MUTTER_KEYBINDINGS: &str = "org.gnome.mutter.keybindings";

/// A window's workspace when it is on every workspace.
pub const ALL_WORKSPACES: u32 = u32::MAX;

/// Window-management keys handled without a switcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WmKey {
    /// `cycle-windows` (`group`: false) or `cycle-group`.
    Cycle { group: bool, backward: bool },
    /// `switch-panels`.
    Panels { backward: bool },
    /// `toggle-on-all-workspaces`.
    ToggleOnAllWorkspaces,
    /// A key acting on the focused window through the compositor (window
    /// menu and window keys).
    Window(WindowAction),
}

pub use helix_protocols::client::helix_window_list_v1::Action as WindowAction;

/// `org.gnome.desktop.wm.keybindings` keys the compositor carries out.
pub const WINDOW_KEYS: [(&str, WindowAction); 30] = [
    ("activate-window-menu", WindowAction::ShowMenu),
    ("close", WindowAction::Close),
    ("minimize", WindowAction::Minimize),
    ("maximize", WindowAction::Maximize),
    ("unmaximize", WindowAction::Unmaximize),
    ("toggle-maximized", WindowAction::ToggleMaximize),
    ("maximize-horizontally", WindowAction::MaximizeHorizontally),
    ("maximize-vertically", WindowAction::MaximizeVertically),
    ("toggle-fullscreen", WindowAction::ToggleFullscreen),
    ("toggle-above", WindowAction::ToggleAbove),
    ("always-on-top", WindowAction::ToggleAbove),
    ("begin-move", WindowAction::BeginMove),
    ("begin-resize", WindowAction::BeginResize),
    ("move-to-monitor-left", WindowAction::MoveToMonitorLeft),
    ("move-to-monitor-right", WindowAction::MoveToMonitorRight),
    ("move-to-monitor-up", WindowAction::MoveToMonitorUp),
    ("move-to-monitor-down", WindowAction::MoveToMonitorDown),
    ("move-to-center", WindowAction::MoveToCenter),
    ("move-to-side-n", WindowAction::MoveToSideN),
    ("move-to-side-s", WindowAction::MoveToSideS),
    ("move-to-side-e", WindowAction::MoveToSideE),
    ("move-to-side-w", WindowAction::MoveToSideW),
    ("move-to-corner-ne", WindowAction::MoveToCornerNe),
    ("move-to-corner-nw", WindowAction::MoveToCornerNw),
    ("move-to-corner-se", WindowAction::MoveToCornerSe),
    ("move-to-corner-sw", WindowAction::MoveToCornerSw),
    ("raise", WindowAction::Raise),
    ("lower", WindowAction::Lower),
    ("raise-or-lower", WindowAction::RaiseOrLower),
    ("show-desktop", WindowAction::ShowDesktop),
];

/// The configured keys of [`WmKey`]s.
pub fn wm_key_accelerators() -> Vec<(String, WmKey)> {
    if !helix_settings::has_schema(KEYBINDINGS) {
        return Vec::new();
    }
    let settings = gio::Settings::new(KEYBINDINGS);
    let mut out: Vec<(String, WmKey)> = Vec::new();
    for (key, action) in [
        ("cycle-windows", WmKey::Cycle { group: false, backward: false }),
        ("cycle-windows-backward", WmKey::Cycle { group: false, backward: true }),
        ("cycle-group", WmKey::Cycle { group: true, backward: false }),
        ("cycle-group-backward", WmKey::Cycle { group: true, backward: true }),
        ("switch-panels", WmKey::Panels { backward: false }),
        ("switch-panels-backward", WmKey::Panels { backward: true }),
        ("toggle-on-all-workspaces", WmKey::ToggleOnAllWorkspaces),
    ]
    .into_iter()
    .chain(WINDOW_KEYS.iter().map(|(k, a)| (*k, WmKey::Window(*a))))
    {
        for accel in settings.strv(key) {
            let accel = accel.to_string();
            if !accel.is_empty() && !out.iter().any(|(a, _)| *a == accel) {
                out.push((accel, action));
            }
        }
    }
    // Tiling keys live in mutter's own schema.
    if helix_settings::has_schema(MUTTER_KEYBINDINGS) {
        let mutter = gio::Settings::new(MUTTER_KEYBINDINGS);
        for (key, action) in [("toggle-tiled-left", WindowAction::ToggleTiledLeft), ("toggle-tiled-right", WindowAction::ToggleTiledRight)] {
            for accel in mutter.strv(key) {
                let accel = accel.to_string();
                if !accel.is_empty() && !out.iter().any(|(a, _)| *a == accel) {
                    out.push((accel, WmKey::Window(action)));
                }
            }
        }
    }
    out
}

/// The order `cycle-windows` / `cycle-group` go through: window ids, most
/// recently used first (only the focused window's app with `group`).
pub fn cycle_order(windows: &Windows, group: bool, only: Option<u32>) -> Vec<u32> {
    let focused_app = windows.mru().iter().find(|e| e.id == windows.focused()).map(|e| e.app_id.clone());
    windows
        .mru()
        .iter()
        .filter(|e| only.is_none_or(|w| e.workspace == w || e.workspace == ALL_WORKSPACES))
        .filter(|e| !group || focused_app.as_ref().is_some_and(|a| !a.is_empty() && *a == e.app_id))
        .map(|e| e.id)
        .collect()
}

/// What a switcher lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchMode {
    /// One entry per application (`switch-applications`).
    Apps,
    /// One entry per window (`switch-windows`).
    Windows,
    /// The focused window's application's windows (`switch-group`).
    Group,
}

/// The configured switcher keys: (accelerator, backward, mode); the first
/// key to use an accelerator wins.
pub fn accelerators() -> Vec<(String, bool, SwitchMode)> {
    if !helix_settings::has_schema(KEYBINDINGS) {
        return vec![
            ("<Alt>Tab".into(), false, SwitchMode::Apps),
            ("<Shift><Alt>Tab".into(), true, SwitchMode::Apps),
            ("<Alt>Above_Tab".into(), false, SwitchMode::Group),
            ("<Shift><Alt>Above_Tab".into(), true, SwitchMode::Group),
        ];
    }
    let settings = gio::Settings::new(KEYBINDINGS);
    let mut out: Vec<(String, bool, SwitchMode)> = Vec::new();
    for (key, backward, mode) in [
        ("switch-applications", false, SwitchMode::Apps),
        ("switch-applications-backward", true, SwitchMode::Apps),
        ("switch-windows", false, SwitchMode::Windows),
        ("switch-windows-backward", true, SwitchMode::Windows),
        ("switch-group", false, SwitchMode::Group),
        ("switch-group-backward", true, SwitchMode::Group),
    ] {
        for accel in settings.strv(key) {
            let accel = accel.to_string();
            if !accel.is_empty() && !out.iter().any(|(a, _, _)| *a == accel) {
                out.push((accel, backward, mode));
            }
        }
    }
    out
}

/// Which workspace a workspace key means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceTarget {
    /// 1-based, as in the key names.
    Number(u32),
    Left,
    Right,
    Last,
}

/// What a workspace key does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceKey {
    /// Take the focused window along.
    pub move_window: bool,
    pub target: WorkspaceTarget,
}

/// The configured workspace keys (`-up` / `-down` do nothing: workspaces
/// are in a row).
pub fn workspace_accelerators() -> Vec<(String, WorkspaceKey)> {
    if !helix_settings::has_schema(KEYBINDINGS) {
        return Vec::new();
    }
    let settings = gio::Settings::new(KEYBINDINGS);
    let mut keys: Vec<(String, WorkspaceKey)> = Vec::new();
    for move_window in [false, true] {
        let prefix = if move_window { "move-to-workspace-" } else { "switch-to-workspace-" };
        let mut targets: Vec<(String, WorkspaceTarget)> =
            (1..=12).map(|n| (n.to_string(), WorkspaceTarget::Number(n))).collect();
        targets.push(("left".into(), WorkspaceTarget::Left));
        targets.push(("right".into(), WorkspaceTarget::Right));
        targets.push(("last".into(), WorkspaceTarget::Last));
        for (suffix, target) in targets {
            for accel in settings.strv(format!("{prefix}{suffix}").as_str()) {
                let accel = accel.to_string();
                if !accel.is_empty() && !keys.iter().any(|(a, _)| *a == accel) {
                    keys.push((accel, WorkspaceKey { move_window, target }));
                }
            }
        }
    }
    keys
}

/// `current-workspace-only` for the (application, window) switchers.
pub fn current_workspace_only() -> (bool, bool) {
    let read = |schema: &str, default: bool| {
        gio::SettingsSchemaSource::default()
            .and_then(|s| s.lookup(schema, true))
            .filter(|s| s.has_key("current-workspace-only"))
            .map_or(default, |_| gio::Settings::new(schema).boolean("current-workspace-only"))
    };
    (read("org.gnome.shell.app-switcher", false), read("org.gnome.shell.window-switcher", true))
}

/// Installed applications' names by app id (desktop file id without
/// `.desktop`, lowercased).
pub fn app_names() -> std::collections::HashMap<String, String> {
    gio::AppInfo::all()
        .into_iter()
        .filter_map(|info| {
            let id = info.id()?.to_string();
            let id = id.strip_suffix(".desktop").unwrap_or(&id).to_lowercase();
            Some((id, info.display_name().to_string()))
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowEntry {
    pub id: u32,
    pub app_id: String,
    pub title: String,
    /// The workspace it is on (0-based).
    pub workspace: u32,
}

impl WindowEntry {
    /// What to show: the title, else the app id.
    pub fn label(&self) -> &str {
        match (self.title.is_empty(), self.app_id.is_empty()) {
            (false, _) => &self.title,
            (true, false) => &self.app_id,
            (true, true) => "Untitled window",
        }
    }
}

#[derive(Debug)]
pub struct Windows {
    /// Most recently focused first.
    entries: Vec<WindowEntry>,
    focused: u32,
    /// Workspaces: how many, and the active one (0-based).
    pub count: u32,
    pub active: u32,
}

impl Default for Windows {
    fn default() -> Self {
        Self { entries: Vec::new(), focused: 0, count: 1, active: 0 }
    }
}

impl Windows {
    /// A window appeared (added as least recently used) or changed.
    pub fn update(&mut self, id: u32, app_id: String, title: String) {
        match self.entries.iter_mut().find(|e| e.id == id) {
            Some(entry) => {
                entry.app_id = app_id;
                entry.title = title;
            }
            None => self.entries.push(WindowEntry { id, app_id, title, workspace: self.active }),
        }
    }

    pub fn set_workspace(&mut self, id: u32, workspace: u32) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.id == id) {
            entry.workspace = workspace;
        }
    }

    pub fn set_workspaces(&mut self, count: u32, active: u32) {
        self.count = count.max(1);
        self.active = active;
    }

    /// The workspace for a workspace key, if it exists.
    pub fn workspace_target(&self, target: WorkspaceTarget) -> Option<u32> {
        let found = match target {
            WorkspaceTarget::Number(n) => n.checked_sub(1)?,
            WorkspaceTarget::Left => self.active.checked_sub(1)?,
            WorkspaceTarget::Right => self.active + 1,
            WorkspaceTarget::Last => self.count - 1,
        };
        (found < self.count).then_some(found)
    }

    pub fn close(&mut self, id: u32) {
        self.entries.retain(|e| e.id != id);
        if self.focused == id {
            self.focused = 0;
        }
    }

    pub fn focus(&mut self, id: u32) {
        self.focused = id;
        if let Some(pos) = self.entries.iter().position(|e| e.id == id) {
            let entry = self.entries.remove(pos);
            self.entries.insert(0, entry);
        }
    }

    /// The workspace window `id` is on.
    pub fn workspace_of(&self, id: u32) -> Option<u32> {
        self.entries.iter().find(|e| e.id == id).map(|e| e.workspace)
    }

    pub fn focused(&self) -> u32 {
        self.focused
    }

    pub fn mru(&self) -> &[WindowEntry] {
        &self.entries
    }
}

/// One switcher entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchEntry {
    /// The window to activate (or, for input sources, the source's index).
    pub id: u32,
    pub label: String,
    /// Shown dimmed on the right.
    pub detail: String,
}

/// An open switcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Switcher {
    pub entries: Vec<SwitchEntry>,
    pub selected: usize,
}

impl Switcher {
    /// Start switching among `entries`: the one after the current one is
    /// selected (the last one, backward). `None` when there is nothing to
    /// switch to.
    pub fn new(entries: Vec<SwitchEntry>, current_first: bool, backward: bool) -> Option<Self> {
        if entries.is_empty() || (entries.len() == 1 && current_first) {
            return None;
        }
        let selected = match (current_first, backward) {
            (true, false) => 1,
            (_, true) => entries.len() - 1,
            (false, false) => 0,
        };
        Some(Self { entries, selected })
    }

    /// `only`: list just the windows on this workspace.
    pub fn open(
        windows: &Windows,
        mode: SwitchMode,
        backward: bool,
        only: Option<u32>,
        names: impl Fn(&str) -> Option<String>,
    ) -> Option<Self> {
        let focused = windows.focused();
        let pool: Vec<&WindowEntry> = windows
            .mru()
            .iter()
            .filter(|e| only.is_none_or(|w| e.workspace == w || e.workspace == ALL_WORKSPACES))
            .collect();
        let window = |e: &WindowEntry| SwitchEntry { id: e.id, label: e.label().to_owned(), detail: e.app_id.clone() };
        match mode {
            SwitchMode::Windows => {
                let entries: Vec<SwitchEntry> = pool.iter().copied().map(window).collect();
                let current_first = entries.first().is_some_and(|e| e.id == focused);
                Self::new(entries, current_first, backward)
            }
            SwitchMode::Group => {
                let app = windows.mru().iter().find(|e| e.id == focused).map(|e| e.app_id.clone()).filter(|a| !a.is_empty())?;
                let entries: Vec<SwitchEntry> = pool.iter().copied().filter(|e| e.app_id == app).map(window).collect();
                let current_first = entries.first().is_some_and(|e| e.id == focused);
                Self::new(entries, current_first, backward)
            }
            SwitchMode::Apps => {
                // Applications in the order of their most recent window.
                let mut apps: Vec<(&WindowEntry, usize)> = Vec::new();
                for e in pool.iter().copied() {
                    match apps.iter_mut().find(|(first, _)| !e.app_id.is_empty() && first.app_id == e.app_id) {
                        Some((_, count)) => *count += 1,
                        None => apps.push((e, 1)),
                    }
                }
                let focused_entry = windows.mru().iter().find(|w| w.id == focused);
                let current_first = match (apps.first(), focused_entry) {
                    (Some((first, _)), Some(f)) => first.id == f.id || (!first.app_id.is_empty() && first.app_id == f.app_id),
                    _ => false,
                };
                let entries = apps
                    .into_iter()
                    .map(|(e, count)| SwitchEntry {
                        id: e.id,
                        label: names(&e.app_id).unwrap_or_else(|| if e.app_id.is_empty() { e.label().to_owned() } else { e.app_id.clone() }),
                        detail: if count == 1 { "1 window".into() } else { format!("{count} windows") },
                    })
                    .collect();
                Self::new(entries, current_first, backward)
            }
        }
    }

    /// The windows of the application `window` belongs to, most recent
    /// first and selected (from the application switcher).
    pub fn app_windows(windows: &Windows, window: u32, only: Option<u32>) -> Option<Self> {
        let app = windows.mru().iter().find(|e| e.id == window)?.app_id.clone();
        let entries: Vec<SwitchEntry> = windows
            .mru()
            .iter()
            .filter(|e| if app.is_empty() { e.id == window } else { e.app_id == app })
            .filter(|e| only.is_none_or(|w| e.workspace == w || e.workspace == ALL_WORKSPACES))
            .map(|e| SwitchEntry { id: e.id, label: e.label().to_owned(), detail: e.app_id.clone() })
            .collect();
        (!entries.is_empty()).then_some(Self { entries, selected: 0 })
    }

    pub fn step(&mut self, backward: bool) {
        let n = self.entries.len();
        self.selected = if backward { (self.selected + n - 1) % n } else { (self.selected + 1) % n };
    }

    pub fn selected_id(&self) -> u32 {
        self.entries[self.selected].id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn windows(ids: &[u32]) -> Windows {
        let mut w = Windows::default();
        for &id in ids {
            w.update(id, format!("app{id}"), format!("Window {id}"));
        }
        w
    }

    #[test]
    fn most_recently_focused_first() {
        let mut w = windows(&[1, 2, 3]);
        w.focus(3);
        w.focus(2);
        assert_eq!(w.mru().iter().map(|e| e.id).collect::<Vec<_>>(), [2, 3, 1]);
        w.update(3, "app3".into(), "Renamed".into());
        assert_eq!(w.mru()[1].title, "Renamed", "updates keep the order");
        w.close(2);
        assert_eq!(w.focused(), 0);
        assert_eq!(w.mru().iter().map(|e| e.id).collect::<Vec<_>>(), [3, 1]);
    }

    fn open(w: &Windows, mode: SwitchMode, backward: bool) -> Option<Switcher> {
        Switcher::open(w, mode, backward, None, |_| None)
    }

    #[test]
    fn switcher_selection() {
        let mut w = windows(&[1]);
        w.focus(1);
        assert_eq!(open(&w, SwitchMode::Windows, false), None, "nothing to switch to");
        let mut w = windows(&[1, 2, 3]);
        w.focus(1);
        let mut s = open(&w, SwitchMode::Windows, false).unwrap();
        assert_eq!(s.selected_id(), 2, "the previously focused window");
        s.step(false);
        s.step(false);
        assert_eq!(s.selected_id(), 1, "wraps around");
        s.step(true);
        assert_eq!(s.selected_id(), 3);
        assert_eq!(open(&w, SwitchMode::Windows, true).unwrap().selected_id(), 3, "backward starts at the end");
        w.focus(0);
        assert_eq!(open(&w, SwitchMode::Windows, false).unwrap().selected_id(), 1, "no focus: the most recent one");
    }

    /// Windows 1..=4: 1 and 3 are app "a", 2 is "b", 4 has no app id.
    fn mixed() -> Windows {
        let mut w = Windows::default();
        for (id, app) in [(1, "a"), (2, "b"), (3, "a"), (4, "")] {
            w.update(id, app.into(), format!("Window {id}"));
        }
        for id in [4, 2, 3, 1] {
            w.focus(id);
        }
        w
    }

    #[test]
    fn applications_are_grouped() {
        let w = mixed();
        // MRU: 1 (a), 3 (a), 2 (b), 4 (none).
        let names = |app: &str| (app == "a").then(|| "App A".to_owned());
        let s = Switcher::open(&w, SwitchMode::Apps, false, None, names).unwrap();
        let shown: Vec<(&str, &str, u32)> = s.entries.iter().map(|e| (e.label.as_str(), e.detail.as_str(), e.id)).collect();
        assert_eq!(shown, [("App A", "2 windows", 1), ("b", "1 window", 2), ("Window 4", "1 window", 4)]);
        assert_eq!(s.selected_id(), 2, "the next application's most recent window");
        assert_eq!(open(&w, SwitchMode::Apps, true).unwrap().selected_id(), 4);
    }

    #[test]
    fn workspace_filter_and_targets() {
        let mut w = mixed();
        w.set_workspaces(3, 1);
        w.set_workspace(3, 1);
        w.set_workspace(2, 1);
        // MRU 1 (a, ws0), 3 (a, ws1), 2 (b, ws1), 4 (ws0); focus on 1.
        let s = Switcher::open(&w, SwitchMode::Windows, false, Some(1), |_| None).unwrap();
        assert_eq!(s.entries.iter().map(|e| e.id).collect::<Vec<_>>(), [3, 2]);
        assert_eq!(s.selected_id(), 3, "the focused window is elsewhere: start at the first");
        let apps = Switcher::open(&w, SwitchMode::Apps, false, Some(1), |_| None).unwrap();
        assert_eq!(apps.entries.iter().map(|e| e.id).collect::<Vec<_>>(), [3, 2], "a's window here, then b");
        assert_eq!(w.workspace_target(WorkspaceTarget::Number(1)), Some(0));
        assert_eq!(w.workspace_target(WorkspaceTarget::Number(4)), None);
        assert_eq!(w.workspace_target(WorkspaceTarget::Left), Some(0));
        assert_eq!(w.workspace_target(WorkspaceTarget::Right), Some(2));
        assert_eq!(w.workspace_target(WorkspaceTarget::Last), Some(2));
        w.set_workspaces(3, 0);
        assert_eq!(w.workspace_target(WorkspaceTarget::Left), None);
        w.set_workspaces(3, 2);
        assert_eq!(w.workspace_target(WorkspaceTarget::Right), None);
        assert_eq!(w.workspace_target(WorkspaceTarget::Number(0)), None);
    }

    #[test]
    fn cycles_and_app_windows() {
        let mut w = mixed();
        // MRU 1 (a), 3 (a), 2 (b), 4 (none); focus 1.
        assert_eq!(cycle_order(&w, false, None), [1, 3, 2, 4]);
        assert_eq!(cycle_order(&w, true, None), [1, 3]);
        w.set_workspaces(2, 1);
        w.set_workspace(2, 1);
        w.set_workspace(4, ALL_WORKSPACES);
        assert_eq!(cycle_order(&w, false, Some(1)), [2, 4], "on every workspace counts");
        let s = Switcher::app_windows(&w, 3, None).unwrap();
        assert_eq!(s.entries.iter().map(|e| e.id).collect::<Vec<_>>(), [1, 3]);
        assert_eq!(s.selected_id(), 1);
        assert_eq!(Switcher::app_windows(&w, 4, None).unwrap().entries.len(), 1, "no app id: just it");
    }

    #[test]
    fn group_switches_within_the_application() {
        let mut w = mixed();
        let s = open(&w, SwitchMode::Group, false).unwrap();
        assert_eq!(s.entries.iter().map(|e| e.id).collect::<Vec<_>>(), [1, 3]);
        assert_eq!(s.selected_id(), 3);
        w.focus(2);
        assert_eq!(open(&w, SwitchMode::Group, false), None, "only one window of b");
        w.focus(4);
        assert_eq!(open(&w, SwitchMode::Group, false), None, "no app id");
    }

    #[test]
    fn labels() {
        let e = |app: &str, title: &str| WindowEntry { id: 1, app_id: app.into(), title: title.into(), workspace: 0 };
        assert_eq!(e("org.gnome.Calculator", "Calculator").label(), "Calculator");
        assert_eq!(e("org.gnome.Calculator", "").label(), "org.gnome.Calculator");
        assert_eq!(e("", "").label(), "Untitled window");
    }
}
