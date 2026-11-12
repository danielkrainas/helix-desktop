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
    /// Its desktop file's `Categories` (for app folders).
    pub categories: Vec<String>,
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
                categories: crate::favorites::categories(&id),
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
    /// Index into the search providers' results.
    Search(usize),
    /// Index into the app folders.
    Folder(usize),
    /// "Search in <app>" after a provider's results: index of one of its
    /// results.
    More(usize),
}

/// A search provider's result.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    /// The provider's place in the provider order.
    pub provider: usize,
    /// The provider's app name.
    pub app: String,
    pub id: String,
    pub name: String,
    pub description: String,
    /// Its icon (`icon` or `icon-data`).
    pub icon: Option<crate::icons::IconSource>,
    /// Text to put on the clipboard when it is chosen (`clipboardText`).
    pub clipboard: Option<String>,
}

/// The search results' rows: each provider's results, then a "Search in"
/// row for it (pointing at the provider's last result).
pub fn search_rows(external: &[SearchHit]) -> Vec<Hit> {
    let mut rows = Vec::new();
    for (i, hit) in external.iter().enumerate() {
        rows.push(Hit::Search(i));
        if external.get(i + 1).is_none_or(|next| next.provider != hit.provider) {
            rows.push(Hit::More(i));
        }
    }
    rows
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
    /// Search provider results for `query`, in provider order.
    pub external: Vec<SearchHit>,
    /// Favorite app ids, in order.
    pub favorites: Vec<String>,
    pub folders: Vec<crate::favorites::Folder>,
    /// The folder shown, if one is open.
    pub open_folder: Option<usize>,
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

    /// Show favorites and folders (with an empty search).
    pub fn set_collections(&mut self, favorites: Vec<String>, folders: Vec<crate::favorites::Folder>) {
        self.favorites = favorites;
        self.folders = folders;
        self.refilter();
    }

    /// Whether app `i` is a favorite.
    pub fn is_favorite(&self, i: usize) -> bool {
        self.favorites.contains(&self.apps[i].id)
    }

    /// Open folder `i`.
    pub fn enter_folder(&mut self, i: usize) {
        if i < self.folders.len() {
            self.open_folder = Some(i);
            self.query.clear();
            self.refilter();
        }
    }

    /// Back from an open folder; false if none was open.
    pub fn leave_folder(&mut self) -> bool {
        if self.open_folder.take().is_some() {
            self.refilter();
            return true;
        }
        false
    }

    /// The open folder's name.
    pub fn folder_name(&self) -> Option<&str> {
        self.open_folder.and_then(|i| self.folders.get(i)).map(|f| f.name.as_str())
    }

    fn index_of(&self, id: &str) -> Option<usize> {
        self.apps.iter().position(|a| a.id == id)
    }

    /// The empty search: favorites, folders, then the other apps.
    fn browse(&self) -> Vec<Hit> {
        if let Some(folder) = self.open_folder.and_then(|i| self.folders.get(i)) {
            return folder.apps.iter().filter_map(|id| self.index_of(id)).map(Hit::App).collect();
        }
        let in_folders: Vec<&String> = self.folders.iter().flat_map(|f| f.apps.iter()).collect();
        let favorites: Vec<usize> = self.favorites.iter().filter_map(|id| self.index_of(id)).collect();
        favorites
            .iter()
            .copied()
            .map(Hit::App)
            .chain((0..self.folders.len()).map(Hit::Folder))
            .chain((0..self.apps.len()).filter(|i| !favorites.contains(i) && !in_folders.contains(&&self.apps[*i].id)).map(Hit::App))
            .collect()
    }

    /// Windows whose title or app id match come first, then apps, then
    /// search provider results (the query changed: those go).
    fn refilter(&mut self) {
        self.external.clear();
        self.rebuild();
        self.selected = 0;
    }

    /// A provider answered for the current query.
    pub fn set_search(&mut self, provider: usize, hits: Vec<SearchHit>) {
        self.external.retain(|h| h.provider != provider);
        self.external.extend(hits);
        self.external.sort_by_key(|h| h.provider);
        let selected = self.selected();
        self.rebuild();
        // Keep the selection on what it was, if it is still there.
        self.selected = selected.and_then(|s| self.results.iter().position(|r| *r == s)).unwrap_or(0);
    }

    fn rebuild(&mut self) {
        let q = self.query.trim().to_lowercase();
        if q.is_empty() && (self.open_folder.is_some() || !self.favorites.is_empty() || !self.folders.is_empty()) {
            let windows = if self.open_folder.is_some() { Vec::new() } else { (0..self.windows.len()).map(Hit::Window).collect() };
            self.results = windows.into_iter().chain(self.browse()).collect();
            return;
        }
        self.results = self
            .windows
            .iter()
            .enumerate()
            .filter(|(_, w)| q.is_empty() || w.title.to_lowercase().contains(&q) || w.app_id.to_lowercase().contains(&q))
            .map(|(i, _)| Hit::Window(i))
            .chain(search(&self.apps, &self.query).into_iter().map(Hit::App))
            .chain(if q.is_empty() { Vec::new() } else { search_rows(&self.external) })
            .collect();
    }

    /// The search result a row shows.
    pub fn search_hit(&self, hit: Hit) -> Option<&SearchHit> {
        match hit {
            Hit::Search(i) | Hit::More(i) => self.external.get(i),
            _ => None,
        }
    }

    pub fn selected(&self) -> Option<Hit> {
        self.results.get(self.selected).copied()
    }

    /// Text for a row, and whether it is an open window.
    pub fn row_label(&self, hit: Hit) -> (&str, bool) {
        match hit {
            Hit::Window(i) => (self.windows[i].label(), true),
            Hit::App(i) => (&self.apps[i].name, false),
            Hit::Search(i) => (&self.external[i].name, false),
            Hit::More(i) => (&self.external[i].app, false),
            Hit::Folder(i) => (&self.folders[i].name, false),
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
            Hit::Window(_) | Hit::Search(_) | Hit::Folder(_) | Hit::More(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(id: &str, name: &str, search: &str) -> App {
        App {
            categories: Vec::new(),
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
        let window = |id: u32, app: &str, title: &str| WindowEntry { id, app_id: app.into(), title: title.into(), workspace: 0 };
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

    #[test]
    fn favorites_and_folders() {
        let mut l = Launcher::new(sample());
        let folder = crate::favorites::Folder { id: "u".into(), name: "Utilities".into(), apps: vec!["org.gnome.Calculator.desktop".into(), "org.gnome.TextEditor.desktop".into()] };
        l.set_collections(vec!["firefox.desktop".into(), "not.installed.desktop".into()], vec![folder]);
        let labels = |l: &Launcher| l.results.iter().map(|h| l.row_label(*h).0.to_owned()).collect::<Vec<_>>();
        assert_eq!(labels(&l), ["Firefox Web Browser", "Utilities", "Files"]);
        assert!(l.is_favorite(2));
        l.move_selection(1);
        assert_eq!(l.selected(), Some(Hit::Folder(0)));
        l.enter_folder(0);
        assert_eq!(labels(&l), ["Calculator", "Text Editor"]);
        assert_eq!(l.folder_name(), Some("Utilities"));
        // Typing searches every app.
        l.type_text("fi");
        assert!(labels(&l).contains(&"Files".to_owned()));
        assert!(l.leave_folder());
        l.backspace();
        l.backspace();
        assert_eq!(labels(&l), ["Firefox Web Browser", "Utilities", "Files"]);
    }
}
