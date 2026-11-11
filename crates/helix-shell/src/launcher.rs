//! The application launcher's model: open windows and installed apps,
//! search, selection.
//!
//! Apps come from installed `.desktop` files via GIO, the same source GNOME
//! uses; launching goes through GIO too, so `Exec` field codes, `Terminal`
//! and startup notification behave as usual.

use gio::prelude::*;

use crate::windows::WindowEntry;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    /// Desktop file id, e.g. `org.gnome.Calculator.desktop`.
    pub id: String,
    pub name: String,
    /// Extra searchable text (description, command line).
    pub search: String,
}

/// Installed apps that should be shown in menus, sorted by name.
pub fn installed() -> Vec<App> {
    let mut apps: Vec<App> = gio::AppInfo::all()
        .into_iter()
        .filter(|a| a.should_show())
        .filter_map(|info| {
            let id = info.id()?.to_string();
            // Not `executable()`: it assumes an Exec line, which
            // D-Bus-activatable apps may lack (the binding then panics).
            let mut search: Vec<String> = info
                .commandline()
                .map(|c| c.to_string_lossy().into_owned())
                .into_iter()
                .collect();
            if let Some(description) = info.description() {
                search.push(description.to_string());
            }
            Some(App {
                id,
                name: info.display_name().to_string(),
                search: search.join(" ").to_lowercase(),
            })
        })
        .collect();
    apps.sort_by_key(|a| a.name.to_lowercase());
    apps
}

/// Launch an app by desktop file id.
pub fn launch(id: &str) -> Result<(), String> {
    let info = gio::AppInfo::all()
        .into_iter()
        .find(|a| a.id().is_some_and(|i| i == id))
        .ok_or_else(|| format!("no app {id}"))?;
    info.launch(&[], None::<&gio::AppLaunchContext>)
        .map_err(|e| e.to_string())
}

/// Indices of apps matching `query`: name prefix matches first, then name
/// substring, then other searchable text. An empty query matches all.
pub fn search(apps: &[App], query: &str) -> Vec<usize> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return (0..apps.len()).collect();
    }
    let mut ranked: Vec<(u8, usize)> = apps
        .iter()
        .enumerate()
        .filter_map(|(i, app)| {
            let name = app.name.to_lowercase();
            let rank = if name.starts_with(&q) {
                0
            } else if name.split_whitespace().any(|w| w.starts_with(&q)) {
                1
            } else if name.contains(&q) {
                2
            } else if app.search.contains(&q) || app.id.to_lowercase().contains(&q) {
                3
            } else {
                return None;
            };
            Some((rank, i))
        })
        .collect();
    ranked.sort();
    ranked.into_iter().map(|(_, i)| i).collect()
}

/// One row of the launcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    /// Index into the open windows.
    Window(usize),
    /// Index into the apps.
    App(usize),
}

/// Launcher state while it is open.
#[derive(Debug, Default)]
pub struct Launcher {
    pub apps: Vec<App>,
    /// Open windows, most recently used first.
    pub windows: Vec<WindowEntry>,
    pub query: String,
    pub results: Vec<Hit>,
    pub selected: usize,
}

impl Launcher {
    pub fn new(apps: Vec<App>) -> Self {
        Self::with_windows(apps, Vec::new())
    }

    pub fn with_windows(apps: Vec<App>, windows: Vec<WindowEntry>) -> Self {
        let mut l = Self {
            apps,
            windows,
            ..Self::default()
        };
        l.refilter();
        l
    }

    /// Windows whose title or app id match come first, then apps.
    fn refilter(&mut self) {
        let q = self.query.trim().to_lowercase();
        self.results = self
            .windows
            .iter()
            .enumerate()
            .filter(|(_, w)| q.is_empty() || w.title.to_lowercase().contains(&q) || w.app_id.to_lowercase().contains(&q))
            .map(|(i, _)| Hit::Window(i))
            .chain(search(&self.apps, &self.query).into_iter().map(Hit::App))
            .collect();
        self.selected = 0;
    }

    pub fn selected(&self) -> Option<Hit> {
        self.results.get(self.selected).copied()
    }

    /// Text for a row, and whether it is an open window.
    pub fn row_label(&self, hit: Hit) -> (&str, bool) {
        match hit {
            Hit::Window(i) => (self.windows[i].label(), true),
            Hit::App(i) => (&self.apps[i].name, false),
        }
    }

    pub fn type_text(&mut self, text: &str) {
        let printable: String = text.chars().filter(|c| !c.is_control()).collect();
        if !printable.is_empty() {
            self.query.push_str(&printable);
            self.refilter();
        }
    }

    pub fn backspace(&mut self) {
        if self.query.pop().is_some() {
            self.refilter();
        }
    }

    /// Move the selection by `delta`, clamped to the results.
    pub fn move_selection(&mut self, delta: isize) {
        if self.results.is_empty() {
            return;
        }
        let max = self.results.len() as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, max) as usize;
    }

    pub fn selected_app(&self) -> Option<&App> {
        match self.selected()? {
            Hit::App(i) => Some(&self.apps[i]),
            Hit::Window(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(id: &str, name: &str, search: &str) -> App {
        App {
            id: id.into(),
            name: name.into(),
            search: search.into(),
        }
    }

    fn sample() -> Vec<App> {
        vec![
            app("org.gnome.Calculator.desktop", "Calculator", "gnome-calculator math"),
            app("org.gnome.TextEditor.desktop", "Text Editor", "gnome-text-editor notepad"),
            app("firefox.desktop", "Firefox Web Browser", "firefox internet"),
            app("org.gnome.Nautilus.desktop", "Files", "nautilus folder"),
        ]
    }

    #[test]
    fn search_ranks_prefix_then_word_then_substring_then_keywords() {
        let apps = sample();
        assert_eq!(search(&apps, ""), vec![0, 1, 2, 3]);
        assert_eq!(search(&apps, "calc"), vec![0]);
        // "Text Editor" matches by word prefix; nothing by name prefix.
        assert_eq!(search(&apps, "edit"), vec![1]);
        assert_eq!(search(&apps, "web"), vec![2]);
        // Keyword-only matches come last.
        assert_eq!(search(&apps, "folder"), vec![3]);
        // Case-insensitive; equal ranks keep list (alphabetical) order.
        assert_eq!(search(&apps, "FI"), vec![2, 3]);
        // Word prefix and plain substring matches.
        assert_eq!(search(&apps, "ed"), vec![1]);
        assert_eq!(search(&apps, "lat"), vec![0], "substring match");
        assert!(search(&apps, "zzz").is_empty());
    }

    #[test]
    fn typing_selection_and_backspace() {
        let mut l = Launcher::new(sample());
        assert_eq!(l.results.len(), 4);
        l.move_selection(2);
        assert_eq!(l.selected_app().unwrap().name, "Firefox Web Browser");
        l.move_selection(10);
        assert_eq!(l.selected, 3, "clamped");
        l.type_text("te");
        assert_eq!(l.selected, 0, "typing resets selection");
        assert_eq!(l.selected_app().unwrap().name, "Text Editor");
        l.type_text("\u{8}\t");
        assert_eq!(l.query, "te", "control characters ignored");
        l.backspace();
        l.backspace();
        l.backspace();
        assert_eq!(l.query, "");
        assert_eq!(l.results.len(), 4);
    }

    #[test]
    fn open_windows_come_first() {
        let window = |id: u32, app: &str, title: &str| WindowEntry { id, app_id: app.into(), title: title.into() };
        let windows = vec![window(7, "org.gnome.TextEditor", "notes.txt"), window(3, "firefox", "News")];
        let mut l = Launcher::with_windows(sample(), windows);
        assert_eq!(l.results[..3], [Hit::Window(0), Hit::Window(1), Hit::App(0)]);
        assert_eq!(l.row_label(Hit::Window(0)), ("notes.txt", true));
        assert_eq!(l.selected_app(), None, "a window is selected");
        l.type_text("fire");
        assert_eq!(l.results, [Hit::Window(1), Hit::App(2)], "windows match by app id too");
        l.type_text("x");
        assert!(l.results.is_empty());
    }
}
