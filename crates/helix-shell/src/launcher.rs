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
    launch_with(id, &[])
}

/// Launch an app by desktop file id, handing it `uris` (agents' `Open`
/// and `Launch`, helixdesktop/desktop#147).
pub fn launch_with(id: &str, uris: &[String]) -> Result<(), String> {
    let info = gio::AppInfo::all()
        .into_iter()
        .find(|a| a.id().is_some_and(|i| i == id))
        .ok_or_else(|| format!("no app {id}"))?;
    let context = crate::app_scope::context(None);
    if uris.is_empty() {
        info.launch(&[], Some(&context))
    } else {
        let uris: Vec<&str> = uris.iter().map(String::as_str).collect();
        info.launch_uris(&uris, Some(&context))
    }
    .map_err(|e| e.to_string())?;
    crate::app_usage::record(id);
    Ok(())
}

/// Indices of apps matching `query`: name prefix matches first, then name
/// substring, then other searchable text; within each, more used apps
/// first (`app_usage`). An empty query matches all.
pub fn search(apps: &[App], query: &str) -> Vec<usize> {
    // Tests rank without this machine's usage.
    let usage = if cfg!(test) { crate::app_usage::Usage::new() } else { crate::app_usage::read() };
    search_with(apps, query, &usage, crate::app_usage::at())
}

/// [`search`] with this usage, at Unix time `at`.
pub fn search_with(apps: &[App], query: &str, usage: &crate::app_usage::Usage, at: i64) -> Vec<usize> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return (0..apps.len()).collect();
    }
    let mut ranked: Vec<(u8, i64, usize)> = apps
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
            // More used first (scores to thousandths).
            let used = -((crate::app_usage::score(usage, &app.id, at) * 1000.0) as i64);
            Some((rank, used, i))
        })
        .collect();
    ranked.sort();
    ranked.into_iter().map(|(_, _, i)| i).collect()
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

/// A part of the result list: a section's heading or a row (by result
/// index).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListItem {
    Heading(Section),
    Row(usize),
}

/// A section of the result list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Windows,
    Apps,
    /// A search provider's results: the index of its first one.
    Provider(usize),
}

/// The first item of `heights` (each item's height) to show so that item
/// `keep` shows in `room` (and as many before it as fit).
pub fn list_start(heights: &[i32], keep: usize, room: i32) -> usize {
    let mut start = 0;
    while start < keep && heights[start..=keep.min(heights.len().saturating_sub(1))].iter().sum::<i32>() > room {
        start += 1;
    }
    start
}

/// The first item to show so that the list ends filled: the smallest
/// start whose items from there on all fit in `room`.
pub fn last_start(heights: &[i32], room: i32) -> usize {
    let (mut start, mut used) = (heights.len(), 0);
    while start > 0 && used + heights[start - 1] <= room {
        start -= 1;
        used += heights[start];
    }
    start
}

/// How the result list is scrolled, in whole items (headings and rows):
/// the pointer wheel or touchpad moves it, the keyboard's selection is
/// kept in sight when it moves, and a new query starts at the top.
#[derive(Debug, Clone, Default)]
pub struct ListScroll {
    /// The first item shown.
    pub start: usize,
    /// The query and selected item the list last followed.
    followed: Option<(String, usize)>,
    /// Touchpad travel not yet a whole item (logical px; down positive).
    travel: f64,
}

impl ListScroll {
    /// The first item to show of items `heights` (`headings`: which are
    /// section headings) in `room`, with item `keep` (the selection)
    /// selected for `query`. A selection that moved is brought into
    /// sight (with its heading when that is just above it), scrolling as
    /// little as needed; otherwise the list stays where the wheel left
    /// it. The list never scrolls past its end.
    pub fn place(&mut self, query: &str, heights: &[i32], headings: &[bool], keep: usize, room: i32) -> usize {
        if heights.is_empty() {
            *self = Self::default();
            return 0;
        }
        let keep = keep.min(heights.len() - 1);
        if self.followed.as_ref().is_some_and(|(q, _)| q != query) {
            self.start = 0;
            self.travel = 0.0;
        }
        if self.followed.as_ref().is_none_or(|(q, k)| q != query || *k != keep) {
            let top = if keep > 0 && headings.get(keep - 1) == Some(&true) { keep - 1 } else { keep };
            if top < self.start {
                self.start = top;
            } else {
                self.start = self.start.max(list_start(heights, keep, room));
            }
            self.followed = Some((query.to_owned(), keep));
        }
        self.start = self.start.min(last_start(heights, room));
        self.start
    }

    /// Scroll by `items` (down positive); the next [`place`](Self::place)
    /// keeps it within the list.
    pub fn scroll_by(&mut self, items: isize) {
        self.start = self.start.saturating_add_signed(items);
    }

    /// Touchpad travel of `px` (logical, down positive): the whole items
    /// of `step` px it adds up to, the rest kept for the next.
    pub fn travel(&mut self, px: f64, step: f64) -> isize {
        // A change of direction drops what was left the other way.
        if self.travel * px < 0.0 {
            self.travel = 0.0;
        }
        self.travel += px;
        let items = (self.travel / step.max(1.0)).trunc();
        self.travel -= items * step.max(1.0);
        items as isize
    }
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

    /// The results in sections, each under its heading: open windows,
    /// apps, then each search provider's results (with its "Search in"
    /// row last).
    pub fn sections(&self) -> Vec<ListItem> {
        let mut out = Vec::new();
        let mut current: Option<Section> = None;
        for (i, hit) in self.results.iter().enumerate() {
            let section = match hit {
                Hit::Window(_) => Section::Windows,
                Hit::App(_) | Hit::Folder(_) => Section::Apps,
                Hit::Search(e) | Hit::More(e) => {
                    let provider = self.external.get(*e).map(|h| h.provider);
                    let first = self.external.iter().position(|h| Some(h.provider) == provider).unwrap_or(*e);
                    Section::Provider(first)
                }
            };
            if current != Some(section) {
                out.push(ListItem::Heading(section));
                current = Some(section);
            }
            out.push(ListItem::Row(i));
        }
        out
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

    /// Replace the query (an assistive technology set the field's text).
    pub fn set_query(&mut self, query: &str) {
        let query: String = query.chars().filter(|c| !c.is_control()).collect();
        if query != self.query {
            self.query = query;
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

    #[test]
    fn sections() {
        use super::*;
        let app = |name: &str| App { id: format!("{name}.desktop"), name: name.into(), search: String::new(), categories: Vec::new() };
        let mut l = Launcher::with_windows(vec![app("Notes"), app("Notebook")], vec![WindowEntry { id: 7, app_id: "notes".into(), title: "Notes".into(), workspace: 0 }]);
        l.type_text("note");
        let hit = |provider, name: &str| SearchHit { provider, app: format!("P{provider}"), id: name.into(), name: name.into(), description: String::new(), icon: None, clipboard: None };
        l.set_search(1, vec![hit(1, "a.txt"), hit(1, "b.txt")]);
        l.set_search(0, vec![hit(0, "x")]);
        let s = l.sections();
        use ListItem::{Heading as H, Row as R};
        assert_eq!(s, [H(Section::Windows), R(0), H(Section::Apps), R(1), R(2), H(Section::Provider(0)), R(3), R(4), H(Section::Provider(1)), R(5), R(6), R(7)]);
        assert!(matches!(l.results[4], Hit::More(0)) && matches!(l.results[7], Hit::More(_)), "Search in … ends each provider's section");
        // The window shown keeps the selection in sight.
        assert_eq!(list_start(&[30, 48, 48, 48], 3, 200), 0);
        assert_eq!(list_start(&[30, 48, 48, 48], 3, 100), 2);
        assert_eq!(list_start(&[30, 48, 48, 48], 0, 10), 0);
    }

    /// Items: a heading, five rows, a heading, three rows (30 and 48 px).
    fn list() -> (Vec<i32>, Vec<bool>) {
        let headings = [true, false, false, false, false, false, true, false, false, false];
        (headings.iter().map(|h| if *h { 30 } else { 48 }).collect(), headings.to_vec())
    }

    #[test]
    fn scroll_keeps_the_end_filled() {
        let (heights, _) = list();
        // 2 × 30 + 8 × 48 = 444.
        assert_eq!(last_start(&heights, 500), 0, "all fit");
        assert_eq!(last_start(&heights, 200), 6, "a heading and three rows");
        assert_eq!(last_start(&heights, 143), 8);
        assert_eq!(last_start(&[], 100), 0);
    }

    #[test]
    fn wheel_scrolls_within_the_list() {
        let (heights, headings) = list();
        let mut s = ListScroll::default();
        assert_eq!(s.place("a", &heights, &headings, 1, 200), 0, "the first row selected: the top");
        s.scroll_by(3);
        assert_eq!(s.place("a", &heights, &headings, 1, 200), 3, "the wheel moves it; the selection is left out of sight");
        s.scroll_by(10);
        assert_eq!(s.place("a", &heights, &headings, 1, 200), 6, "never past the end");
        s.scroll_by(-100);
        assert_eq!(s.place("a", &heights, &headings, 1, 200), 0, "never before the start");
    }

    #[test]
    fn keyboard_selection_stays_in_sight() {
        let (heights, headings) = list();
        let mut s = ListScroll::default();
        assert_eq!(s.place("a", &heights, &headings, 1, 200), 0);
        // Down to the fifth row (item 5): just enough to show it.
        assert_eq!(s.place("a", &heights, &headings, 5, 200), 2);
        // Back up to item 3: already in sight, nothing moves.
        assert_eq!(s.place("a", &heights, &headings, 3, 200), 2);
        // Scrolled away with the wheel, then the selection moves: back
        // to it, with its heading (item 6 then item 7).
        s.scroll_by(-2);
        assert_eq!(s.place("a", &heights, &headings, 3, 200), 0, "the wheel's place kept while the selection stays");
        assert_eq!(s.place("a", &heights, &headings, 7, 200), 4);
        s.scroll_by(2);
        assert_eq!(s.place("a", &heights, &headings, 7, 200), 6);
        assert_eq!(s.place("a", &heights, &headings, 1, 200), 0, "up to the first row: with its heading");
    }

    #[test]
    fn new_query_starts_at_the_top() {
        let (heights, headings) = list();
        let mut s = ListScroll::default();
        s.place("a", &heights, &headings, 1, 200);
        s.scroll_by(4);
        assert_eq!(s.place("a", &heights, &headings, 1, 200), 4);
        assert_eq!(s.place("ab", &heights, &headings, 1, 200), 0);
        assert_eq!(s.place("ab", &[], &[], 0, 200), 0, "no results");
    }

    #[test]
    fn touchpad_travel_in_whole_items() {
        let mut s = ListScroll::default();
        assert_eq!(s.travel(20.0, 48.0), 0);
        assert_eq!(s.travel(30.0, 48.0), 1, "50 px: one item, 2 px kept");
        assert_eq!(s.travel(94.0, 48.0), 2);
        assert_eq!(s.travel(-10.0, 48.0), 0, "turning back drops the rest");
        assert_eq!(s.travel(-40.0, 48.0), -1);
    }

    #[test]
    fn used_apps_first() {
        let app = |id: &str, name: &str| App { id: id.into(), name: name.into(), search: String::new(), categories: Vec::new() };
        let apps = vec![app("a.desktop", "Text Editor"), app("b.desktop", "Terminal"), app("c.desktop", "Tetris")];
        let usage = crate::app_usage::parse("c.desktop\t5\t100\nb.desktop\t1\t100\n");
        assert_eq!(search_with(&apps, "te", &usage, 100), [2, 1, 0], "same match: most used first");
        assert_eq!(search_with(&apps, "text", &usage, 100), [0], "a better match still wins");
    }

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
