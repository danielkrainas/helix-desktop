//! Open windows as the compositor reports them (`helix_window_list_v1`),
//! most recently focused first, and the window switcher's selection
//! (helix-desktop-spec `contracts/wayland/README.md`, "Window switching").

use gio::prelude::*;

const KEYBINDINGS: &str = "org.gnome.desktop.wm.keybindings";

/// The configured switcher keys: (accelerator, backward).
pub fn accelerators() -> Vec<(String, bool)> {
    if !helix_settings::has_schema(KEYBINDINGS) {
        return vec![("<Alt>Tab".into(), false), ("<Shift><Alt>Tab".into(), true)];
    }
    let settings = gio::Settings::new(KEYBINDINGS);
    let mut out: Vec<(String, bool)> = Vec::new();
    for (key, backward) in [
        ("switch-applications", false),
        ("switch-applications-backward", true),
        ("switch-windows", false),
        ("switch-windows-backward", true),
    ] {
        for accel in settings.strv(key) {
            let accel = accel.to_string();
            if !out.iter().any(|(a, _)| *a == accel) {
                out.push((accel, backward));
            }
        }
    }
    out
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

/// The open window switcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Switcher {
    pub entries: Vec<WindowEntry>,
    pub selected: usize,
}

impl Switcher {
    /// Start switching: the previously focused window is selected (or the
    /// last one, backward). `None` when there is nothing to switch to.
    pub fn open(windows: &Windows, backward: bool) -> Option<Self> {
        let entries = windows.mru().to_vec();
        let current_first = entries.first().is_some_and(|e| e.id == windows.focused());
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

    #[test]
    fn switcher_selection() {
        let mut w = windows(&[1]);
        w.focus(1);
        assert_eq!(Switcher::open(&w, false), None, "nothing to switch to");
        let mut w = windows(&[1, 2, 3]);
        w.focus(1);
        let mut s = Switcher::open(&w, false).unwrap();
        assert_eq!(s.selected_id(), 2, "the previously focused window");
        s.step(false);
        s.step(false);
        assert_eq!(s.selected_id(), 1, "wraps around");
        s.step(true);
        assert_eq!(s.selected_id(), 3);
        assert_eq!(Switcher::open(&w, true).unwrap().selected_id(), 3, "backward starts at the end");
        w.focus(0);
        assert_eq!(Switcher::open(&w, false).unwrap().selected_id(), 1, "no focus: the most recent one");
    }

    #[test]
    fn labels() {
        let e = |app: &str, title: &str| WindowEntry { id: 1, app_id: app.into(), title: title.into() };
        assert_eq!(e("org.gnome.Calculator", "Calculator").label(), "Calculator");
        assert_eq!(e("org.gnome.Calculator", "").label(), "org.gnome.Calculator");
        assert_eq!(e("", "").label(), "Untitled window");
    }
}
