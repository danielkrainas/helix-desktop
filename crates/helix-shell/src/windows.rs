//! Open windows as the compositor reports them (`helix_window_list_v1`),
//! most recently focused first, and the switchers' entries and selection
//! (helix-desktop-spec `contracts/wayland/README.md`, "Window switching").

use gio::prelude::*;

const KEYBINDINGS: &str = "org.gnome.desktop.wm.keybindings";

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

#[derive(Debug, Default)]
pub struct Windows {
    /// Most recently focused first.
    entries: Vec<WindowEntry>,
    focused: u32,
}

impl Windows {
    /// A window appeared (added as least recently used) or changed.
    pub fn update(&mut self, id: u32, app_id: String, title: String) {
        match self.entries.iter_mut().find(|e| e.id == id) {
            Some(entry) => {
                entry.app_id = app_id;
                entry.title = title;
            }
            None => self.entries.push(WindowEntry { id, app_id, title }),
        }
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

    pub fn open(windows: &Windows, mode: SwitchMode, backward: bool, names: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let focused = windows.focused();
        let window = |e: &WindowEntry| SwitchEntry { id: e.id, label: e.label().to_owned(), detail: e.app_id.clone() };
        match mode {
            SwitchMode::Windows => {
                let entries: Vec<SwitchEntry> = windows.mru().iter().map(window).collect();
                let current_first = entries.first().is_some_and(|e| e.id == focused);
                Self::new(entries, current_first, backward)
            }
            SwitchMode::Group => {
                let app = windows.mru().iter().find(|e| e.id == focused).map(|e| e.app_id.clone()).filter(|a| !a.is_empty())?;
                let entries: Vec<SwitchEntry> = windows.mru().iter().filter(|e| e.app_id == app).map(window).collect();
                let current_first = entries.first().is_some_and(|e| e.id == focused);
                Self::new(entries, current_first, backward)
            }
            SwitchMode::Apps => {
                // Applications in the order of their most recent window.
                let mut apps: Vec<(&WindowEntry, usize)> = Vec::new();
                for e in windows.mru() {
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
        Switcher::open(w, mode, backward, |_| None)
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
        let s = Switcher::open(&w, SwitchMode::Apps, false, names).unwrap();
        let shown: Vec<(&str, &str, u32)> = s.entries.iter().map(|e| (e.label.as_str(), e.detail.as_str(), e.id)).collect();
        assert_eq!(shown, [("App A", "2 windows", 1), ("b", "1 window", 2), ("Window 4", "1 window", 4)]);
        assert_eq!(s.selected_id(), 2, "the next application's most recent window");
        assert_eq!(open(&w, SwitchMode::Apps, true).unwrap().selected_id(), 4);
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
        let e = |app: &str, title: &str| WindowEntry { id: 1, app_id: app.into(), title: title.into() };
        assert_eq!(e("org.gnome.Calculator", "Calculator").label(), "Calculator");
        assert_eq!(e("org.gnome.Calculator", "").label(), "org.gnome.Calculator");
        assert_eq!(e("", "").label(), "Untitled window");
    }
}
