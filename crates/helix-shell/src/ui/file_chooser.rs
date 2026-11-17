//! The file chooser dialog (helixdesktop/spec
//! `contracts/dbus/org.freedesktop.impl.portal.md`, "FileChooser",
//! "Helix"), shown for `org.helixdesktop.PortalDialog.ChooseFiles`.
//!
//! One dialog surface stays up for the whole choice: going into a folder
//! swaps the list in place while the folder is read on a worker thread
//! (`UiRequest::ListFiles`), with "Loading…" after a moment. The layout
//! comes from the screen's size alone, so the dialog neither moves nor
//! changes size between folders; only the rows that show are drawn.
//! Modelled on GTK 4's file chooser: a path bar, places, a column list
//! (name, size, modified) sorted by clicking the headers, type-ahead,
//! double-click, a file type menu, and in Save mode the name at the top.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use smithay_client_toolkit::reexports::client::protocol::wl_shm;
use smithay_client_toolkit::seat::keyboard::{KeyEvent, Keysym, Modifiers};
use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerEventKind};
use smithay_client_toolkit::shell::WaylandSurface;
use tokio::sync::oneshot;

use super::{accent, baseline, label, physical, theme, Canvas, DialogKind, DialogState, Overlay, OverlayHit, Rect, Ui, DIM_FG, ERROR_TEXT, FG, SCRIM};
use crate::file_chooser::{self as model, Chosen, Listing, Mode, Options, Request, TextEdit};
use crate::text::Text;
use helix_draw::theme::size;
use helix_draw::Image;
use helix_i18n::{tr, trf, trnf};
use helix_settings::files::{self, Entry, Filter, Place, PlaceKind, Sort, SortColumn};

/// GTK 4's file chooser settings (sort order, hidden files, the size
/// column, start in Recent, the clock).
const GTK_SCHEMA: &str = "org.gtk.gtk4.Settings.FileChooser";
const MOUSE_SCHEMA: &str = "org.gnome.desktop.peripherals.mouse";
/// Type-ahead text is forgotten after this long without a key.
const TYPEAHEAD_RESET: std::time::Duration = std::time::Duration::from_millis(1500);
/// Rows each wheel notch scrolls.
const NOTCH_ROWS: f64 = 3.0;
/// Up to this many rows every row is on the accessibility bus; above, the
/// rows on screen and those near them (each still telling its place in
/// the whole list).
const A11Y_ALL_ROWS: usize = 1000;
/// The places are read again this often while the dialog is open.
const PLACES_EVERY: std::time::Duration = std::time::Duration::from_secs(2);

/// A part of the file chooser.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum FilesHit {
    /// A row of the list (its place in the shown order).
    Row(usize),
    Place(usize),
    /// A path bar button (its index among the folders), and the button
    /// standing for the folders that do not fit.
    Crumb(usize),
    CrumbMore,
    Back,
    Forward,
    Search,
    NewFolder,
    Column(SortColumn),
    Scrollbar,
    /// The list or the places away from a row.
    List,
    Sidebar,
    Name,
    /// The field in the path bar's place (location, search, new folder).
    Field,
    FilterButton,
    FilterItem(usize),
    Check,
    ReplaceCancel,
    ReplaceConfirm,
    /// The dialog's edges and corners, for resizing it ([`EDGE_LEFT`]…
    /// together), and the edge between the places and the list.
    Resize(u8),
    SidebarEdge,
    /// The eject button of a place (a removable drive).
    Eject(usize),
}

/// Which edges a [`FilesHit::Resize`] moves.
pub(super) const EDGE_LEFT: u8 = 1;
pub(super) const EDGE_RIGHT: u8 = 2;
pub(super) const EDGE_TOP: u8 = 4;
pub(super) const EDGE_BOTTOM: u8 = 8;

/// A drag that resizes: the dialog by its `edges`, or the places.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Resizing {
    Dialog { edges: u8, from: (f64, f64), size: (i32, i32) },
    Sidebar { from: f64, width: i32 },
}

/// What the list shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Shown {
    Folder(PathBuf),
    Recent,
    Search { dir: PathBuf, query: String },
}

/// Where the keys go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Focus {
    List,
    Name,
    /// The path bar's field.
    Field,
    Check,
    Filter,
    Cancel,
    Accept,
}

/// What the path bar's field is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FieldUse {
    Location,
    Search,
    NewFolder,
}

/// Settings read when the dialog opens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct ChooserSettings {
    pub sort: Sort,
    pub hidden: bool,
    pub size_column: bool,
    pub start_in_recent: bool,
    pub twelve_hour: bool,
    pub double_click_ms: u32,
    /// The Type column, and how it names types.
    pub type_column: bool,
    pub type_format: model::TypeFormat,
    /// `date-format` `with-time`: dates show the time too.
    pub with_time: bool,
    /// The dialog's size and the places' width the user chose (logical
    /// px; `None`: from the screen).
    pub window_size: Option<(i32, i32)>,
    pub sidebar_width: Option<i32>,
}

impl Default for ChooserSettings {
    fn default() -> Self {
        Self {
            sort: Sort::default(),
            hidden: false,
            size_column: true,
            start_in_recent: true,
            twelve_hour: false,
            double_click_ms: 400,
            type_column: true,
            type_format: model::TypeFormat::Category,
            with_time: false,
            window_size: None,
            sidebar_width: None,
        }
    }
}

impl ChooserSettings {
    fn read() -> Self {
        let mut out = Self::default();
        if helix_settings::has_schema(GTK_SCHEMA) {
            let s = helix_settings::Settings::new(GTK_SCHEMA);
            let has = |k: &str| s.has_key(k);
            if has("sort-column") {
                out.sort.column = SortColumn::from_key(&s.string("sort-column"));
            }
            if has("sort-order") {
                out.sort.ascending = s.string("sort-order") != "descending";
            }
            if has("sort-directories-first") {
                out.sort.folders_first = s.boolean("sort-directories-first");
            }
            if has("show-hidden") {
                out.hidden = s.boolean("show-hidden");
            }
            if has("show-size-column") {
                out.size_column = s.boolean("show-size-column");
            }
            if has("startup-mode") {
                out.start_in_recent = s.string("startup-mode") == "recent";
            }
            if has("clock-format") {
                out.twelve_hour = s.string("clock-format") == "12h";
            }
            if has("show-type-column") {
                out.type_column = s.boolean("show-type-column");
            }
            if has("type-format") {
                out.type_format = model::TypeFormat::from_key(&s.string("type-format"));
            }
            if has("date-format") {
                out.with_time = s.string("date-format") == "with-time";
            }
            if has("window-size") {
                out.window_size = s.value("window-size").get::<(i32, i32)>().filter(|(w, h)| *w > 0 && *h > 0);
            }
            if has("sidebar-width") {
                out.sidebar_width = Some(s.int("sidebar-width")).filter(|w| *w > 0);
            }
        }
        if helix_settings::has_schema(MOUSE_SCHEMA) {
            let s = helix_settings::Settings::new(MOUSE_SCHEMA);
            if s.has_key("double-click") {
                out.double_click_ms = u32::try_from(s.int("double-click")).unwrap_or(400).clamp(100, 2000);
            }
        }
        out
    }

    /// Store the sort order and hidden files as GTK does.
    fn store(&self) {
        if !helix_settings::has_schema(GTK_SCHEMA) {
            return;
        }
        let s = helix_settings::Settings::new(GTK_SCHEMA);
        let _ = s.set_string("sort-column", self.sort.column.key());
        let _ = s.set_string("sort-order", if self.sort.ascending { "ascending" } else { "descending" });
        let _ = s.set_boolean("show-hidden", self.hidden);
    }

    /// Store the dialog's size and the places' width as GTK does.
    fn store_size(&self) {
        if !helix_settings::has_schema(GTK_SCHEMA) {
            return;
        }
        let s = helix_settings::Settings::new(GTK_SCHEMA);
        if let Some(size) = self.window_size {
            let _ = s.set_value("window-size", &gio::glib::variant::ToVariant::to_variant(&size));
        }
        if let Some(w) = self.sidebar_width {
            let _ = s.set_int("sidebar-width", w);
        }
    }
}

/// The file chooser on screen.
pub(super) struct ChooserView {
    pub opts: Options,
    reply: Option<oneshot::Sender<Option<Chosen>>>,
    pub home: PathBuf,
    pub settings: ChooserSettings,
    pub shown: Shown,
    back: Vec<Shown>,
    forward: Vec<Shown>,
    /// The deepest folder of the branch the path bar shows.
    pub crumb_tail: PathBuf,
    pub entries: Vec<Entry>,
    /// The rows shown: indexes into `entries`, in order.
    pub rows: Vec<usize>,
    /// The current row (keyboard focus and, without `multiple`, the
    /// selection); `None`: nothing selected.
    pub current: Option<usize>,
    /// Where a Shift selection started (a row).
    anchor: Option<usize>,
    /// With `multiple`: the entries picked.
    pub picked: BTreeSet<usize>,
    /// The listing asked for and not answered yet, and since when.
    pub loading: Option<(u64, std::time::Instant)>,
    serial: u64,
    loading_said: bool,
    pub error: Option<String>,
    /// The entry to select once the listing comes (by name).
    reselect: Option<String>,
    /// How far the list and the places are scrolled (logical px).
    pub scroll: f64,
    pub places_scroll: f64,
    pub focus: Focus,
    pub name: TextEdit,
    pub field: Option<(FieldUse, TextEdit)>,
    pub filter: Option<usize>,
    pub check: bool,
    pub dropdown: bool,
    /// The filter highlighted in the open menu.
    dropdown_at: usize,
    typeahead: String,
    typeahead_at: Option<std::time::Instant>,
    /// The last press on a row: its entry and when (ms).
    last_press: Option<(usize, u32)>,
    pub warning: String,
    /// Asking before replacing this file.
    pub replace: Option<PathBuf>,
    /// Previews asked for, by file (`None` until decoded or when it
    /// cannot be shown).
    pub previews: HashMap<String, Option<crate::Preview>>,
    /// Previews answered (some with nothing to show).
    pub previews_done: std::collections::HashSet<String>,
    /// Dragging the scrollbar: where on the thumb it was taken.
    dragging: Option<f64>,
    /// From the last paint: the list's rows height and the visible
    /// height (for keeping the current row in view).
    row_h: i32,
    body_h: i32,
    /// A folder being made (serial).
    making: Option<u64>,
    /// The parts last logged (for tests driving the dialog).
    logged_hits: String,
    /// The dialog's size and the places' width as last drawn, and a
    /// resizing drag.
    shown_size: (i32, i32),
    shown_sidebar: i32,
    resizing: Option<Resizing>,
    /// Mounted media by folder: their roots (URIs), for the eject
    /// buttons.
    pub mounts: Vec<(PathBuf, String)>,
    /// When the places were last asked for.
    places_asked: Option<std::time::Instant>,
}

impl ChooserView {
    pub(super) fn new(request: Request, settings: ChooserSettings, home: PathBuf) -> Self {
        let opts = request.options;
        let start = match &opts.folder {
            Some(f) => Shown::Folder(f.clone()),
            None if opts.mode == Mode::Open && settings.start_in_recent && opts.places.iter().any(|p| p.kind == PlaceKind::Recent) => Shown::Recent,
            None => Shown::Folder(home.clone()),
        };
        let name = if opts.mode == Mode::Save { TextEdit::selecting(&opts.name, 0, model::base_name_end(&opts.name)) } else { TextEdit::default() };
        let focus = if opts.mode == Mode::Save { Focus::Name } else { Focus::List };
        let crumb_tail = match &start {
            Shown::Folder(f) => f.clone(),
            _ => home.clone(),
        };
        Self {
            filter: opts.filter,
            check: opts.check.as_ref().is_some_and(|c| c.1),
            opts,
            reply: Some(request.reply),
            home,
            settings,
            shown: start,
            back: Vec::new(),
            forward: Vec::new(),
            crumb_tail,
            entries: Vec::new(),
            rows: Vec::new(),
            current: None,
            anchor: None,
            picked: BTreeSet::new(),
            loading: None,
            serial: 0,
            loading_said: false,
            error: None,
            reselect: None,
            scroll: 0.0,
            places_scroll: 0.0,
            focus,
            name,
            field: None,
            dropdown: false,
            dropdown_at: 0,
            typeahead: String::new(),
            typeahead_at: None,
            last_press: None,
            warning: String::new(),
            replace: None,
            previews: HashMap::new(),
            previews_done: Default::default(),
            dragging: None,
            row_h: 32,
            body_h: 320,
            making: None,
            logged_hits: String::new(),
            shown_size: (0, 0),
            shown_sidebar: 0,
            resizing: None,
            mounts: Vec::new(),
            places_asked: Some(std::time::Instant::now()),
        }
    }

    /// The root (a URI) of the mount place `i` is, when it can be
    /// ejected or unmounted: removable drives and network places.
    pub(super) fn eject_root(&self, i: usize) -> Option<&str> {
        let place = self.opts.places.get(i).filter(|p| matches!(p.kind, PlaceKind::Drive | PlaceKind::Network))?;
        let path = place.path.as_ref()?;
        self.mounts.iter().find(|(p, _)| p == path).map(|(_, root)| root.as_str())
    }

    /// Answer (once): `None` cancels.
    fn answer(&mut self, chosen: Option<Chosen>) {
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(chosen);
        }
    }

    pub(super) fn cancel(&mut self) {
        self.answer(None);
    }

    fn filter_now(&self) -> Option<&Filter> {
        self.filter.and_then(|i| self.opts.filters.get(i))
    }

    /// The folder shown, when one is.
    pub(super) fn folder(&self) -> Option<&Path> {
        match &self.shown {
            Shown::Folder(f) => Some(f),
            _ => None,
        }
    }

    pub(super) fn current_entry(&self) -> Option<&Entry> {
        self.current.and_then(|r| self.rows.get(r)).and_then(|i| self.entries.get(*i))
    }

    /// Work the rows out again (after a listing, a filter, the sort or
    /// hidden files changed), keeping the current entry.
    pub(super) fn refresh_rows(&mut self) {
        let keep = self.reselect.take().or_else(|| self.current_entry().map(|e| e.name.clone()));
        let mode = match self.shown {
            // Recent files and search results are files whatever is chosen.
            Shown::Folder(_) => self.opts.mode,
            _ => Mode::Open,
        };
        let sort = match self.shown {
            Shown::Folder(_) => self.settings.sort,
            // Newest first; search results as found.
            Shown::Recent => Sort { column: SortColumn::Modified, ascending: false, folders_first: false },
            Shown::Search { .. } => self.settings.sort,
        };
        let filter = self.filter_now().cloned();
        self.rows = model::rows_typed(&self.entries, mode, filter.as_ref(), self.settings.hidden, sort, self.settings.type_format);
        self.picked.retain(|e| self.rows.contains(e));
        self.current = keep.and_then(|n| self.rows.iter().position(|&i| self.entries[i].name == n));
        if self.current.is_none() && matches!(self.opts.mode, Mode::Open) && !self.rows.is_empty() {
            self.current = Some(0);
        }
        self.anchor = self.current;
        let max = self.max_scroll();
        self.scroll = self.scroll.clamp(0.0, max);
        self.keep_in_view();
    }

    fn content_h(&self) -> f64 {
        self.rows.len() as f64 * f64::from(self.row_h)
    }

    fn max_scroll(&self) -> f64 {
        (self.content_h() - f64::from(self.body_h)).max(0.0)
    }

    /// Scroll so the current row shows whole.
    fn keep_in_view(&mut self) {
        let Some(r) = self.current else { return };
        let (top, bottom) = (r as f64 * f64::from(self.row_h), (r + 1) as f64 * f64::from(self.row_h));
        if top < self.scroll {
            self.scroll = top;
        } else if bottom > self.scroll + f64::from(self.body_h) {
            self.scroll = bottom - f64::from(self.body_h);
        }
        self.scroll = self.scroll.clamp(0.0, self.max_scroll());
    }

    pub(super) fn scroll_by(&mut self, px: f64) {
        self.scroll = (self.scroll + px).clamp(0.0, self.max_scroll());
    }

    /// Rows a page holds.
    fn page(&self) -> usize {
        usize::try_from(self.body_h / self.row_h.max(1)).unwrap_or(1).max(1)
    }

    /// Make row `r` current; with `extend` (Shift) pick the rows from the
    /// anchor to it, with `toggle` (Ctrl) flip it, else (with `multiple`)
    /// pick just it.
    pub(super) fn select(&mut self, r: usize, extend: bool, toggle: bool) {
        if r >= self.rows.len() {
            return;
        }
        self.current = Some(r);
        if self.opts.multiple {
            let entry = self.rows[r];
            let file = !self.entries[entry].dir;
            if extend {
                let a = self.anchor.unwrap_or(r);
                let (lo, hi) = (a.min(r), a.max(r));
                self.picked = (lo..=hi).map(|i| self.rows[i]).filter(|&e| !self.entries[e].dir).collect();
            } else if toggle {
                if file && !self.picked.remove(&entry) {
                    self.picked.insert(entry);
                }
                self.anchor = Some(r);
            } else {
                self.picked.clear();
                if file {
                    self.picked.insert(entry);
                }
                self.anchor = Some(r);
            }
        } else {
            self.anchor = Some(r);
        }
        self.keep_in_view();
    }

    /// Move the current row by `by` rows (clamped).
    fn move_by(&mut self, by: isize, extend: bool) {
        if self.rows.is_empty() {
            return;
        }
        let to = match self.current {
            Some(r) => r.saturating_add_signed(by).min(self.rows.len() - 1),
            None if by < 0 => self.rows.len() - 1,
            None => 0,
        };
        self.select(to, extend, false);
    }

    /// Type-ahead: `c` typed with the list focused.
    fn type_ahead(&mut self, text: &str, now: std::time::Instant) {
        if self.typeahead_at.is_none_or(|t| now.duration_since(t) > TYPEAHEAD_RESET) {
            self.typeahead.clear();
        }
        self.typeahead.push_str(text);
        self.typeahead_at = Some(now);
        // The same letter again goes to the next match; more letters
        // narrow from the current row.
        let repeat = self.typeahead.chars().count() > 1 && self.typeahead.chars().all(|c| self.typeahead.starts_with(c));
        let (typed, from) = if repeat { (text.to_owned(), self.current.map_or(0, |r| r + 1)) } else if self.typeahead.chars().count() == 1 { (self.typeahead.clone(), self.current.map_or(0, |r| r + 1)) } else { (self.typeahead.clone(), self.current.unwrap_or(0)) };
        if let Some(r) = model::typeahead(&self.entries, &self.rows, &typed, from) {
            self.select(r, false, false);
        }
    }

    /// The type-ahead text to show (while it is fresh).
    pub(super) fn typeahead_shown(&self, now: std::time::Instant) -> Option<&str> {
        self.typeahead_at.filter(|t| now.duration_since(*t) <= TYPEAHEAD_RESET).map(|_| self.typeahead.as_str()).filter(|t| !t.is_empty())
    }

    /// The place the sidebar marks: the one shown, else the deepest one
    /// holding the folder shown (Computer only when nothing else does).
    pub(super) fn current_place(&self) -> Option<usize> {
        match &self.shown {
            Shown::Recent => self.opts.places.iter().position(|p| p.kind == PlaceKind::Recent),
            Shown::Search { .. } => None,
            Shown::Folder(f) => self
                .opts
                .places
                .iter()
                .enumerate()
                .filter_map(|(i, p)| p.path.as_ref().filter(|path| f.starts_with(path)).map(|path| (i, path.components().count())))
                .max_by_key(|(_, depth)| *depth)
                .map(|(i, _)| i),
        }
    }

    /// The path bar's folders: (label, path, is the folder shown).
    pub(super) fn crumbs(&self) -> Vec<(String, PathBuf, bool)> {
        let Some(folder) = self.folder() else { return Vec::new() };
        let tail = if self.crumb_tail.starts_with(folder) { self.crumb_tail.clone() } else { folder.to_path_buf() };
        model::crumbs(&tail, &self.home)
            .into_iter()
            .enumerate()
            .map(|(i, (l, p))| {
                let l = match (i, l.is_empty()) {
                    (0, true) if p == self.home => tr("Home").to_owned(),
                    (0, true) => tr("Computer").to_owned(),
                    _ => l,
                };
                let here = p == folder;
                (l, p, here)
            })
            .collect()
    }

    /// The chosen files, picked or current, that are files.
    fn chosen_files(&self) -> Vec<PathBuf> {
        if self.opts.multiple && !self.picked.is_empty() {
            return self.rows.iter().filter(|e| self.picked.contains(e)).map(|&e| self.entries[e].path.clone()).collect();
        }
        self.current_entry().filter(|e| !e.dir).map(|e| vec![e.path.clone()]).unwrap_or_default()
    }

    fn outcome(&self, paths: Vec<PathBuf>) -> Chosen {
        let folder = match &self.shown {
            Shown::Folder(f) => f.clone(),
            Shown::Search { dir, .. } => dir.clone(),
            Shown::Recent => paths.first().and_then(|p| p.parent()).map_or_else(|| self.home.clone(), Path::to_path_buf),
        };
        Chosen { paths, folder, filter: self.filter, check: self.check }
    }

    /// The folder files are saved into.
    fn save_folder(&self) -> PathBuf {
        match &self.shown {
            Shown::Folder(f) => f.clone(),
            _ => self.current_entry().and_then(|e| e.path.parent()).map_or_else(|| self.home.clone(), Path::to_path_buf),
        }
    }

    /// The list's accessible name.
    pub(super) fn list_name(&self) -> String {
        match &self.shown {
            Shown::Folder(f) => {
                let name = if f == &self.home { tr("Home").to_owned() } else { f.file_name().map_or_else(|| f.display().to_string(), |n| n.to_string_lossy().into_owned()) };
                trf!("Files in {folder}", folder = name)
            }
            Shown::Recent => tr("Recent Files").to_owned(),
            Shown::Search { query, .. } => trf!("Search results for “{query}”", query = query),
        }
    }

    /// What the dialog says about the list now (read out when it
    /// changes).
    pub(super) fn status(&self, now: std::time::Instant) -> String {
        if self.loading.is_some_and(|(_, since)| now.duration_since(since) >= model::LOADING_AFTER) {
            return tr("Loading…").to_owned();
        }
        if self.loading.is_some() {
            return String::new();
        }
        let what = match &self.shown {
            Shown::Folder(f) => f.file_name().map_or_else(|| f.display().to_string(), |n| n.to_string_lossy().into_owned()),
            Shown::Recent => tr("Recent").to_owned(),
            Shown::Search { query, .. } => query.clone(),
        };
        trnf!("{what}, {n} item", "{what}, {n} items", self.rows.len(), what = what)
    }
}

impl Drop for ChooserView {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// The parts' places, from the screen's size (`w`×`h`), the text scale
/// `k` and what the dialog has; nothing in it depends on the folder
/// shown.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(super) struct Layout {
    pub dialog: Rect,
    pub title_base: i32,
    pub back: Rect,
    pub forward: Rect,
    /// The path bar (or its field).
    pub bar: Rect,
    pub search: Option<Rect>,
    pub new_folder: Option<Rect>,
    /// Save: the name field (its label left of it).
    pub name: Option<Rect>,
    pub sidebar: Rect,
    pub header: Rect,
    pub body: Rect,
    pub row_h: i32,
    pub preview: Option<Rect>,
    pub footer: Rect,
    pub cancel: Rect,
    pub accept: Rect,
}

/// What the layout makes room for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct Wants {
    pub save: bool,
    pub search: bool,
    pub new_folder: bool,
    pub preview: bool,
    /// The size the user gave the dialog and the places (logical px).
    pub size: Option<(i32, i32)>,
    pub sidebar: Option<i32>,
}

pub(super) fn layout(w: i32, h: i32, k: f32, wants: Wants) -> Layout {
    let g = |v: i32| (v as f32 * k).round() as i32;
    // From the screen, or as the user sized it (never smaller than the
    // smallest from the screen, nor off it).
    let (min_w, min_h) = (600.min(w - 24), 440.min(h - 24));
    let (dw, dh) = match wants.size {
        Some((sw, sh)) => (sw.clamp(min_w, (w - 24).max(min_w)).max(0), sh.clamp(min_h, (h - 24).max(min_h)).max(0)),
        None => (
            ((w as f32 * 0.8).round() as i32).clamp(min_w, 1180).min(w - 24).max(0),
            ((h as f32 * 0.85).round() as i32).clamp(min_h, 900).min(h - 24).max(0),
        ),
    };
    let (dx, dy) = ((w - dw) / 2, (h - dh) / 2);
    let pad = 16;
    let title_base = dy + pad + g(20);
    let ty = dy + pad + g(26) + 8;
    let th = g(34);
    let back = Rect { x: dx + pad, y: ty, w: th, h: th };
    let forward = Rect { x: back.x + th + 4, y: ty, w: th, h: th };
    let mut right = dx + dw - pad;
    let mut button = |on: bool| {
        on.then(|| {
            right -= th;
            let r = Rect { x: right, y: ty, w: th, h: th };
            right -= 4;
            r
        })
    };
    let new_folder = button(wants.new_folder);
    let search = button(wants.search);
    let bar_x = forward.x + th + 8;
    let bar = Rect { x: bar_x, y: ty, w: (right - 4 - bar_x).max(0), h: th };
    let mut below = ty + th;
    let name = wants.save.then(|| {
        let label_w = g(56);
        let r = Rect { x: dx + pad + label_w, y: below + 8, w: dw - 2 * pad - label_w, h: g(34) };
        below = r.y + r.h;
        r
    });
    let content_top = below + 12;
    let fh = g(36);
    let footer = Rect { x: dx + pad, y: dy + dh - pad - fh, w: dw - 2 * pad, h: fh };
    let bw = g(104);
    let accept = Rect { x: footer.x + footer.w - bw, y: footer.y, w: bw, h: fh };
    let cancel = Rect { x: accept.x - 8 - bw, y: footer.y, w: bw, h: fh };
    let content_bottom = footer.y - 12;
    let ch = (content_bottom - content_top).max(0);
    let sidebar_w = match wants.sidebar {
        Some(sw) => sw.clamp(g(120).min(dw / 3), dw / 3),
        None => g(172).min(dw / 4),
    };
    let sidebar = Rect { x: dx + pad, y: content_top, w: sidebar_w, h: ch };
    let lx = sidebar.x + sidebar.w + 12;
    let lw = (dx + dw - pad - lx).max(0);
    let pw = if wants.preview { ((lw as f32 * 0.3) as i32).clamp(g(160).min(lw / 2), 320).min(lw / 2) } else { 0 };
    let list_w = if pw > 0 { lw - pw - 12 } else { lw };
    let preview = (pw > 0).then(|| Rect { x: lx + list_w + 12, y: content_top, w: pw, h: ch });
    let header_h = g(28).min(ch);
    let header = Rect { x: lx, y: content_top, w: list_w, h: header_h };
    let body = Rect { x: lx, y: content_top + header_h, w: list_w, h: ch - header_h };
    Layout { dialog: Rect { x: dx, y: dy, w: dw, h: dh }, title_base, back, forward, bar, search, new_folder, name, sidebar, header, body, row_h: g(32), preview, footer, cancel, accept }
}

/// `s` shortened in its middle to fit `room` (keeping the end, so the
/// extension and the end of a long name show).
pub(super) fn middle_ellipsis(t: &Text, s: &str, px: f32, room: f32) -> String {
    if t.width(s, px) <= room {
        return s.to_owned();
    }
    let chars: Vec<char> = s.chars().collect();
    let ext = s.rfind('.').map_or(0, |i| s[i..].chars().count());
    let candidate = |n: usize| -> String {
        let tail = (n / 2).max(ext + 3).min(n);
        let head = n - tail;
        let mut out: String = chars[..head].iter().collect();
        out.push('…');
        out.extend(&chars[chars.len() - tail..]);
        out
    };
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if t.width(&candidate(mid), px) <= room {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    candidate(lo)
}

/// The columns' places in the list: name (from `x`), the size or
/// location column, the modified column (each `None` when there is no
/// room).
#[derive(Debug, Clone, Copy)]
struct Columns {
    name: (i32, i32),
    second: Option<(i32, i32)>,
    /// The Type column.
    kind: Option<(i32, i32)>,
    modified: Option<(i32, i32)>,
}

/// `modified_w` and `type_w` (`None`: no Type column) as wide as their
/// texts need.
fn columns(body: Rect, k: f32, second_w: i32, has_second: bool, modified_w: i32, type_w: Option<i32>) -> Columns {
    let g = |v: i32| (v as f32 * k).round() as i32;
    let right = body.x + body.w - 14;
    let left = body.x + 8;
    let mut x = right;
    let modified = (body.w >= g(300)).then(|| {
        x -= modified_w;
        (x, modified_w)
    });
    // Type (GTK's order: name, size, type, modified), with room left for
    // the name.
    let kind = type_w.filter(|tw| body.w >= g(300) + tw + g(220) + if has_second { second_w } else { 0 }).map(|tw| {
        x -= tw + 12;
        (x, tw)
    });
    let second = (has_second && body.w >= g(300) + second_w + g(140)).then(|| {
        x -= second_w + 12;
        (x, second_w)
    });
    Columns { name: (left, (x - 12 - left).max(0)), second, kind, modified }
}

/// The places' groups (a separator between groups).
fn place_group(kind: PlaceKind) -> u8 {
    match kind {
        PlaceKind::Recent | PlaceKind::Home | PlaceKind::Standard => 0,
        PlaceKind::Bookmark => 1,
        PlaceKind::Drive | PlaceKind::Network => 2,
        PlaceKind::Computer => 3,
    }
}

/// A `type-format` (for the gallery).
#[cfg(test)]
pub(super) fn model_type_format(key: &str) -> model::TypeFormat {
    model::TypeFormat::from_key(key)
}

/// Mounted media by folder (where their roots are local folders, FUSE
/// ones too): (folder, root URI).
fn mount_paths(mounts: &[crate::media::MountInfo]) -> Vec<(PathBuf, String)> {
    use gio::prelude::*;
    mounts.iter().filter(|m| m.mounted).filter_map(|m| Some((gio::File::for_uri(&m.root).path()?, m.root.clone()))).collect()
}

/// A place's name.
pub(super) fn place_label(p: &Place) -> String {
    match p.kind {
        PlaceKind::Recent => tr("Recent").to_owned(),
        PlaceKind::Home => tr("Home").to_owned(),
        PlaceKind::Computer => tr("Computer").to_owned(),
        _ => p.label.clone(),
    }
}

/// A place's icon.
fn place_icon(p: &Place, home: &Path) -> &'static str {
    use gio::glib::UserDirectory as D;
    let special = |d: D| gio::glib::user_special_dir(d).is_some_and(|s| p.path.as_deref() == Some(s.as_path()) && s != home);
    match p.kind {
        PlaceKind::Recent => "document-open-recent-symbolic",
        PlaceKind::Home => "user-home-symbolic",
        PlaceKind::Computer => "drive-harddisk-symbolic",
        PlaceKind::Drive => "drive-removable-media-symbolic",
        PlaceKind::Network => "folder-remote-symbolic",
        PlaceKind::Bookmark => "folder-symbolic",
        PlaceKind::Standard if special(D::Desktop) => "user-desktop-symbolic",
        PlaceKind::Standard if special(D::Documents) => "folder-documents-symbolic",
        PlaceKind::Standard if special(D::Downloads) => "folder-download-symbolic",
        PlaceKind::Standard if special(D::Music) => "folder-music-symbolic",
        PlaceKind::Standard if special(D::Pictures) => "folder-pictures-symbolic",
        PlaceKind::Standard if special(D::Videos) => "folder-videos-symbolic",
        PlaceKind::Standard => "folder-symbolic",
    }
}

/// Fonts and icons for painting.
pub(super) struct Paint<'a> {
    pub text: Option<&'a Text>,
    pub bold: Option<&'a Text>,
    /// A symbolic icon by name, in a color (`None` while it loads).
    pub icon: &'a mut dyn FnMut(&str, u32) -> Option<Image>,
    pub now: std::time::Instant,
    /// Unix seconds now (for the dates).
    pub now_unix: i64,
}

/// What a paint shows.
pub(super) struct Painted {
    pub hits: Vec<(Rect, OverlayHit)>,
    /// The rows drawn.
    pub drawn: std::ops::Range<usize>,
    pub loading_shown: bool,
}

/// The wants for `view`.
pub(super) fn wants(view: &ChooserView) -> Wants {
    let mode = view.opts.mode;
    // The preview pane shows beside the list while the file selected is
    // of a type that can be previewed.
    let preview = mode.lists_files() && view.current_entry().is_some_and(files::previewable);
    Wants { save: mode == Mode::Save, search: mode == Mode::Open, new_folder: mode != Mode::Open, preview, size: view.settings.window_size, sidebar: view.settings.sidebar_width }
}

fn hit(h: FilesHit) -> OverlayHit {
    OverlayHit::Files(h)
}

/// Draw a one-line field's text (scrolled to keep the caret in view),
/// its selection and caret.
fn field_text(canvas: &mut Canvas, text: Option<&Text>, r: Rect, edit: &TextEdit, focused: bool, placeholder: &str) {
    super::text_field(canvas, r, focused);
    let Some(t) = text else { return };
    let px = size::LABEL;
    let room = (r.w - 20) as f32;
    let at = |s: &str| t.width(s, px);
    let caret_x = at(edit.before_caret());
    let offset = (caret_x - room).max(0.0);
    let x0 = (r.x + 10) as f32 - offset;
    let base = baseline(text, r.y, r.h, px);
    let clip = canvas.push_clip(r.x + 6, r.y, r.w - 12, r.h);
    if edit.text.is_empty() && !placeholder.is_empty() {
        label(text, canvas, placeholder, (r.x + 10) as f32, base, px, super::hint_text(), room);
    }
    if let Some((a, b)) = edit.selection().filter(|_| focused) {
        let (xa, xb) = (x0 + at(edit.before(a)), x0 + at(edit.before(b)));
        let hh = (px * 1.5).round() as i32;
        canvas.round_rect(f64::from(xa), f64::from(r.y + (r.h - hh) / 2), f64::from(xb - xa), f64::from(hh), 2.0, (accent() & 0x00FF_FFFF) | 0x8000_0000);
    }
    label(text, canvas, &edit.text, x0, base, px, FG, f32::MAX);
    if focused {
        super::caret(canvas, r, (x0 + caret_x) as i32 + 1, px);
    }
    canvas.restore_clip(clip);
}

/// A small square button with a symbolic icon (dimmed when `off`).
fn icon_button(canvas: &mut Canvas, p: &mut Paint<'_>, r: Rect, icon: &str, look: theme::Look, off: bool, focused: bool) {
    let rect = (f64::from(r.x), f64::from(r.y), f64::from(r.w), f64::from(r.h));
    if !off {
        theme::flat(canvas, rect, theme::RADIUS_BUTTON, look);
    }
    let color = if off { theme::disabled_text() } else { FG };
    if let Some(img) = (p.icon)(icon, color) {
        canvas.image(f64::from(r.x + (r.w - 16) / 2), f64::from(r.y + (r.h - 16) / 2), 16.0, 16.0, &img);
    }
    if focused {
        theme::focus_ring_round(canvas, rect.0, rect.1, rect.2, rect.3, theme::RADIUS_BUTTON);
    }
}

/// Paint the chooser over a scrim on a `w`×`h` surface. `hover` and
/// `pressed` are the parts under the pointer and held.
pub(super) fn paint_files(canvas: &mut Canvas, p: &mut Paint<'_>, (w, h): (usize, usize), view: &ChooserView, (hover, pressed): (Option<OverlayHit>, Option<OverlayHit>)) -> Painted {
    canvas.fill(SCRIM);
    let k = theme::text_scale();
    let g = |v: i32| (v as f32 * k).round() as i32;
    let l = layout(w as i32, h as i32, k, wants(view));
    let (text, bold) = (p.text, p.bold.or(p.text));
    let look = |x: FilesHit| theme::Look { hovered: hover == Some(hit(x)), pressed: pressed == Some(hit(x)), on: false };
    let modal = view.replace.is_some();
    let mut hits: Vec<(Rect, OverlayHit)> = Vec::new();
    let d = l.dialog;
    // The edges and corners resize the dialog (first: they take presses
    // before the parts they overlap).
    if !modal {
        let (band, corner) = (6, 14);
        let (l_, r_, t_, b_) = (EDGE_LEFT, EDGE_RIGHT, EDGE_TOP, EDGE_BOTTOM);
        for (r, edges) in [
            (Rect { x: d.x - band, y: d.y - band, w: corner + band, h: corner + band }, l_ | t_),
            (Rect { x: d.x + d.w - corner, y: d.y - band, w: corner + band, h: corner + band }, r_ | t_),
            (Rect { x: d.x - band, y: d.y + d.h - corner, w: corner + band, h: corner + band }, l_ | b_),
            (Rect { x: d.x + d.w - corner, y: d.y + d.h - corner, w: corner + band, h: corner + band }, r_ | b_),
            (Rect { x: d.x - band, y: d.y, w: 2 * band, h: d.h }, l_),
            (Rect { x: d.x + d.w - band, y: d.y, w: 2 * band, h: d.h }, r_),
            (Rect { x: d.x, y: d.y - band, w: d.w, h: 2 * band }, t_),
            (Rect { x: d.x, y: d.y + d.h - band, w: d.w, h: 2 * band }, b_),
        ] {
            hits.push((r, hit(FilesHit::Resize(edges))));
        }
        // Between the places and the list.
        let sb = l.sidebar;
        hits.push((Rect { x: sb.x + sb.w + 1, y: sb.y, w: 10, h: sb.h }, hit(FilesHit::SidebarEdge)));
    }
    theme::popover(canvas, d.x, d.y, d.w, d.h);
    // A grip at the bottom corner: the dialog can be resized.
    for i in 1..=3 {
        let o = f64::from(4 * i + 2);
        let (cx, cy) = (f64::from(d.x + d.w) - 6.0, f64::from(d.y + d.h) - 6.0);
        canvas.line((cx - o, cy), (cx, cy - o), 1.0, 0x40FF_FFFF);
    }
    let inner = (d.w - 32) as f32;
    // The title.
    let tw = bold.map_or(0.0, |t| t.width(&view.opts.title, size::TITLE).min(inner));
    label(bold, canvas, &view.opts.title, d.x as f32 + (d.w as f32 - tw) / 2.0, l.title_base as f32, size::TITLE, FG, inner);

    // ----- Toolbar: back, forward, the path bar, search, new folder.
    let no_back = view.back.is_empty();
    let no_forward = view.forward.is_empty();
    icon_button(canvas, p, l.back, "go-previous-symbolic", look(FilesHit::Back), no_back, false);
    icon_button(canvas, p, l.forward, "go-next-symbolic", look(FilesHit::Forward), no_forward, false);
    hits.push((l.back, hit(FilesHit::Back)));
    hits.push((l.forward, hit(FilesHit::Forward)));
    if let Some(r) = l.search {
        let on = matches!(view.field, Some((FieldUse::Search, _))) || matches!(view.shown, Shown::Search { .. });
        icon_button(canvas, p, r, "edit-find-symbolic", theme::Look { on, ..look(FilesHit::Search) }, false, false);
        hits.push((r, hit(FilesHit::Search)));
    }
    if let Some(r) = l.new_folder {
        let on = matches!(view.field, Some((FieldUse::NewFolder, _)));
        icon_button(canvas, p, r, "folder-new-symbolic", theme::Look { on, ..look(FilesHit::NewFolder) }, false, false);
        hits.push((r, hit(FilesHit::NewFolder)));
    }
    let bar = l.bar;
    if let Some((what, edit)) = &view.field {
        let placeholder = match what {
            FieldUse::Location => tr("Type a file name or location"),
            FieldUse::Search => tr("Search files"),
            FieldUse::NewFolder => tr("New folder name"),
        };
        field_text(canvas, text, bar, edit, view.focus == Focus::Field && !modal, placeholder);
        hits.push((bar, hit(FilesHit::Field)));
    } else {
        match &view.shown {
            Shown::Folder(_) => {
                let crumbs = view.crumbs();
                let px = size::LABEL;
                let widths: Vec<i32> = crumbs.iter().enumerate().map(|(i, (lab, _, _))| bold.map_or(60, |t| t.width(lab, px).ceil() as i32) + 20 + if i == 0 { 22 } else { 0 }).collect();
                let more_w = l.bar.h;
                let here = crumbs.iter().position(|c| c.2).unwrap_or(0);
                // The last folders that fit (the one shown among them).
                let mut end = crumbs.len();
                let fits = |s: usize, e: usize| widths[s..e].iter().sum::<i32>() + if s > 0 { more_w + 4 } else { 0 } + (e - s) as i32 * 4 <= bar.w;
                let mut start = 0;
                while start < here && !fits(start, end) {
                    start += 1;
                }
                while end > here + 1 && !fits(start, end) {
                    end -= 1;
                }
                let mut x = bar.x;
                if start > 0 {
                    let r = Rect { x, y: bar.y, w: more_w, h: bar.h };
                    theme::flat(canvas, (f64::from(r.x), f64::from(r.y), f64::from(r.w), f64::from(r.h)), theme::RADIUS_BUTTON, look(FilesHit::CrumbMore));
                    let tw = text.map_or(0.0, |t| t.width("…", px));
                    label(text, canvas, "…", r.x as f32 + (r.w as f32 - tw) / 2.0, baseline(text, r.y, r.h, px), px, FG, r.w as f32);
                    hits.push((r, hit(FilesHit::CrumbMore)));
                    x += more_w + 4;
                }
                for (i, (lab, _, here)) in crumbs.iter().enumerate().take(end).skip(start) {
                    let cw = widths[i].min(bar.x + bar.w - x);
                    if cw <= 0 {
                        break;
                    }
                    let r = Rect { x, y: bar.y, w: cw, h: bar.h };
                    let rect = (f64::from(r.x), f64::from(r.y), f64::from(r.w), f64::from(r.h));
                    let lk = look(FilesHit::Crumb(i));
                    if *here {
                        theme::control_round(canvas, rect, theme::RADIUS_BUTTON, lk);
                    } else {
                        theme::flat(canvas, rect, theme::RADIUS_BUTTON, lk);
                    }
                    let mut tx = r.x + 10;
                    if i == 0 {
                        let icon = if crumbs[0].1 == view.home { "user-home-symbolic" } else { "drive-harddisk-symbolic" };
                        if let Some(img) = (p.icon)(icon, FG) {
                            canvas.image(f64::from(tx), f64::from(r.y + (r.h - 16) / 2), 16.0, 16.0, &img);
                        }
                        tx += 22;
                    }
                    let font = if *here { bold } else { text };
                    label(font, canvas, lab, tx as f32, baseline(text, r.y, r.h, px), px, if *here { FG } else { DIM_FG }, (r.x + r.w - tx - 8) as f32);
                    hits.push((r, hit(FilesHit::Crumb(i))));
                    x += cw + 4;
                }
            }
            Shown::Recent | Shown::Search { .. } => {
                let (icon, lab) = match &view.shown {
                    Shown::Recent => ("document-open-recent-symbolic", tr("Recent").to_owned()),
                    Shown::Search { query, .. } => ("edit-find-symbolic", trf!("Search for “{query}”", query = query)),
                    Shown::Folder(_) => unreachable!(),
                };
                let px = size::LABEL;
                let lw = bold.map_or(80, |t| t.width(&lab, px).ceil() as i32).min(bar.w - 40);
                let r = Rect { x: bar.x, y: bar.y, w: lw + 42, h: bar.h };
                theme::control_round(canvas, (f64::from(r.x), f64::from(r.y), f64::from(r.w), f64::from(r.h)), theme::RADIUS_BUTTON, theme::Look::default());
                if let Some(img) = (p.icon)(icon, FG) {
                    canvas.image(f64::from(r.x + 10), f64::from(r.y + (r.h - 16) / 2), 16.0, 16.0, &img);
                }
                label(bold, canvas, &lab, (r.x + 32) as f32, baseline(text, r.y, r.h, px), px, FG, lw as f32);
            }
        }
    }

    // ----- Save: the name.
    if let Some(r) = l.name {
        label(text, canvas, tr("Name"), (d.x + 16) as f32, baseline(text, r.y, r.h, size::LABEL), size::LABEL, FG, (r.x - d.x - 24) as f32);
        field_text(canvas, text, r, &view.name, view.focus == Focus::Name && !modal, "");
        hits.push((r, hit(FilesHit::Name)));
    }

    // ----- Places.
    let sb = l.sidebar;
    let row_h = l.row_h;
    let clip = canvas.push_clip(sb.x, sb.y, sb.w, sb.h);
    let current_place = view.current_place();
    let mut y = sb.y - view.places_scroll.round() as i32;
    let mut group = None;
    for (i, place) in view.opts.places.iter().enumerate() {
        let gnow = place_group(place.kind);
        if group.is_some_and(|g| g != gnow) {
            theme::separator(canvas, sb.x + 8, y + 4, sb.w - 16);
            y += 9;
        }
        group = Some(gnow);
        let r = Rect { x: sb.x, y, w: sb.w, h: row_h };
        let on = current_place == Some(i);
        let lk = theme::Look { on, ..look(FilesHit::Place(i)) };
        theme::row(canvas, r.x, r.y + 1, r.w, r.h - 2, lk);
        let color = theme::control_text(lk);
        if let Some(img) = (p.icon)(place_icon(place, &view.home), color) {
            canvas.image(f64::from(r.x + 10), f64::from(r.y + (r.h - 16) / 2), 16.0, 16.0, &img);
        }
        // Removable drives and network places: an eject button at the
        // end.
        let eject = view.eject_root(i).map(|_| {
            let s = (row_h - 6).min(g(26));
            Rect { x: r.x + r.w - s - 3, y: r.y + (r.h - s) / 2, w: s, h: s }
        });
        let label_end = eject.map_or(r.x + r.w - 8, |e| e.x - 4);
        label(text, canvas, &place_label(place), (r.x + 34) as f32, baseline(text, r.y, r.h, size::LABEL), size::LABEL, color, (label_end - r.x - 34) as f32);
        let visible = |a: Rect| Rect { x: a.x, y: a.y.max(sb.y), w: a.w, h: (a.y + a.h).min(sb.y + sb.h) - a.y.max(sb.y) };
        if let Some(e) = eject {
            let elk = look(FilesHit::Eject(i));
            if elk.hovered || elk.pressed {
                theme::flat(canvas, (f64::from(e.x), f64::from(e.y), f64::from(e.w), f64::from(e.h)), theme::RADIUS_BUTTON, elk);
            }
            if let Some(img) = (p.icon)("media-eject-symbolic", color) {
                canvas.image(f64::from(e.x + (e.w - 16) / 2), f64::from(e.y + (e.h - 16) / 2), 16.0, 16.0, &img);
            }
            if visible(e).h > 0 {
                hits.push((visible(e), hit(FilesHit::Eject(i))));
            }
        }
        let shown = visible(r);
        if shown.h > 0 {
            hits.push((shown, hit(FilesHit::Place(i))));
        }
        y += row_h;
    }
    canvas.restore_clip(clip);
    hits.push((sb, hit(FilesHit::Sidebar)));
    // The edge between the places and the list: a line while it is
    // pointed at or dragged.
    if hover == Some(hit(FilesHit::SidebarEdge)) || matches!(view.resizing, Some(Resizing::Sidebar { .. })) {
        canvas.rect(sb.x + sb.w + 5, sb.y + 4, 2, sb.h - 8, accent());
    }

    // ----- The list.
    let body = l.body;
    let head = l.header;
    theme::well(canvas, head.x, head.y, head.w, head.h + body.h);
    let location = !matches!(view.shown, Shown::Folder(_));
    let second_w = if location { g(170) } else { g(84) };
    // Dates with the time are wider, and so are descriptions and MIME
    // types than categories (longer ones are cut).
    let modified_w = if view.settings.with_time { g(168) } else { g(118) };
    let type_w = view.settings.type_column.then(|| if view.settings.type_format == model::TypeFormat::Category { g(104) } else { g(170) });
    let cols = columns(body, k, second_w, location || view.settings.size_column, modified_w, type_w);
    let col_label = |c: SortColumn| match c {
        SortColumn::Name => tr("Name"),
        SortColumn::Size => tr("Size"),
        SortColumn::Modified => tr("Modified"),
        SortColumn::Location => tr("Location"),
        SortColumn::Type => tr("Type"),
    };
    let mut header_cols = vec![(SortColumn::Name, cols.name.0, cols.name.1, false)];
    if let Some((x, cw)) = cols.second {
        header_cols.push((if location { SortColumn::Location } else { SortColumn::Size }, x, cw, !location));
    }
    if let Some((x, cw)) = cols.kind {
        header_cols.push((SortColumn::Type, x, cw, false));
    }
    if let Some((x, cw)) = cols.modified {
        header_cols.push((SortColumn::Modified, x, cw, false));
    }
    let sort_on = matches!(view.shown, Shown::Folder(_) | Shown::Search { .. });
    for (c, x, cw, right) in &header_cols {
        let r = Rect { x: x - 4, y: head.y, w: cw + 8, h: head.h };
        let lk = look(FilesHit::Column(*c));
        if sort_on {
            theme::flat(canvas, (f64::from(r.x), f64::from(r.y + 2), f64::from(r.w), f64::from(r.h - 4)), theme::RADIUS_ROW, lk);
        }
        let s = col_label(*c);
        let lw = text.map_or(0.0, |t| t.width(s, size::CAPTION)).min(*cw as f32 - 14.0);
        let lx = if *right { (x + cw) as f32 - lw - 14.0 } else { *x as f32 };
        label(bold, canvas, s, lx, baseline(text, head.y, head.h, size::CAPTION), size::CAPTION, DIM_FG, *cw as f32 - 14.0);
        if sort_on && view.settings.sort.column == *c {
            let towards = if view.settings.sort.ascending { theme::Towards::Up } else { theme::Towards::Down };
            theme::chevron(canvas, f64::from(lx + lw + 8.0), f64::from(head.y + head.h / 2), 7.0, towards, DIM_FG);
        }
        if sort_on {
            hits.push((r, hit(FilesHit::Column(*c))));
        }
    }
    theme::separator(canvas, head.x + 6, head.y + head.h - 1, head.w - 12);
    let rh = l.row_h;
    let content = view.rows.len() as i32 * rh;
    let scroll = view.scroll.round() as i32;
    let first = usize::try_from(scroll / rh.max(1)).unwrap_or(0).min(view.rows.len());
    let last = (usize::try_from((scroll + body.h) / rh.max(1)).unwrap_or(0) + 1).min(view.rows.len());
    // The scrollbar (before the rows: it takes presses first).
    let bar_track = (content > body.h).then(|| {
        let track = Rect { x: body.x + body.w - 10, y: body.y + 4, w: 6, h: body.h - 8 };
        let thumb_h = ((i64::from(track.h) * i64::from(body.h) / i64::from(content.max(1))) as i32).max(24).min(track.h);
        let max = (content - body.h).max(1);
        let thumb_y = track.y + ((i64::from(track.h - thumb_h) * i64::from(scroll) / i64::from(max)) as i32);
        (track, Rect { x: track.x, y: thumb_y, w: track.w, h: thumb_h })
    });
    if let Some((track, _)) = bar_track {
        hits.push((Rect { x: track.x - 4, y: body.y, w: 14, h: body.h }, hit(FilesHit::Scrollbar)));
    }
    let clip = canvas.push_clip(body.x, body.y, body.w, body.h);
    let list_focused = view.focus == Focus::List && !modal;
    let selected = |r: usize| if view.opts.multiple { view.picked.contains(&view.rows[r]) } else { view.current == Some(r) };
    for r in first..last {
        let e = &view.entries[view.rows[r]];
        let ry = body.y + r as i32 * rh - scroll;
        let row = Rect { x: body.x + 4, y: ry, w: body.w - 8 - if bar_track.is_some() { 10 } else { 0 }, h: rh };
        let lk = theme::Look { on: selected(r), ..look(FilesHit::Row(r)) };
        theme::row(canvas, row.x, row.y + 1, row.w, row.h - 2, lk);
        if list_focused && view.current == Some(r) {
            canvas.round_outline(f64::from(row.x + 1), f64::from(row.y + 2), f64::from(row.w - 2), f64::from(row.h - 4), theme::RADIUS_ROW - 1.0, 1.5, if lk.on { theme::control_text(lk) } else { FG });
        }
        let color = theme::control_text(lk);
        let dim = if lk.on { color } else { DIM_FG };
        let icon = files::icon_name(e);
        if let Some(img) = (p.icon)(&icon, color) {
            canvas.image(f64::from(cols.name.0 + 2), f64::from(row.y + (rh - 16) / 2), 16.0, 16.0, &img);
        }
        let base = baseline(text, row.y, rh, size::LABEL);
        let name_x = cols.name.0 + 26;
        let room = (cols.name.0 + cols.name.1 - name_x) as f32;
        if let Some(t) = text {
            let shown = middle_ellipsis(t, &e.name, size::LABEL, room);
            label(text, canvas, &shown, name_x as f32, base, size::LABEL, color, room + 4.0);
        }
        if let Some((x, cw)) = cols.second {
            if location {
                let parent = e.path.parent().map(|p| files::shown_path(p, &view.home)).unwrap_or_default();
                if let Some(t) = text {
                    let shown = middle_ellipsis(t, &parent, size::LABEL, cw as f32);
                    label(text, canvas, &shown, x as f32, base, size::LABEL, dim, cw as f32 + 4.0);
                }
            } else if !e.dir {
                let s = model::size_text(e.size);
                let sw = text.map_or(0.0, |t| t.width(&s, size::LABEL)).min(cw as f32);
                label(text, canvas, &s, (x + cw) as f32 - sw - 14.0, base, size::LABEL, dim, cw as f32);
            }
        }
        if let Some((x, cw)) = cols.kind {
            let s = model::type_text(e, view.settings.type_format);
            label(text, canvas, &s, x as f32, base, size::LABEL, dim, cw as f32);
        }
        if let Some((x, cw)) = cols.modified {
            let s = model::modified_text(e.modified, p.now_unix, view.settings.twelve_hour, view.settings.with_time);
            label(text, canvas, &s, x as f32, base, size::LABEL, dim, cw as f32);
        }
        let shown = Rect { x: row.x, y: row.y.max(body.y), w: row.w, h: (row.y + rh).min(body.y + body.h) - row.y.max(body.y) };
        if shown.h > 0 {
            hits.push((shown, hit(FilesHit::Row(r))));
        }
    }
    // Nothing to show: say why.
    let loading_shown = view.loading.is_some_and(|(_, since)| p.now.duration_since(since) >= model::LOADING_AFTER);
    if view.rows.is_empty() {
        let s = if loading_shown {
            tr("Loading…").to_owned()
        } else if view.loading.is_some() {
            String::new()
        } else if let Some(err) = &view.error {
            trf!("This folder cannot be shown: {error}", error = err)
        } else {
            match view.shown {
                Shown::Folder(_) => tr("Folder is empty").to_owned(),
                Shown::Recent => tr("No recent files").to_owned(),
                Shown::Search { .. } => tr("No results found").to_owned(),
            }
        };
        if !s.is_empty() {
            let lines = text.map_or_else(|| vec![s.clone()], |t| t.wrap(&s, size::BODY, (body.w - 48) as f32, 3));
            let mut ly = body.y + body.h / 3;
            if loading_shown {
                // A still spinner: a ring with a brighter arc.
                let (cx, cy) = (f64::from(body.x + body.w / 2), f64::from(ly - 28));
                canvas.ring(cx, cy, 11.0, 3.0, theme::TRACK);
                canvas.arc(cx, cy, 11.0, 3.0, -std::f64::consts::FRAC_PI_2, std::f64::consts::FRAC_PI_2, accent());
            }
            for line in &lines {
                let lw = text.map_or(0.0, |t| t.width(line, size::BODY));
                label(text, canvas, line, body.x as f32 + (body.w as f32 - lw) / 2.0, ly as f32, size::BODY, DIM_FG, (body.w - 48) as f32);
                ly += g(22);
            }
        }
    }
    if let Some((track, thumb)) = bar_track {
        let grabbed = view.dragging.is_some() || hover == Some(hit(FilesHit::Scrollbar));
        canvas.round_rect(f64::from(track.x), f64::from(track.y), f64::from(track.w), f64::from(track.h), 3.0, 0x30FF_FFFF);
        canvas.round_rect(f64::from(thumb.x), f64::from(thumb.y), f64::from(thumb.w), f64::from(thumb.h), 3.0, if grabbed { FG } else { DIM_FG });
    }
    // Type-ahead: what was typed, in a bubble.
    if let (Some(typed), Some(t)) = (view.typeahead_shown(p.now), text) {
        let tw = t.width(typed, size::LABEL).ceil() as i32;
        let bh = g(30);
        let r = Rect { x: body.x + body.w - tw - 40, y: body.y + body.h - bh - 10, w: tw + 24, h: bh };
        theme::popover(canvas, r.x, r.y, r.w, r.h);
        label(text, canvas, typed, (r.x + 12) as f32, baseline(text, r.y, r.h, size::LABEL), size::LABEL, FG, tw as f32 + 4.0);
    }
    canvas.restore_clip(clip);
    hits.push((body, hit(FilesHit::List)));

    // ----- The preview.
    if let Some(r) = l.preview {
        theme::well(canvas, r.x, r.y, r.w, r.h);
        let entry = view.current_entry().filter(|e| files::previewable(e));
        let preview = entry.and_then(|e| view.previews.get(&*e.path.to_string_lossy())).and_then(|i| i.as_ref());
        match preview {
            Some(crate::Preview::Image(image)) if image.width > 0 && image.height > 0 => {
                let (aw, ah) = (f64::from(r.w - 24), f64::from(r.h - 24));
                let s = (aw / image.width as f64).min(ah / image.height as f64).min(1.0);
                let (iw, ih) = (image.width as f64 * s, image.height as f64 * s);
                canvas.image_rounded(f64::from(r.x) + (f64::from(r.w) - iw) / 2.0, f64::from(r.y) + 12.0 + (ah - ih) / 2.0, iw, ih, theme::RADIUS_ROW, image, 1.0);
            }
            Some(crate::Preview::Text(lines)) => {
                let line_h = g(16);
                let fit = usize::try_from((r.h - 24) / line_h.max(1)).unwrap_or(0);
                for (i, line) in lines.iter().take(fit).enumerate() {
                    label(text, canvas, line, (r.x + 12) as f32, (r.y + 22 + i as i32 * line_h) as f32, size::CAPTION, FG, (r.w - 24) as f32);
                }
            }
            _ => {
                // While a preview is being made, nothing; else say there
                // is none.
                let pending = entry.is_some_and(|e| view.previews.get(&*e.path.to_string_lossy()).is_some_and(Option::is_none) && !view.previews_done.contains(&*e.path.to_string_lossy()));
                let s = if pending { "" } else { tr("No preview") };
                let tw = text.map_or(0.0, |t| t.width(s, size::CAPTION));
                label(text, canvas, s, r.x as f32 + (r.w as f32 - tw) / 2.0, baseline(text, r.y, r.h, size::CAPTION), size::CAPTION, DIM_FG, (r.w - 16) as f32);
            }
        }
    }

    // ----- The footer: check box, warning, file type, buttons.
    let f = l.footer;
    let mut fx = f.x;
    if let Some((name, _)) = &view.opts.check {
        let r = Rect { x: fx, y: f.y + (f.h - g(26)) / 2, w: g(26) + 8 + text.map_or(100, |t| t.width(name, size::LABEL).ceil() as i32).min(g(260)), h: g(26) };
        let (bx, by) = (f64::from(r.x), f64::from(r.y + (r.h - 18) / 2));
        theme::check_box(canvas, bx, by, 18.0, view.check, look(FilesHit::Check));
        if view.focus == Focus::Check && !modal {
            theme::focus_ring_round(canvas, bx, by, 18.0, 18.0, theme::RADIUS_CHECK);
        }
        label(text, canvas, name, (r.x + 28) as f32, baseline(text, r.y, r.h, size::LABEL), size::LABEL, FG, (r.w - 28) as f32);
        hits.push((r, hit(FilesHit::Check)));
        fx = r.x + r.w + 16;
    }
    let filters = view.opts.filters.len();
    let filter_r = (filters >= 1 && view.opts.mode.lists_files()).then(|| {
        let name = view.filter_now().map_or(tr("All Files"), |f| f.0.as_str());
        let fw = (text.map_or(120, |t| t.width(name, size::BODY).ceil() as i32) + 48).clamp(g(120), g(240));
        Rect { x: l.cancel.x - 16 - fw, y: f.y, w: fw, h: f.h }
    });
    if !view.warning.is_empty() {
        let room = (filter_r.map_or(l.cancel.x, |r| r.x) - 16 - fx) as f32;
        label(text, canvas, &view.warning, fx as f32, baseline(text, f.y, f.h, size::LABEL), size::LABEL, ERROR_TEXT, room);
    }
    if let Some(r) = filter_r {
        let name = view.filter_now().map_or(tr("All Files"), |f| f.0.as_str());
        let lk = theme::Look { on: view.dropdown, ..look(FilesHit::FilterButton) };
        let rect = (f64::from(r.x), f64::from(r.y), f64::from(r.w), f64::from(r.h));
        theme::control_round(canvas, rect, theme::RADIUS_BUTTON, theme::Look { on: false, ..lk });
        label(text, canvas, name, (r.x + 12) as f32, baseline(text, r.y, r.h, size::BODY), size::BODY, FG, (r.w - 40) as f32);
        theme::chevron(canvas, f64::from(r.x + r.w - 18), f64::from(r.y + r.h / 2), 8.0, theme::Towards::Down, FG);
        if view.focus == Focus::Filter && !modal {
            theme::focus_ring_round(canvas, rect.0, rect.1, rect.2, rect.3, theme::RADIUS_BUTTON);
        }
        hits.push((r, hit(FilesHit::FilterButton)));
    }
    let blook = |x: OverlayHit| theme::Look { hovered: hover == Some(x), pressed: pressed == Some(x), on: false };
    let at = |r: Rect| (r.x, r.y, r.w, r.h);
    theme::button(canvas, text, at(l.cancel), tr("Cancel"), theme::Kind::Normal, blook(OverlayHit::Cancel), view.focus == Focus::Cancel && !modal);
    theme::button(canvas, text, at(l.accept), &view.opts.accept, theme::Kind::Suggested, blook(OverlayHit::Confirm), view.focus == Focus::Accept && !modal);
    hits.push((l.cancel, OverlayHit::Cancel));
    hits.push((l.accept, OverlayHit::Confirm));

    // ----- The file type menu, above its button.
    if let (true, Some(b)) = (view.dropdown, filter_r) {
        let ih = g(32);
        let mw = view.opts.filters.iter().map(|f| text.map_or(120, |t| t.width(&f.0, size::LABEL).ceil() as i32)).max().unwrap_or(120) + 56;
        let mw = mw.max(b.w).min(d.w - 32);
        let mh = view.opts.filters.len() as i32 * ih + 12;
        let m = Rect { x: (b.x + b.w - mw).max(d.x + 16), y: (b.y - 6 - mh).max(0), w: mw, h: mh };
        theme::popover(canvas, m.x, m.y, m.w, m.h);
        let mut items = Vec::new();
        for (i, flt) in view.opts.filters.iter().enumerate() {
            let r = Rect { x: m.x + 6, y: m.y + 6 + i as i32 * ih, w: m.w - 12, h: ih };
            let lk = theme::Look { hovered: hover == Some(hit(FilesHit::FilterItem(i))) || view.dropdown_at == i, pressed: pressed == Some(hit(FilesHit::FilterItem(i))), on: false };
            theme::row(canvas, r.x, r.y, r.w, r.h, lk);
            if view.filter == Some(i) {
                let (cx, cy) = (f64::from(r.x + 14), f64::from(r.y + r.h / 2));
                canvas.line((cx - 5.0, cy), (cx - 1.5, cy + 4.0), 2.0, FG);
                canvas.line((cx - 1.5, cy + 4.0), (cx + 5.0, cy - 4.0), 2.0, FG);
            }
            label(text, canvas, &flt.0, (r.x + 30) as f32, baseline(text, r.y, r.h, size::LABEL), size::LABEL, FG, (r.w - 38) as f32);
            items.push((r, hit(FilesHit::FilterItem(i))));
        }
        items.append(&mut hits);
        hits = items;
    }

    // ----- Asking before replacing a file: over everything else.
    if let Some(path) = &view.replace {
        canvas.round_rect(f64::from(d.x), f64::from(d.y), f64::from(d.w), f64::from(d.h), theme::RADIUS_POPOVER, 0x9000_0000);
        let cw = g(420).min(d.w - 40);
        let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let folder = path.parent().and_then(Path::file_name).map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let message = trf!("A file named “{name}” already exists in “{folder}”. Replacing it overwrites its contents.", name = name, folder = folder);
        let lines = text.map_or_else(|| vec![message.clone()], |t| t.wrap(&message, size::LABEL, (cw - 48) as f32, 5));
        let ch = g(30) + 16 + lines.len() as i32 * g(19) + 20 + g(36) + 24;
        let c = Rect { x: d.x + (d.w - cw) / 2, y: d.y + (d.h - ch) / 2, w: cw, h: ch };
        theme::popover(canvas, c.x, c.y, c.w, c.h);
        let title = tr("Replace File?");
        let tw = bold.map_or(0.0, |t| t.width(title, size::SUBTITLE));
        label(bold, canvas, title, c.x as f32 + (c.w as f32 - tw) / 2.0, (c.y + 16 + g(22)) as f32, size::SUBTITLE, FG, (c.w - 32) as f32);
        let mut ly = c.y + 16 + g(30) + 16 + g(14);
        for line in &lines {
            let lw = text.map_or(0.0, |t| t.width(line, size::LABEL));
            label(text, canvas, line, c.x as f32 + (c.w as f32 - lw) / 2.0, ly as f32, size::LABEL, FG, (c.w - 48) as f32);
            ly += g(19);
        }
        let half = (c.w - 48 - 12) / 2;
        let cancel = Rect { x: c.x + 24, y: c.y + c.h - 24 - g(36), w: half, h: g(36) };
        let replace = Rect { x: cancel.x + half + 12, y: cancel.y, w: half, h: g(36) };
        theme::button(canvas, text, at(cancel), tr("Cancel"), theme::Kind::Normal, look(FilesHit::ReplaceCancel), false);
        theme::button(canvas, text, at(replace), tr("Replace"), theme::Kind::Destructive, look(FilesHit::ReplaceConfirm), true);
        // Only the question takes clicks now.
        hits = vec![(cancel, hit(FilesHit::ReplaceCancel)), (replace, hit(FilesHit::ReplaceConfirm))];
    }
    Painted { hits, drawn: first..last, loading_shown }
}

/// The chooser on the accessibility bus (`ui/atspi.rs`): one dialog node
/// (the same id for its whole life) with the path bar, the places, the
/// file list (named after the folder; each row's place in the whole
/// list, also for rows not on screen), the fields and buttons.
pub(super) fn files_node(view: &ChooserView, painted: &Painted, now: std::time::Instant) -> crate::a11y::Node {
    use crate::a11y::{Node, Role};
    let at = |h: OverlayHit| painted.hits.iter().find(|(_, x)| *x == h).map(|(r, _)| crate::a11y::Rect::from(*r));
    let place = |n: Node, h: OverlayHit| match at(h) {
        Some(r) => n.at(r).click(),
        None => n,
    };
    let modal = view.replace.is_some();
    let mut out = Node::new(Role::Dialog, &view.opts.title).id("file-chooser").modal();
    if let Some(path) = &view.replace {
        let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let alert = Node::new(Role::AlertDialog, tr("Replace File?"))
            .id("replace")
            .child(Node::label(trf!("A file named “{name}” already exists. Replacing it overwrites its contents.", name = name)))
            .child(place(Node::new(Role::Button, tr("Cancel")).id("replace-cancel"), hit(FilesHit::ReplaceCancel)))
            .child(place(Node::new(Role::Button, tr("Replace")).id("replace-confirm"), hit(FilesHit::ReplaceConfirm)).focused(true));
        return out.child(alert);
    }
    // The path bar.
    let mut bar = Node::new(Role::Toolbar, tr("Location")).id("path-bar");
    bar = bar.child(place(Node::new(Role::Button, tr("Back")).id("back").disabled(view.back.is_empty()), hit(FilesHit::Back)));
    bar = bar.child(place(Node::new(Role::Button, tr("Forward")).id("forward").disabled(view.forward.is_empty()), hit(FilesHit::Forward)));
    if let Some((what, edit)) = &view.field {
        let name = match what {
            FieldUse::Location => tr("Location"),
            FieldUse::Search => tr("Search"),
            FieldUse::NewFolder => tr("New folder name"),
        };
        let role = if *what == FieldUse::Search { Role::SearchInput } else { Role::TextInput };
        let r = at(hit(FilesHit::Field)).unwrap_or_default();
        bar = bar.child(Node::text_field(role, name, &edit.text, r).id("field").focused(view.focus == Focus::Field));
    } else {
        for (i, (lab, _, here)) in view.crumbs().iter().enumerate() {
            if let Some(r) = at(hit(FilesHit::Crumb(i))) {
                bar = bar.child(Node::button(lab, r).id(format!("crumb:{i}")).toggled(*here));
            }
        }
    }
    if at(hit(FilesHit::Search)).is_some() {
        bar = bar.child(place(Node::new(Role::Button, tr("Search")).id("search"), hit(FilesHit::Search)));
    }
    if at(hit(FilesHit::NewFolder)).is_some() {
        bar = bar.child(place(Node::new(Role::Button, tr("New Folder")).id("new-folder"), hit(FilesHit::NewFolder)));
    }
    out = out.child(bar);
    if let Some((r, _)) = painted.hits.iter().find(|(_, x)| *x == hit(FilesHit::Name)) {
        out = out.child(Node::text_field(Role::TextInput, tr("Name"), &view.name.text, (*r).into()).id("name").focused(view.focus == Focus::Name && !modal));
    }
    let current_place = view.current_place();
    let places = view.opts.places.iter().enumerate().map(|(i, p)| place(Node::new(Role::ListBoxOption, place_label(p)).id(format!("place:{i}")).selected(current_place == Some(i)), hit(FilesHit::Place(i))));
    out = out.child(Node::new(Role::ListBox, tr("Places")).id("places").children(places));
    for (i, p) in view.opts.places.iter().enumerate().filter(|(i, _)| view.eject_root(*i).is_some()) {
        out = out.child(place(Node::new(Role::Button, trf!("Eject {drive}", drive = place_label(p))).id(format!("eject:{i}")), hit(FilesHit::Eject(i))));
    }
    // The list: every row while there are not too many, else the ones
    // near the screen; each knows its place among all rows.
    let n = view.rows.len();
    let window = if n <= A11Y_ALL_ROWS { 0..n } else { painted.drawn.start.saturating_sub(50)..(painted.drawn.end + 50).min(n) };
    let list_focused = view.focus == Focus::List && !modal;
    let rows = window.map(|r| {
        let e = &view.entries[view.rows[r]];
        let on = if view.opts.multiple { view.picked.contains(&view.rows[r]) } else { view.current == Some(r) };
        let mut desc = Vec::new();
        if e.dir {
            desc.push(tr("Folder").to_owned());
        } else {
            desc.push(model::size_text(e.size));
            if view.settings.type_column {
                desc.push(model::type_text(e, view.settings.type_format));
            }
        }
        let modified = model::modified_text(e.modified, now_unix(), view.settings.twelve_hour, view.settings.with_time);
        if !modified.is_empty() {
            desc.push(trf!("Modified {when}", when = modified));
        }
        if !matches!(view.shown, Shown::Folder(_)) {
            if let Some(parent) = e.path.parent() {
                desc.push(files::shown_path(parent, &view.home));
            }
        }
        place(Node::new(Role::ListBoxOption, &e.name).id(format!("file:{}", e.name)).describe(desc.join(", ")).selected(on).focused(list_focused && view.current == Some(r)).set_size(n).in_set(r + 1), hit(FilesHit::Row(r)))
    });
    out = out.child(Node::new(Role::ListBox, view.list_name()).id("files").set_size(n).focused(list_focused && view.current.is_none()).children(rows));
    let status = view.status(now);
    if !status.is_empty() {
        out = out.child(Node::new(Role::Status, status).id("status").live());
    }
    if let Some((name, _)) = &view.opts.check {
        out = out.child(place(Node::new(Role::CheckBox, name).id("check").toggled(view.check).focused(view.focus == Focus::Check), hit(FilesHit::Check)));
    }
    if at(hit(FilesHit::FilterButton)).is_some() {
        let value = view.filter_now().map_or(tr("All Files"), |f| f.0.as_str());
        let mut combo = place(Node::new(Role::ComboBox, tr("File Type")).id("filter").describe(value).expanded(view.dropdown).focused(view.focus == Focus::Filter), hit(FilesHit::FilterButton));
        if view.dropdown {
            let items = view.opts.filters.iter().enumerate().map(|(i, f)| place(Node::new(Role::ListBoxOption, &f.0).id(format!("filter:{i}")).selected(view.filter == Some(i)).focused(view.dropdown_at == i), hit(FilesHit::FilterItem(i))));
            combo = combo.child(Node::new(Role::ListBox, tr("File Type")).children(items));
        }
        out = out.child(combo);
    }
    if !view.warning.is_empty() {
        out = out.child(Node::label(&view.warning).id("warning").live());
    }
    out.child(place(Node::new(Role::Button, tr("Cancel")).id("cancel").focusable().focused(view.focus == Focus::Cancel), OverlayHit::Cancel))
        .child(place(Node::new(Role::Button, &view.opts.accept).id("confirm").focusable().focused(view.focus == Focus::Accept), OverlayHit::Confirm))
}

fn now_unix() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// A view for the gallery and tests.
#[cfg(test)]
pub(super) fn sample_view(options: Options, shown: Shown, entries: Vec<Entry>, home: &Path) -> ChooserView {
    let (reply, _) = oneshot::channel();
    let mut v = ChooserView::new(Request { options, reply }, ChooserSettings::default(), home.to_path_buf());
    if let Shown::Folder(f) = &shown {
        v.crumb_tail = f.clone();
    }
    v.shown = shown;
    v.entries = entries;
    v.refresh_rows();
    v
}

impl ChooserView {
    /// Keep the scroll in range for this paint's sizes.
    fn note_sizes(&mut self, row_h: i32, body_h: i32) {
        self.row_h = row_h.max(1);
        self.body_h = body_h.max(1);
        self.scroll = self.scroll.clamp(0.0, self.max_scroll());
    }
}

// ----- The shell's side ---------------------------------------------------

impl Ui {
    fn files_view(&mut self) -> Option<&mut ChooserView> {
        match &mut self.dialog.as_mut()?.state.kind {
            DialogKind::Files(view) => Some(view),
            _ => None,
        }
    }

    /// `ChooseFiles` asked: show the chooser (in place of any other
    /// dialog, which is answered "no").
    pub(super) fn show_files(&mut self, request: Request) {
        if self.lock.session.is_some() {
            let _ = request.reply.send(None);
            return;
        }
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/".into());
        let view = ChooserView::new(request, ChooserSettings::read(), home);
        let start = view.shown.clone();
        let mut view = view;
        view.mounts = mount_paths(&self.dock.all_mounts);
        tracing::info!(mode = ?view.opts.mode, title = %view.opts.title, "file chooser opened");
        self.show_dialog(DialogState::new(DialogKind::Files(Box::new(view)), 0));
        self.files_go(start, false);
    }

    /// Drives came or went: the eject buttons follow, and the places are
    /// read again now.
    pub(super) fn files_mounts_changed(&mut self) {
        let mounts = mount_paths(&self.dock.all_mounts);
        let Some(view) = self.files_view() else { return };
        view.mounts = mounts;
        view.places_asked = None;
        self.files_tick();
        self.draw_dialog();
    }

    /// While the chooser is open its places are read again now and then:
    /// drives, network places, bookmarks and folders come and go.
    pub(super) fn files_tick(&mut self) {
        let Some(view) = self.files_view() else { return };
        if view.places_asked.is_some_and(|t| t.elapsed() < PLACES_EVERY) {
            return;
        }
        view.places_asked = Some(std::time::Instant::now());
        let recent = view.opts.places.iter().any(|p| p.kind == PlaceKind::Recent);
        let _ = self.requests.send(crate::UiRequest::FilesPlaces { recent });
    }

    /// The places read again: shown when they changed. A drive that went
    /// while the list showed it leaves for the home folder.
    pub(super) fn files_places(&mut self, places: Vec<Place>) {
        let Some(view) = self.files_view() else { return };
        if view.opts.places == places {
            return;
        }
        let gone: Vec<PathBuf> = view
            .opts
            .places
            .iter()
            .filter(|p| matches!(p.kind, PlaceKind::Drive | PlaceKind::Network))
            .filter_map(|p| p.path.clone())
            .filter(|p| !places.iter().any(|q| q.path.as_ref() == Some(p)))
            .collect();
        tracing::info!(places = places.len(), names = ?places.iter().map(place_label).collect::<Vec<_>>(), "file chooser places");
        view.opts.places = places;
        if view.folder().is_some_and(|f| gone.iter().any(|g| f.starts_with(g))) {
            let home = view.home.clone();
            self.files_go(Shown::Folder(home), true);
        } else {
            self.draw_dialog();
        }
    }

    /// Show `to` (in history unless going back or forward): the list
    /// empties at once and fills when the worker has read the folder.
    fn files_go(&mut self, to: Shown, history: bool) {
        let Some(view) = self.files_view() else { return };
        if history && view.shown != to {
            let from = view.shown.clone();
            view.back.push(from);
            view.forward.clear();
        }
        if let Shown::Folder(f) = &to {
            if !view.crumb_tail.starts_with(f) {
                view.crumb_tail = f.clone();
            }
        }
        view.shown = to.clone();
        view.entries.clear();
        view.rows.clear();
        view.current = None;
        view.picked.clear();
        view.scroll = 0.0;
        view.error = None;
        view.warning.clear();
        view.typeahead.clear();
        view.last_press = None;
        view.dropdown = false;
        if view.field.as_ref().is_some_and(|(u, _)| *u != FieldUse::Search) || !matches!(to, Shown::Search { .. }) {
            if view.focus == Focus::Field {
                view.focus = if view.opts.mode == Mode::Save { Focus::Name } else { Focus::List };
            }
            view.field = None;
        }
        view.serial += 1;
        let serial = view.serial;
        view.loading = Some((serial, std::time::Instant::now()));
        view.loading_said = false;
        let listing = match &to {
            Shown::Folder(f) => Listing::Folder(f.clone()),
            Shown::Recent => Listing::Recent,
            Shown::Search { dir, query } => Listing::Search { dir: dir.clone(), query: query.clone(), filter: view.filter_now().cloned() },
        };
        match &to {
            Shown::Folder(f) => tracing::info!(folder = %f.display(), "file chooser folder"),
            other => tracing::info!(shown = ?other, "file chooser folder"),
        }
        let _ = self.requests.send(crate::UiRequest::ListFiles { serial, listing });
        // Say it is loading if the folder takes a while.
        if let Some(handle) = self.loop_handle.clone() {
            let timer = smithay_client_toolkit::reexports::calloop::timer::Timer::from_duration(model::LOADING_AFTER + std::time::Duration::from_millis(20));
            let _ = handle.insert_source(timer, move |_, _, ui| {
                if ui.dialog.as_ref().is_some_and(|d| matches!(&d.state.kind, DialogKind::Files(v) if v.loading.is_some_and(|(s, _)| s == serial))) {
                    ui.draw_dialog();
                }
                smithay_client_toolkit::reexports::calloop::timer::TimeoutAction::Drop
            });
        }
        self.draw_dialog();
    }

    /// A listing came back.
    pub(super) fn files_listed(&mut self, serial: u64, entries: Result<Vec<Entry>, String>) {
        let Some(view) = self.files_view() else { return };
        if view.loading.is_none_or(|(s, _)| s != serial) {
            return;
        }
        view.loading = None;
        match entries {
            Ok(entries) => view.entries = entries,
            Err(err) => {
                tracing::info!(%err, "file chooser cannot list");
                view.error = Some(err);
            }
        }
        view.refresh_rows();
        tracing::info!(entries = view.entries.len(), rows = view.rows.len(), current = ?view.current_entry().map(|e| &e.name), "file chooser listed");
        self.draw_dialog();
    }

    /// A preview was decoded (or cannot be shown).
    pub(super) fn files_preview(&mut self, path: String, image: Option<crate::Preview>) {
        let Some(view) = self.files_view() else { return };
        if !view.previews.contains_key(&path) {
            return;
        }
        view.previews_done.insert(path.clone());
        view.previews.insert(path, image);
        self.draw_dialog();
    }

    /// A new folder was made (or not).
    pub(super) fn files_folder_made(&mut self, serial: u64, result: Result<PathBuf, String>) {
        let Some(view) = self.files_view() else { return };
        if view.making != Some(serial) {
            return;
        }
        view.making = None;
        match result {
            Ok(path) => {
                tracing::info!(folder = %path.display(), "file chooser made a folder");
                view.field = None;
                view.focus = if view.opts.mode == Mode::Save { Focus::Name } else { Focus::List };
                self.files_go(Shown::Folder(path), true);
            }
            Err(err) => {
                view.warning = trf!("The folder could not be made: {error}", error = err);
                self.draw_dialog();
            }
        }
    }

    fn files_parent(&mut self) {
        let Some(view) = self.files_view() else { return };
        let Some(folder) = view.folder().map(Path::to_path_buf) else { return };
        let Some(parent) = folder.parent() else { return };
        view.reselect = folder.file_name().map(|n| n.to_string_lossy().into_owned());
        let to = Shown::Folder(parent.to_path_buf());
        self.files_go_keeping(to);
    }

    /// Go to `to` keeping the entry to reselect.
    fn files_go_keeping(&mut self, to: Shown) {
        let keep = self.files_view().and_then(|v| v.reselect.take());
        self.files_go(to, true);
        if let Some(view) = self.files_view() {
            view.reselect = keep;
        }
    }

    fn files_history(&mut self, back: bool) {
        let Some(view) = self.files_view() else { return };
        let to = if back { view.back.pop() } else { view.forward.pop() };
        let Some(to) = to else { return };
        let from = view.shown.clone();
        if back {
            view.forward.push(from.clone());
        } else {
            view.back.push(from.clone());
        }
        // Coming back up: the folder left is selected.
        if let (Shown::Folder(f), Shown::Folder(t)) = (&from, &to) {
            if f.parent() == Some(t.as_path()) {
                view.reselect = f.file_name().map(|n| n.to_string_lossy().into_owned());
            }
        }
        let keep = view.reselect.take();
        self.files_go(to, false);
        if let Some(view) = self.files_view() {
            view.reselect = keep;
        }
    }

    /// Activate row `r` (double-click, Enter in the list): a folder
    /// opens; a file is chosen (Save: saved over, after asking).
    fn files_activate(&mut self, r: usize) {
        let Some(view) = self.files_view() else { return };
        let Some(&e) = view.rows.get(r) else { return };
        let entry = view.entries[e].clone();
        if entry.dir {
            self.files_go(Shown::Folder(entry.path), true);
            return;
        }
        match view.opts.mode {
            Mode::Open => {
                let mut paths = view.chosen_files();
                if !paths.contains(&entry.path) {
                    paths = vec![entry.path.clone()];
                }
                self.files_done(paths);
            }
            Mode::Save => {
                view.name = TextEdit::new(&entry.name);
                if !matches!(view.shown, Shown::Folder(_)) {
                    if let Some(parent) = entry.path.parent() {
                        let parent = parent.to_path_buf();
                        view.name = TextEdit::selecting(&entry.name, 0, model::base_name_end(&entry.name));
                        view.focus = Focus::Name;
                        self.files_go(Shown::Folder(parent), true);
                        return;
                    }
                }
                self.files_save();
            }
            Mode::Folder | Mode::SaveFiles => {}
        }
    }

    /// Answer with `paths`.
    fn files_done(&mut self, paths: Vec<PathBuf>) {
        let Some(view) = self.files_view() else { return };
        if paths.is_empty() {
            return;
        }
        let chosen = view.outcome(paths);
        tracing::info!(paths = ?chosen.paths, "file chooser chose");
        view.answer(Some(chosen));
        self.dialog = None;
    }

    /// Save under the name typed, in the folder shown (asking before
    /// replacing a file).
    fn files_save(&mut self) {
        let Some(view) = self.files_view() else { return };
        let typed = view.name.text.trim().to_owned();
        if typed.is_empty() {
            view.warning = tr("Type a name for the file.").into();
            self.draw_dialog();
            return;
        }
        let folder = view.save_folder();
        let target = files::resolve(&folder, &typed, &view.home);
        // The folder's listing knows whether the name is taken; a path
        // typed elsewhere is looked at.
        let listed = (target.parent() == Some(folder.as_path())).then(|| view.entries.iter().find(|e| e.path == target).map(|e| e.dir));
        let (exists, is_dir) = match listed {
            Some(found) => (found.is_some(), found == Some(true)),
            None => (target.exists(), target.is_dir()),
        };
        if is_dir {
            view.name = TextEdit::default();
            self.files_go(Shown::Folder(target), true);
            return;
        }
        if !target.parent().is_some_and(Path::is_dir) {
            view.warning = tr("That folder does not exist.").into();
            self.draw_dialog();
            return;
        }
        if exists {
            tracing::info!(file = %target.display(), "file chooser asks before replacing");
            view.replace = Some(target);
            view.dropdown = false;
            self.draw_dialog();
            return;
        }
        self.files_done(vec![target]);
    }

    /// The accept button (or Enter where it means accept).
    fn files_accept(&mut self) {
        let Some(view) = self.files_view() else { return };
        view.dropdown = false;
        match view.opts.mode {
            Mode::Save => self.files_save(),
            Mode::SaveFiles => {
                let folder = view.save_folder();
                self.files_done(vec![folder]);
            }
            Mode::Folder => {
                let chosen = view.current_entry().filter(|e| e.dir).map(|e| e.path.clone()).or_else(|| view.folder().map(Path::to_path_buf));
                if let Some(folder) = chosen {
                    self.files_done(vec![folder]);
                }
            }
            Mode::Open => {
                let files = view.chosen_files();
                if !files.is_empty() {
                    self.files_done(files);
                } else if let Some(folder) = view.current_entry().filter(|e| e.dir).map(|e| e.path.clone()) {
                    // A folder selected: Open goes into it (as GTK does).
                    self.files_go(Shown::Folder(folder), true);
                }
            }
        }
    }

    /// Enter in the path bar's field.
    fn files_field_enter(&mut self) {
        let Some(view) = self.files_view() else { return };
        let Some((what, edit)) = view.field.clone() else { return };
        let typed = edit.text.trim().to_owned();
        match what {
            FieldUse::Search => {
                if typed.is_empty() {
                    return;
                }
                let dir = match &view.shown {
                    Shown::Folder(f) => f.clone(),
                    Shown::Search { dir, .. } => dir.clone(),
                    Shown::Recent => view.home.clone(),
                };
                view.focus = Focus::List;
                self.files_go(Shown::Search { dir, query: typed }, true);
            }
            FieldUse::NewFolder => {
                let Some(folder) = view.folder().map(Path::to_path_buf) else { return };
                if typed.is_empty() {
                    view.warning = tr("Type a name for the folder.").into();
                } else if typed.contains('/') {
                    view.warning = tr("Folder names cannot contain “/”.").into();
                } else if typed == "." || typed == ".." {
                    view.warning = tr("That name cannot be used.").into();
                } else if view.entries.iter().any(|e| e.name == typed) {
                    view.warning = tr("A file or folder with that name already exists.").into();
                } else {
                    view.serial += 1;
                    view.making = Some(view.serial);
                    let (serial, path) = (view.serial, folder.join(&typed));
                    let _ = self.requests.send(crate::UiRequest::MakeFolder { serial, path });
                    return;
                }
                self.draw_dialog();
            }
            FieldUse::Location => {
                let base = view.save_folder();
                let path = files::resolve(&base, &typed, &view.home);
                if typed.is_empty() {
                    return;
                }
                if path.is_dir() {
                    self.files_go(Shown::Folder(path), true);
                } else if path.is_file() && view.opts.mode == Mode::Open {
                    self.files_done(vec![path]);
                } else if view.opts.mode == Mode::Save && path.parent().is_some_and(Path::is_dir) {
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    view.name = TextEdit::new(&name);
                    view.field = None;
                    view.focus = Focus::Name;
                    if let Some(parent) = path.parent().map(Path::to_path_buf).filter(|p| Some(p.as_path()) != view.folder()) {
                        self.files_go(Shown::Folder(parent), true);
                    } else {
                        self.draw_dialog();
                    }
                } else {
                    view.warning = trf!("“{name}” was not found.", name = typed);
                    self.draw_dialog();
                }
            }
        }
    }

    /// Open the path bar's field for `what` (`text` in it).
    fn files_open_field(&mut self, what: FieldUse, text: &str) {
        let Some(view) = self.files_view() else { return };
        view.field = Some((what, TextEdit::new(text)));
        view.focus = Focus::Field;
        view.dropdown = false;
        view.warning.clear();
        self.draw_dialog();
    }

    /// Close the field (Escape): a search ends and its folder shows again.
    fn files_close_field(&mut self) {
        let Some(view) = self.files_view() else { return };
        view.field = None;
        view.focus = if view.opts.mode == Mode::Save { Focus::Name } else { Focus::List };
        if let Shown::Search { dir, .. } = view.shown.clone() {
            self.files_go(Shown::Folder(dir), true);
            return;
        }
        self.draw_dialog();
    }

    fn files_set_filter(&mut self, i: usize) {
        let Some(view) = self.files_view() else { return };
        if i >= view.opts.filters.len() {
            return;
        }
        view.filter = Some(i);
        view.dropdown = false;
        tracing::info!(filter = %view.opts.filters[i].0, "file chooser filter");
        // Search results depend on the filter: search again.
        if let Shown::Search { .. } = view.shown.clone() {
            let to = view.shown.clone();
            self.files_go(to, false);
            return;
        }
        view.refresh_rows();
        tracing::info!(rows = view.rows.len(), "file chooser filtered");
        self.draw_dialog();
    }

    fn files_sort_by(&mut self, column: SortColumn) {
        let Some(view) = self.files_view() else { return };
        let s = &mut view.settings.sort;
        if s.column == column {
            s.ascending = !s.ascending;
        } else {
            s.column = column;
            s.ascending = column != SortColumn::Modified;
        }
        view.settings.store();
        view.refresh_rows();
        self.draw_dialog();
    }

    fn files_toggle_hidden(&mut self) {
        let Some(view) = self.files_view() else { return };
        view.settings.hidden = !view.settings.hidden;
        view.settings.store();
        view.refresh_rows();
        tracing::info!(hidden = view.settings.hidden, rows = view.rows.len(), "file chooser hidden files");
        self.draw_dialog();
    }

    /// Keys while the chooser is up.
    pub(super) fn files_key(&mut self, event: &KeyEvent, ready: bool) {
        let mods: Modifiers = self.mods;
        let now = std::time::Instant::now();
        let Some(view) = self.files_view() else { return };
        let enter = matches!(event.keysym, Keysym::Return | Keysym::KP_Enter);
        if enter && !ready {
            return;
        }
        // Asking before replacing: Enter replaces, Escape goes back.
        if view.replace.is_some() {
            match event.keysym {
                Keysym::Escape => {
                    view.replace = None;
                    self.draw_dialog();
                }
                _ if enter => {
                    if let Some(path) = view.replace.take() {
                        self.files_done(vec![path]);
                    }
                }
                _ => {}
            }
            return;
        }
        // The file type menu.
        if view.dropdown {
            let n = view.opts.filters.len().max(1);
            match event.keysym {
                Keysym::Escape => view.dropdown = false,
                Keysym::Up => view.dropdown_at = (view.dropdown_at + n - 1) % n,
                Keysym::Down => view.dropdown_at = (view.dropdown_at + 1) % n,
                Keysym::space => {
                    let i = view.dropdown_at;
                    return self.files_set_filter(i);
                }
                _ if enter => {
                    let i = view.dropdown_at;
                    return self.files_set_filter(i);
                }
                _ => {}
            }
            return self.draw_dialog();
        }
        let printable = event.utf8.as_ref().filter(|t| !t.is_empty() && !t.chars().any(char::is_control)).cloned();
        // Shortcuts anywhere.
        match event.keysym {
            Keysym::Escape => {
                if view.field.is_some() {
                    return self.files_close_field();
                }
                view.cancel();
                self.dialog = None;
                return;
            }
            Keysym::Up if mods.alt => return self.files_parent(),
            Keysym::Left if mods.alt => return self.files_history(true),
            Keysym::Right if mods.alt => return self.files_history(false),
            Keysym::Home if mods.alt => {
                let home = view.home.clone();
                return self.files_go(Shown::Folder(home), true);
            }
            Keysym::l | Keysym::L if mods.ctrl => {
                let start = view.folder().map(|f| format!("{}/", files::shown_path(f, &view.home).trim_end_matches('/'))).unwrap_or_default();
                return self.files_open_field(FieldUse::Location, &start);
            }
            Keysym::d | Keysym::D if mods.alt => {
                let start = view.folder().map(|f| format!("{}/", files::shown_path(f, &view.home).trim_end_matches('/'))).unwrap_or_default();
                return self.files_open_field(FieldUse::Location, &start);
            }
            Keysym::h | Keysym::H if mods.ctrl => return self.files_toggle_hidden(),
            Keysym::f | Keysym::F if mods.ctrl && view.opts.mode == Mode::Open => return self.files_open_field(FieldUse::Search, ""),
            Keysym::Tab | Keysym::ISO_Left_Tab => {
                let mut order = Vec::new();
                if view.opts.mode == Mode::Save {
                    order.push(Focus::Name);
                }
                if view.field.is_some() {
                    order.push(Focus::Field);
                }
                order.push(Focus::List);
                if view.opts.check.is_some() {
                    order.push(Focus::Check);
                }
                if !view.opts.filters.is_empty() && view.opts.mode.lists_files() {
                    order.push(Focus::Filter);
                }
                order.extend([Focus::Cancel, Focus::Accept]);
                let i = order.iter().position(|f| *f == view.focus).unwrap_or(0);
                let n = order.len();
                view.focus = order[if event.keysym == Keysym::ISO_Left_Tab || mods.shift { (i + n - 1) % n } else { (i + 1) % n }];
                return self.draw_dialog();
            }
            _ => {}
        }
        match view.focus {
            Focus::Name | Focus::Field => {
                let field = view.focus == Focus::Field;
                if enter {
                    if field {
                        return self.files_field_enter();
                    }
                    return self.files_accept();
                }
                let Some(edit) = (if field { view.field.as_mut().map(|(_, e)| e) } else { Some(&mut view.name) }) else { return };
                let shift = mods.shift;
                match event.keysym {
                    Keysym::BackSpace => edit.backspace(),
                    Keysym::Delete | Keysym::KP_Delete => edit.delete(),
                    Keysym::Left | Keysym::KP_Left => edit.step(-1, shift),
                    Keysym::Right | Keysym::KP_Right => edit.step(1, shift),
                    Keysym::Home | Keysym::KP_Home => edit.move_to(0, shift),
                    Keysym::End | Keysym::KP_End => edit.move_to(usize::MAX, shift),
                    Keysym::a | Keysym::A if mods.ctrl => edit.select_all(),
                    // Down from the name goes to the list.
                    Keysym::Down if !field => view.focus = Focus::List,
                    _ if !mods.ctrl && !mods.alt => {
                        if let Some(t) = &printable {
                            edit.insert(t);
                        }
                    }
                    _ => {}
                }
                if !field {
                    view.warning.clear();
                }
                self.draw_dialog()
            }
            Focus::List => {
                let page = view.page() as isize;
                let shift = mods.shift;
                match event.keysym {
                    _ if enter => {
                        if view.opts.mode.saves() {
                            return self.files_accept();
                        }
                        match view.current {
                            Some(r) => return self.files_activate(r),
                            None => return self.files_accept(),
                        }
                    }
                    Keysym::Down | Keysym::KP_Down => view.move_by(1, shift),
                    Keysym::Up | Keysym::KP_Up => view.move_by(-1, shift),
                    Keysym::Page_Down | Keysym::KP_Page_Down => view.move_by(page, shift),
                    Keysym::Page_Up | Keysym::KP_Page_Up => view.move_by(-page, shift),
                    Keysym::Home | Keysym::KP_Home if !view.rows.is_empty() => view.select(0, shift, false),
                    Keysym::End | Keysym::KP_End if !view.rows.is_empty() => {
                        let last = view.rows.len() - 1;
                        view.select(last, shift, false);
                    }
                    Keysym::BackSpace => return self.files_parent(),
                    Keysym::a | Keysym::A if mods.ctrl && view.opts.multiple => {
                        view.picked = view.rows.iter().copied().filter(|&e| !view.entries[e].dir).collect();
                    }
                    Keysym::space if view.opts.multiple && view.typeahead_shown(now).is_none() => {
                        if let Some(r) = view.current {
                            let e = view.rows[r];
                            if !view.entries[e].dir && !view.picked.remove(&e) {
                                view.picked.insert(e);
                            }
                        }
                    }
                    _ if !mods.ctrl && !mods.alt => {
                        let Some(t) = printable else { return };
                        // `/` and `~` start a location (as in GTK).
                        if view.typeahead_shown(now).is_none() && (t == "/" || t == "~") {
                            return self.files_open_field(FieldUse::Location, &t);
                        }
                        view.type_ahead(&t, now);
                        tracing::info!(typed = %view.typeahead, current = ?view.current_entry().map(|e| &e.name), "file chooser type-ahead");
                        // Redraw when the bubble goes.
                        if let Some(handle) = self.loop_handle.clone() {
                            let timer = smithay_client_toolkit::reexports::calloop::timer::Timer::from_duration(TYPEAHEAD_RESET + std::time::Duration::from_millis(50));
                            let _ = handle.insert_source(timer, |_, _, ui| {
                                ui.draw_dialog();
                                smithay_client_toolkit::reexports::calloop::timer::TimeoutAction::Drop
                            });
                        }
                        return self.draw_dialog();
                    }
                    _ => return,
                }
                if let Some(e) = view.current_entry() {
                    tracing::info!(current = %e.name, "file chooser current row");
                }
                self.draw_dialog()
            }
            Focus::Check => {
                if event.keysym == Keysym::space {
                    view.check = !view.check;
                } else if enter {
                    return self.files_accept();
                }
                self.draw_dialog()
            }
            Focus::Filter => {
                if event.keysym == Keysym::space || matches!(event.keysym, Keysym::Down) {
                    view.dropdown = true;
                    view.dropdown_at = view.filter.unwrap_or(0);
                } else if enter {
                    return self.files_accept();
                }
                self.draw_dialog()
            }
            Focus::Cancel => {
                if enter || event.keysym == Keysym::space {
                    view.cancel();
                    self.dialog = None;
                }
            }
            Focus::Accept => {
                if enter || event.keysym == Keysym::space {
                    self.files_accept();
                }
            }
        }
    }

    /// Pointer input on the chooser's surface.
    pub(super) fn files_pointer(&mut self, event: &PointerEvent) {
        let (x, y) = event.position;
        let mods = self.mods;
        let Some(d) = self.dialog.as_mut() else { return };
        let found = d.hits.iter().find(|(r, _)| r.contains(x, y)).copied();
        let part = found.map(|(_, h)| h);
        let DialogKind::Files(view) = &mut d.state.kind else { return };
        // The wheel scrolls the list or the places.
        if let PointerEventKind::Axis { vertical, .. } = &event.kind {
            let rows = f64::from(view.row_h);
            let px = if vertical.discrete != 0 { f64::from(vertical.discrete) * NOTCH_ROWS * rows } else { vertical.absolute };
            match part {
                Some(OverlayHit::Files(FilesHit::Place(_) | FilesHit::Sidebar)) => {
                    let total = view.opts.places.len() as f64 * rows + 30.0;
                    let room = d.hits.iter().find(|(_, h)| *h == OverlayHit::Files(FilesHit::Sidebar)).map_or(0.0, |(r, _)| f64::from(r.h));
                    view.places_scroll = (view.places_scroll + px).clamp(0.0, (total - room).max(0.0));
                }
                _ => {
                    view.scroll_by(px);
                    tracing::debug!(scroll = view.scroll, "file chooser scrolled");
                }
            }
            d.state.pressed = None;
            self.draw_dialog();
            // What is under the pointer now.
            if let Some(d) = self.dialog.as_mut() {
                let now = d.hits.iter().find(|(r, _)| r.contains(x, y)).map(|(_, h)| *h);
                if now != d.state.hover {
                    d.state.hover = now;
                    self.draw_dialog();
                }
            }
            return;
        }
        // Resizing the dialog or the places (the size is kept when the
        // button goes up).
        if let Some(resizing) = view.resizing {
            match event.kind {
                PointerEventKind::Motion { .. } => {
                    match resizing {
                        // The dialog stays centred: an edge moves the
                        // opposite one too.
                        Resizing::Dialog { edges, from, size } => {
                            let (mut nw, mut nh) = size;
                            let (dx, dy) = ((x - from.0).round() as i32, (y - from.1).round() as i32);
                            if edges & EDGE_LEFT != 0 {
                                nw -= 2 * dx;
                            }
                            if edges & EDGE_RIGHT != 0 {
                                nw += 2 * dx;
                            }
                            if edges & EDGE_TOP != 0 {
                                nh -= 2 * dy;
                            }
                            if edges & EDGE_BOTTOM != 0 {
                                nh += 2 * dy;
                            }
                            view.settings.window_size = Some((nw.max(1), nh.max(1)));
                        }
                        Resizing::Sidebar { from, width } => view.settings.sidebar_width = Some((width + (x - from).round() as i32).max(1)),
                    }
                    return self.draw_dialog();
                }
                PointerEventKind::Release { .. } => {
                    view.resizing = None;
                    // Kept as drawn (within the screen's bounds).
                    match resizing {
                        Resizing::Dialog { .. } => view.settings.window_size = Some(view.shown_size),
                        Resizing::Sidebar { .. } => view.settings.sidebar_width = Some(view.shown_sidebar),
                    }
                    view.settings.store_size();
                    tracing::info!(size = ?view.settings.window_size, sidebar = ?view.settings.sidebar_width, "file chooser resized");
                    d.state.pressed = None;
                    return self.draw_dialog();
                }
                _ => {}
            }
        }
        // Dragging the scrollbar.
        if let Some(grab) = view.dragging {
            match event.kind {
                PointerEventKind::Motion { .. } => {
                    let (track_h, thumb_h) = scrollbar_sizes(view);
                    let max = view.max_scroll();
                    let span = f64::from((track_h - thumb_h).max(1));
                    let top = d.hits.iter().find(|(_, h)| *h == OverlayHit::Files(FilesHit::Scrollbar)).map_or(0.0, |(r, _)| f64::from(r.y + 4));
                    view.scroll = ((y - top - grab) / span * max).clamp(0.0, max);
                    return self.draw_dialog();
                }
                PointerEventKind::Release { .. } => {
                    view.dragging = None;
                    d.state.pressed = None;
                    return self.draw_dialog();
                }
                _ => {}
            }
        }
        if let PointerEventKind::Press { button: super::BTN_LEFT, time, .. } = event.kind {
            // An open menu closes on a press elsewhere.
            if view.dropdown && !matches!(part, Some(OverlayHit::Files(FilesHit::FilterItem(_) | FilesHit::FilterButton))) {
                view.dropdown = false;
                d.state.pressed = None;
                return self.draw_dialog();
            }
            match part {
                Some(OverlayHit::Files(FilesHit::Resize(edges))) => {
                    view.resizing = Some(Resizing::Dialog { edges, from: (x, y), size: view.shown_size });
                    d.state.pressed = part;
                    return;
                }
                Some(OverlayHit::Files(FilesHit::SidebarEdge)) => {
                    view.resizing = Some(Resizing::Sidebar { from: x, width: view.shown_sidebar });
                    d.state.pressed = part;
                    return self.draw_dialog();
                }
                Some(OverlayHit::Files(FilesHit::Row(r))) => {
                    // Rows act on the press: select, and a second press
                    // on the same entry within the double-click time
                    // activates it.
                    let Some(&e) = view.rows.get(r) else { return };
                    let double = view.last_press.is_some_and(|(pe, t)| pe == e && time.wrapping_sub(t) <= view.settings.double_click_ms);
                    view.select(r, mods.shift && view.opts.multiple, mods.ctrl && view.opts.multiple);
                    view.typeahead.clear();
                    if view.opts.mode != Mode::Save {
                        view.focus = Focus::List;
                    }
                    let entry = view.entries[e].clone();
                    if double {
                        view.last_press = None;
                        tracing::info!(row = %entry.name, "file chooser double-click");
                        return self.files_activate(r);
                    }
                    view.last_press = Some((e, time));
                    // Save: a file's name goes into Name (the base
                    // selected).
                    if view.opts.mode == Mode::Save && !entry.dir {
                        view.name = TextEdit::selecting(&entry.name, 0, model::base_name_end(&entry.name));
                        view.focus = Focus::Name;
                    }
                    d.state.pressed = None;
                    return self.draw_dialog();
                }
                Some(OverlayHit::Files(FilesHit::Scrollbar)) => {
                    let (track_h, thumb_h) = scrollbar_sizes(view);
                    let top = found.map_or(0.0, |(r, _)| f64::from(r.y + 4));
                    let max = view.max_scroll();
                    let span = f64::from((track_h - thumb_h).max(1));
                    let thumb_y = top + view.scroll / max.max(1.0) * span;
                    let grab = if y >= thumb_y && y < thumb_y + f64::from(thumb_h) {
                        y - thumb_y
                    } else {
                        // Pressed beside the thumb: it jumps there.
                        let g = f64::from(thumb_h) / 2.0;
                        view.scroll = ((y - top - g) / span * max).clamp(0.0, max);
                        g
                    };
                    view.dragging = Some(grab);
                    d.state.pressed = part;
                    return self.draw_dialog();
                }
                Some(OverlayHit::Files(FilesHit::Name)) => {
                    let r = found.map(|(r, _)| r).unwrap_or_default();
                    place_caret(&mut view.name, self.text.as_ref(), r, x);
                    view.focus = Focus::Name;
                    return self.draw_dialog();
                }
                Some(OverlayHit::Files(FilesHit::Field)) => {
                    let r = found.map(|(r, _)| r).unwrap_or_default();
                    if let Some((_, edit)) = view.field.as_mut() {
                        place_caret(edit, self.text.as_ref(), r, x);
                    }
                    view.focus = Focus::Field;
                    return self.draw_dialog();
                }
                Some(OverlayHit::Files(FilesHit::List)) => {
                    view.focus = Focus::List;
                    return self.draw_dialog();
                }
                _ => {}
            }
        }
        if matches!(event.kind, PointerEventKind::Press { .. } | PointerEventKind::Release { .. }) {
            tracing::debug!(?part, pressed = ?d.state.pressed, kind = ?event.kind, x, y, "file chooser button");
        }
        let out = super::press::track(&mut d.state.hover, &mut d.state.pressed, &event.kind, part);
        match out.click {
            Some(h) => self.files_click(h),
            None if out.redraw => self.draw_dialog(),
            None => {}
        }
    }

    /// A click (press and release) on part `h`.
    pub(super) fn files_click(&mut self, h: OverlayHit) {
        crate::click_feedback();
        let Some(view) = self.files_view() else { return };
        let files_hit = match h {
            OverlayHit::Cancel => {
                view.cancel();
                self.dialog = None;
                return;
            }
            OverlayHit::Confirm => return self.files_accept(),
            OverlayHit::Files(f) => f,
            _ => return,
        };
        match files_hit {
            FilesHit::Place(i) => {
                let Some(place) = view.opts.places.get(i).cloned() else { return };
                view.field = None;
                match place.path {
                    None => self.files_go(Shown::Recent, true),
                    Some(path) => self.files_go(Shown::Folder(path), true),
                }
            }
            FilesHit::Crumb(i) => {
                let crumbs = view.crumbs();
                if let Some((_, path, _)) = crumbs.get(i) {
                    let to = Shown::Folder(path.clone());
                    // Going up the branch: the folder below is selected.
                    if let Some((_, below, _)) = crumbs.get(i + 1) {
                        if view.folder().is_some_and(|f| f.starts_with(below)) {
                            view.reselect = below.file_name().map(|n| n.to_string_lossy().into_owned());
                        }
                    }
                    self.files_go_keeping(to);
                }
            }
            FilesHit::CrumbMore => {
                // The deepest folder that did not fit.
                let crumbs = view.crumbs();
                let shown_first = self.dialog.as_ref().and_then(|d| d.hits.iter().find_map(|(_, h)| if let OverlayHit::Files(FilesHit::Crumb(i)) = h { Some(*i) } else { None }));
                if let Some(path) = shown_first.and_then(|i| i.checked_sub(1)).and_then(|i| crumbs.get(i)).map(|c| c.1.clone()) {
                    self.files_go(Shown::Folder(path), true);
                }
            }
            FilesHit::Back => self.files_history(true),
            FilesHit::Forward => self.files_history(false),
            FilesHit::Search => {
                if matches!(view.field, Some((FieldUse::Search, _))) || matches!(view.shown, Shown::Search { .. }) {
                    self.files_close_field();
                } else {
                    self.files_open_field(FieldUse::Search, "");
                }
            }
            FilesHit::NewFolder => {
                if matches!(view.field, Some((FieldUse::NewFolder, _))) {
                    self.files_close_field();
                } else if view.folder().is_some() {
                    self.files_open_field(FieldUse::NewFolder, "");
                }
            }
            FilesHit::Column(c) => self.files_sort_by(c),
            FilesHit::FilterButton => {
                view.dropdown = !view.dropdown;
                view.dropdown_at = view.filter.unwrap_or(0);
                view.focus = Focus::Filter;
                self.draw_dialog();
            }
            FilesHit::FilterItem(i) => self.files_set_filter(i),
            FilesHit::Check => {
                view.check = !view.check;
                view.focus = Focus::Check;
                self.draw_dialog();
            }
            FilesHit::ReplaceCancel => {
                view.replace = None;
                self.draw_dialog();
            }
            FilesHit::ReplaceConfirm => {
                if let Some(path) = view.replace.take() {
                    self.files_done(vec![path]);
                }
            }
            FilesHit::Eject(i) => {
                if let Some(root) = view.eject_root(i).map(str::to_owned) {
                    tracing::info!(%root, "file chooser eject");
                    crate::media::request(root, crate::media::Request::Eject);
                }
                self.draw_dialog();
            }
            FilesHit::Row(_) | FilesHit::Scrollbar | FilesHit::List | FilesHit::Sidebar | FilesHit::Name | FilesHit::Field | FilesHit::Resize(_) | FilesHit::SidebarEdge => self.draw_dialog(),
        }
    }

    /// Paint the chooser (and ask for the current file's preview).
    pub(super) fn paint_files_dialog(&mut self, overlay: &mut Overlay<DialogState>) {
        let started = std::time::Instant::now();
        let DialogKind::Files(view) = &mut overlay.state.kind else { return };
        // The current file's preview, asked for once (decoded off this
        // thread; only for the row selected).
        if let Some(e) = view.current_entry().filter(|e| files::previewable(e)) {
            let path = e.path.to_string_lossy().into_owned();
            if !view.previews.contains_key(&path) {
                view.previews.insert(path.clone(), None);
                let _ = self.requests.send(crate::UiRequest::Preview(path));
            }
        }
        let (w, h) = (overlay.width as usize, overlay.height as usize);
        let scale = self.scales.of(overlay.layer.wl_surface());
        let l = layout(w as i32, h as i32, theme::text_scale(), wants(view));
        view.note_sizes(l.row_h, l.body.h);
        (view.shown_size, view.shown_sidebar) = ((l.dialog.w, l.dialog.h), l.sidebar.w);
        let Ok((buffer, data)) = self.buffers.create_buffer(overlay.layer.wl_surface(), Ui::draw_dialog, physical(w, scale) as i32, physical(h, scale) as i32, (physical(w, scale) * 4) as i32, wl_shm::Format::Argb8888) else {
            return;
        };
        let mut canvas = Canvas::scaled(data, w, h, scale).mirrored(super::mirror::on(overlay.layer.wl_surface()));
        let cache = &mut self.icon_cache;
        let mut icon = |name: &str, color: u32| {
            let px = (16.0 * scale).round() as u32;
            let key = format!("sym:{name}@{px}#{color:06x}");
            let rgb = [(color >> 16) as u8, (color >> 8) as u8, color as u8];
            let name = name.to_owned();
            cache.get(key, move || crate::icons::symbolic(&name, px, rgb))
        };
        let now = std::time::Instant::now();
        let mut p = Paint { text: self.text.as_ref(), bold: self.bold.as_ref(), icon: &mut icon, now, now_unix: now_unix() };
        let painted = paint_files(&mut canvas, &mut p, (w, h), view, (overlay.state.hover, overlay.state.pressed));
        if painted.loading_shown && !view.loading_said {
            view.loading_said = true;
            tracing::info!("file chooser loading");
        }
        // Where the parts are, when that changes (at debug level: tests
        // click on them).
        if tracing::enabled!(tracing::Level::DEBUG) {
            let summary: Vec<String> = painted.hits.iter().map(|(r, h)| format!("{h:?}={},{},{},{}", r.x, r.y, r.w, r.h)).collect();
            let summary = summary.join(" ");
            if summary != view.logged_hits {
                tracing::debug!(hits = %summary, "file chooser parts");
                view.logged_hits = summary;
            }
        }
        let node = files_node(view, &painted, now);
        let drawn = painted.drawn.len();
        overlay.hits = painted.hits;
        self.atspi_record(overlay.layer.wl_surface(), vec![node]);
        let surface = overlay.layer.wl_surface();
        self.scales.prepare(&self.qh, surface, w as i32, h as i32);
        if buffer.attach_to(surface).is_ok() {
            overlay.layer.commit();
        }
        tracing::debug!(us = started.elapsed().as_micros() as u64, drawn, "file chooser painted");
    }

    /// The chooser's text field with the keyboard focus (accessible id).
    pub(super) fn files_focused_field(&self) -> Option<String> {
        let Some(DialogKind::Files(view)) = self.dialog.as_ref().map(|d| &d.state.kind) else { return None };
        match view.focus {
            Focus::Name if view.opts.mode == Mode::Save => Some("name".into()),
            Focus::Field if view.field.is_some() => Some("field".into()),
            _ => None,
        }
    }

    pub(super) fn files_field_text(&self, id: &str) -> Option<(String, bool)> {
        let Some(DialogKind::Files(view)) = self.dialog.as_ref().map(|d| &d.state.kind) else { return None };
        match id {
            "name" => Some((view.name.text.clone(), false)),
            "field" => view.field.as_ref().map(|(_, e)| (e.text.clone(), false)),
            _ => None,
        }
    }

    /// An assistive technology (or a paste) set field `id`'s text.
    pub(super) fn files_set_text(&mut self, id: &str, value: &str, range: Option<(usize, usize)>) {
        let Some(view) = self.files_view() else { return };
        let edit = match id {
            "name" => &mut view.name,
            "field" => match view.field.as_mut() {
                Some((_, e)) => e,
                None => return,
            },
            _ => return,
        };
        let new = super::atspi::edited(&edit.text, false, value, range);
        *edit = TextEdit::new(&new);
        self.draw_dialog();
    }
}

/// The scrollbar's track and thumb heights for `view`'s list.
fn scrollbar_sizes(view: &ChooserView) -> (i32, i32) {
    let track = view.body_h - 8;
    let content = view.content_h().max(1.0);
    let thumb = ((f64::from(track) * f64::from(view.body_h) / content) as i32).max(24).min(track);
    (track, thumb)
}

/// Put `edit`'s caret where `x` falls in field `r`.
fn place_caret(edit: &mut TextEdit, text: Option<&Text>, r: Rect, x: f64) {
    let Some(t) = text else {
        edit.move_to(usize::MAX, false);
        return;
    };
    let advances = t.advances(&edit.text, size::LABEL);
    let mut at = f64::from(r.x + 10);
    let mut i = 0;
    for a in advances {
        if at + f64::from(a) / 2.0 > x {
            break;
        }
        at += f64::from(a);
        i += 1;
    }
    edit.move_to(i, false);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, dir: bool) -> Entry {
        Entry { name: name.into(), path: PathBuf::from("/home/ada/Documents").join(name), dir, size: 1234, modified: 1_700_000_000, content_type: files::fast_content_type(name) }
    }

    fn options(mode: Mode) -> Options {
        Options {
            title: "Open Document".into(),
            accept: "Open".into(),
            mode,
            multiple: false,
            folder: Some("/home/ada/Documents".into()),
            name: "Untitled document.odt".into(),
            filters: vec![("PDF documents".into(), vec![(0, "*.pdf".into())]), ("All Files".into(), vec![(0, "*".into())])],
            filter: Some(1),
            check: None,
            places: vec![Place { kind: PlaceKind::Home, label: String::new(), path: Some("/home/ada".into()) }, Place { kind: PlaceKind::Standard, label: "Documents".into(), path: Some("/home/ada/Documents".into()) }],
        }
    }

    /// The dialog's place and buttons depend on the screen, never on the
    /// folder (helixdesktop/desktop#116).
    #[test]
    fn eject_buttons_and_sizes() {
        use crate::media::MountInfo;
        let home = Path::new("/home/ada");
        let mut o = options(Mode::Open);
        o.places.push(Place { kind: PlaceKind::Drive, label: "SANDISK".into(), path: Some("/media/ada/SANDISK".into()) });
        o.places.push(Place { kind: PlaceKind::Drive, label: "Old".into(), path: Some("/media/ada/Old".into()) });
        let mut v = sample_view(o, Shown::Folder("/home/ada/Documents".into()), Vec::new(), home);
        let mount = |root: &str, mounted| MountInfo { name: String::new(), root: root.into(), icons: Vec::new(), ejectable: true, network: false, mounted };
        v.mounts = mount_paths(&[mount("file:///media/ada/SANDISK", true), mount("volume:1234", false), mount("file:///home/ada/Documents", true)]);
        // Only mounted drives (and network places) get one.
        assert_eq!(v.eject_root(2), Some("file:///media/ada/SANDISK"));
        assert_eq!((v.eject_root(1), v.eject_root(3)), (None, None));
        // A size the user chose, kept within the screen and above the
        // smallest; the places within a third of the dialog.
        let sized = |size, sidebar| layout(1280, 800, 1.0, Wants { size, sidebar, ..Wants::default() });
        assert_eq!((sized(Some((900, 600)), None).dialog.w, sized(Some((900, 600)), None).dialog.h), (900, 600));
        assert_eq!(sized(Some((5000, 5000)), None).dialog, Rect { x: 12, y: 12, w: 1256, h: 776 });
        assert_eq!((sized(Some((100, 100)), None).dialog.w, sized(Some((100, 100)), None).dialog.h), (600, 440));
        assert_eq!(sized(Some((900, 600)), Some(250)).sidebar.w, 250);
        assert_eq!(sized(Some((900, 600)), Some(800)).sidebar.w, 300);
        assert_eq!(sized(Some((900, 600)), Some(10)).sidebar.w, 120);
    }

    #[test]
    fn layout_is_stable() {
        let home = Path::new("/home/ada");
        let few: Vec<Entry> = (0..2).map(|i| entry(&format!("a{i}.dat"), false)).collect();
        let many: Vec<Entry> = (0..200).map(|i| entry(&format!("photo-{i}.png"), false)).collect();
        let none: Vec<Entry> = (0..200).map(|i| entry(&format!("data-{i}.bin"), false)).collect();
        let dialog_of = |entries: Vec<Entry>| {
            let v = sample_view(options(Mode::Open), Shown::Folder("/home/ada/Documents".into()), entries, home);
            layout(1280, 800, 1.0, wants(&v))
        };
        let (a, b, c) = (dialog_of(few), dialog_of(many), dialog_of(none));
        assert_eq!((a.dialog, a.cancel, a.accept, a.sidebar), (b.dialog, b.cancel, b.accept, b.sidebar));
        assert_eq!((b.dialog, b.cancel, b.accept, b.sidebar), (c.dialog, c.cancel, c.accept, c.sidebar));
        assert!(b.preview.is_some() && c.preview.is_none(), "the preview pane only takes room from the list");
        // Every text scale from 1 to 2 and screens from 600 high: the
        // title, fields and buttons stay on the screen.
        for (w, h) in [(1280, 600), (1280, 720), (1280, 800), (1024, 600), (1920, 1080), (853, 600)] {
            for k in [1.0, 1.25, 1.5, 1.75, 2.0] {
                for save in [false, true] {
                    let l = layout(w, h, k, Wants { save, search: !save, new_folder: save, preview: true, ..Wants::default() });
                    let inside = |r: Rect| r.x >= 0 && r.y >= 0 && r.x + r.w <= w && r.y + r.h <= h;
                    for r in [l.dialog, l.cancel, l.accept, l.back, l.bar, l.footer] {
                        assert!(inside(r), "{r:?} off a {w}×{h} screen at text {k}");
                    }
                    assert!(l.title_base > l.dialog.y && l.title_base < l.back.y, "title above the path bar");
                    assert!(l.name.is_none_or(|n| inside(n) && n.y + n.h < l.body.y), "name above the list");
                    assert!(l.body.h >= l.row_h, "a row at least shows ({w}×{h}, {k}): {l:?}");
                    assert!(l.accept.y >= l.body.y + l.body.h, "buttons below the list");
                }
            }
        }
        let big = layout(1920, 1080, 1.0, Wants::default());
        assert!(big.dialog.w > 1000 && big.dialog.h > 800, "the dialog grows with the screen: {big:?}");
    }

    #[test]
    fn selection_and_keys() {
        let home = Path::new("/home/ada");
        let entries = vec![entry("Invoices", true), entry("Archive", true), entry("b.pdf", false), entry("a.pdf", false), entry(".hidden-config", false)];
        let mut v = sample_view(options(Mode::Open), Shown::Folder("/home/ada/Documents".into()), entries, home);
        v.note_sizes(32, 320);
        assert_eq!(v.current_entry().map(|e| e.name.as_str()), Some("Archive"), "the first row");
        let now = std::time::Instant::now();
        v.type_ahead("i", now);
        v.type_ahead("n", now);
        assert_eq!(v.current_entry().unwrap().name, "Invoices");
        v.move_by(10, false);
        assert_eq!(v.current_entry().unwrap().name, "b.pdf");
        assert_eq!(v.chosen_files(), [PathBuf::from("/home/ada/Documents/b.pdf")]);
        v.settings.hidden = true;
        v.refresh_rows();
        assert!(v.rows.iter().any(|&i| v.entries[i].name == ".hidden-config"));
        assert_eq!(v.current_entry().unwrap().name, "b.pdf", "the current row stays");
        // The filter applies at once.
        v.filter = Some(0);
        v.refresh_rows();
        assert_eq!(v.rows.len(), 4, "folders and PDFs");
        // Multiple: plain, Ctrl and Shift clicks.
        let mut m = sample_view(Options { multiple: true, ..options(Mode::Open) }, Shown::Folder("/home/ada/Documents".into()), (0..6).map(|i| entry(&format!("f{i}.txt"), false)).collect(), home);
        m.select(1, false, false);
        m.select(3, true, false);
        assert_eq!(m.chosen_files().len(), 3);
        m.select(5, false, true);
        assert_eq!(m.chosen_files().len(), 4);
        m.select(0, false, false);
        assert_eq!(m.chosen_files(), [PathBuf::from("/home/ada/Documents/f0.txt")]);
        // Save: the base name is selected, nothing in the list is.
        let s = sample_view(options(Mode::Save), Shown::Folder("/home/ada/Documents".into()), vec![entry("x.odt", false)], home);
        assert_eq!((s.focus, s.name.selection(), s.current), (Focus::Name, Some((0, 17)), None));
        assert_eq!(s.list_name(), "Files in Documents");
        assert_eq!(s.status(now), "Documents, 1 item");
        // The path bar keeps the branch below when going up.
        let mut c = sample_view(options(Mode::Open), Shown::Folder("/home/ada/Documents/Client/2026".into()), Vec::new(), home);
        c.shown = Shown::Folder("/home/ada/Documents".into());
        let crumbs = c.crumbs();
        assert_eq!(crumbs.iter().map(|c| (c.0.as_str(), c.2)).collect::<Vec<_>>(), [("Home", false), ("Documents", true), ("Client", false), ("2026", false)]);
        assert_eq!(c.current_place(), Some(1));
        c.shown = Shown::Folder("/home/ada/Documents/Client".into());
        assert_eq!(c.current_place(), Some(1), "the deepest place holding the folder");
    }

    #[test]
    fn middle_ellipsis_keeps_the_end() {
        let Some(t) = Text::load() else { return };
        let long = "Quarterly financial report for the board of directors — final revised version after legal review (3) copy.pdf";
        let s = middle_ellipsis(&t, long, 14.0, 300.0);
        assert!(s.contains('…') && s.ends_with("(3) copy.pdf") && s.starts_with("Quarterly"), "{s}");
        assert!(t.width(&s, 14.0) <= 300.0);
        assert_eq!(middle_ellipsis(&t, "short.txt", 14.0, 300.0), "short.txt");
    }
}
