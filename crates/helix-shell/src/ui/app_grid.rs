//! The app grid (helixdesktop/spec `contracts/wayland/README.md`, "App
//! grid"): the launcher with an empty search, as pages of icons that can
//! be rearranged and put into folders by dragging. The first page starts
//! with a "Recent" row: the apps launched last (a Helix addition).

use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerEventKind, BTN_LEFT, BTN_RIGHT};

use crate::item_feedback::{Feedback, Look, GHOST_SCALE, LAUNCH_LINGER};

use super::*;
use crate::app_grid::{self as layout, Pages, PerPage};

/// A recent row cell's key: its app's id behind this prefix, so that its
/// hover, focus and launch feedback is its own and not that of the same
/// app's cell in the grid below.
const RECENT_KEY: &str = "recent:";

fn recent_key(id: &str) -> String {
    format!("{RECENT_KEY}{id}")
}

/// The item (app or folder id) a cell key stands for.
pub(super) fn item_id(key: &str) -> &str {
    key.strip_prefix(RECENT_KEY).unwrap_or(key)
}

/// Whether a cell key is one of the "Recent" row's.
fn is_recent(key: &str) -> bool {
    key.starts_with(RECENT_KEY)
}

/// The "Recent" heading's height, and the gap between the row and the
/// grid below it.
fn heading_h() -> i32 {
    helix_draw::theme::grow(28)
}
const ROW_GAP: i32 = 10;

/// A cell's size: its label's room grows with the text scale.
fn cell_w() -> i32 {
    helix_draw::theme::grow(120)
}
fn cell_h() -> i32 {
    118 - 22 + helix_draw::theme::grow(22)
}
const ICON: i32 = 64;
const MINI: i32 = 26;
const DOT: i32 = 10;

/// What a drag would do if dropped now.
#[derive(Debug, Clone, PartialEq)]
enum Drop {
    /// Before item `index` of page `page`.
    Insert { page: usize, index: usize },
    /// Onto item `id` (an app makes a folder with it; a folder takes it).
    Combine(String),
    /// Back into the folder it came from (nothing changes).
    Stay,
}

#[derive(Debug, Clone)]
struct Drag {
    id: String,
    at: (f64, f64),
    /// Dragged out of this folder (its id).
    from_folder: Option<String>,
}

/// The grid's state while the launcher is open.
#[derive(Default)]
pub(super) struct GridView {
    pages: Pages,
    page: usize,
    /// The selected item of the page (counting the "Recent" row's first)
    /// or of the open folder.
    selected: Option<usize>,
    cols: usize,
    per_page: PerPage,
    /// The apps launched last (the latest first), and whether the
    /// "Recent" row may show (`app-grid-recent` and `remember-app-usage`).
    pub(super) recent_ids: Vec<String>,
    pub(super) recent_on: bool,
    /// The "Recent" row's apps (empty: no row), as last laid out.
    recent: Vec<String>,
    press: Option<(String, (f64, f64), Option<String>)>,
    drag: Option<Drag>,
    /// What the last paint showed: the cells by key (the "Recent" row's
    /// `row` first, then the page's), the row's heading, and the heading
    /// with the row.
    cells: Vec<(Rect, String)>,
    row: usize,
    heading: Option<Rect>,
    recent_area: Option<Rect>,
    dots: Vec<(Rect, usize)>,
    folder_box: Option<Rect>,
    folder_title: Option<Rect>,
    folder_cells: Vec<(Rect, String)>,
    area: Rect,
    /// The open folder's page, and its page dots.
    folder_page: usize,
    folder_dots: Vec<(Rect, usize)>,
    folder_pages: usize,
    /// A dragged icon held beyond the grid's left (false) or right (true)
    /// side turns the page after a moment.
    edge: Option<bool>,
    edge_timer: Option<smithay_client_toolkit::reexports::calloop::RegistrationToken>,
    /// Hover, selection, press and launch feedback, by item id.
    pub(super) feedback: Feedback<String>,
    /// The next frame while feedback moves.
    frame_timer: Option<smithay_client_toolkit::reexports::calloop::RegistrationToken>,
    /// An app was launched: the launcher closes when this fires.
    linger: Option<smithay_client_toolkit::reexports::calloop::RegistrationToken>,
}

impl GridView {
    /// A grid whose feedback moves as `motion` allows.
    pub(super) fn new(motion: helix_draw::motion::Motion) -> Self {
        Self { feedback: Feedback::with_motion(motion), ..Self::default() }
    }

    /// The item under (x, y): in the open folder's box, else on the page.
    fn item_at(&self, x: f64, y: f64) -> Option<String> {
        let cells = if self.folder_box.is_some() { &self.folder_cells } else { &self.cells };
        cells.iter().find(|(r, _)| r.contains(x, y)).map(|(_, id)| id.clone())
    }
}

/// How long a dragged icon is held at the side before the page turns,
/// and then between turns.
const EDGE_WAIT: std::time::Duration = std::time::Duration::from_millis(700);
const EDGE_REPEAT: std::time::Duration = std::time::Duration::from_millis(900);

impl GridView {
    fn page_items(&self) -> &[String] {
        self.pages.get(self.page).map_or(&[], Vec::as_slice)
    }

    /// The "Recent" row's cell keys on the page shown (only the first
    /// page has the row).
    fn row_keys(&self) -> Vec<String> {
        if self.page == 0 {
            self.recent.iter().map(|id| recent_key(id)).collect()
        } else {
            Vec::new()
        }
    }

    /// The page's cell keys as the selection counts them: the "Recent"
    /// row's, then the page's items.
    fn shown(&self) -> Vec<String> {
        let mut out = self.row_keys();
        out.extend(self.page_items().iter().cloned());
        out
    }
}

/// The "Recent" row for `model` with `cols` columns, and what the pages
/// then hold with `rows` rows (the first a row less while the row shows).
/// No row while it is turned off or nothing launched is still shown.
fn recent_layout(grid: &GridView, model: &Launcher, cols: usize, rows: usize) -> (Vec<String>, PerPage) {
    let recent = if grid.recent_on { layout::recent_row(&grid.recent_ids, |id| model.apps.iter().any(|a| a.id == id), cols) } else { Vec::new() };
    let first_rows = if recent.is_empty() { rows } else { rows.saturating_sub(1).max(1) };
    (recent, PerPage { first: (cols * first_rows).max(1), rest: (cols * rows).max(1) })
}

/// Where an arrow key moves the selection.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Nav {
    To(usize),
    /// Past the page's end (Right) or start (Left).
    NextPage,
    PrevPage,
}

/// Where `key` moves the selection `at` on a page showing `row` recent
/// apps (a line of their own above the grid) and then `count` items in
/// lines of `cols`; indices count the row's first. Down from the row goes
/// to the grid's first line, Up from it back to the row.
fn arrow(at: Option<usize>, key: Keysym, row: usize, count: usize, cols: usize) -> Nav {
    let total = row + count;
    let cols = cols.max(1);
    let Some(i) = at.filter(|_| total > 0) else { return Nav::To(0) };
    let last = total - 1;
    match key {
        Keysym::Right if i >= last => Nav::NextPage,
        Keysym::Right => Nav::To(i + 1),
        Keysym::Left if i == 0 => Nav::PrevPage,
        Keysym::Left => Nav::To(i - 1),
        Keysym::Down if i < row => Nav::To(if count == 0 { i } else { row + i.min(count - 1) }),
        Keysym::Down => Nav::To((i + cols).min(last)),
        Keysym::Up if i < row => Nav::To(i),
        Keysym::Up if i - row < cols && row > 0 => Nav::To((i - row).min(row - 1)),
        Keysym::Up => Nav::To(i.saturating_sub(cols)),
        _ => Nav::To(i),
    }
}

/// Tab (`forward`) or Shift+Tab from the selection `at`: the search field
/// (`None`), the "Recent" row's first app, the grid's first item, round.
fn tab(at: Option<usize>, row: usize, count: usize, forward: bool) -> Option<usize> {
    let mut stops = vec![None];
    if row > 0 {
        stops.push(Some(0));
    }
    if count > 0 {
        stops.push(Some(row));
    }
    let here = match at {
        None => 0,
        Some(i) if i < row => 1,
        Some(_) => stops.len() - 1,
    };
    let n = stops.len();
    stops[if forward { (here + 1) % n } else { (here + n - 1) % n }]
}

/// Where `drag` would land now (nothing on the "Recent" row: it only shows
/// what was launched).
fn drop_at(g: &GridView, model: &Launcher, drag: &Drag) -> Option<Drop> {
    let (x, y) = drag.at;
    if let Some(b) = g.folder_box {
        if drag.from_folder.is_some() && b.contains(x, y) {
            return Some(Drop::Stay);
        }
    }
    if g.folder_box.is_none() && g.recent_area.is_some_and(|r| r.contains(x, y)) {
        return None;
    }
    if let Some(&(_, page)) = g.dots.iter().find(|(r, _)| r.contains(x, y)) {
        return Some(Drop::Insert { page, index: usize::MAX });
    }
    let is_folder = model.folders.iter().any(|f| f.id == drag.id);
    for (i, (cell, id)) in g.cells.iter().skip(g.row).enumerate() {
        if !cell.contains(x, y) {
            continue;
        }
        // The icon's middle combines (apps only; not with itself).
        let icon = Rect { x: cell.x + (cell.w - ICON) / 2 + 8, y: cell.y + 18, w: ICON - 16, h: ICON - 16 };
        if !is_folder && *id != drag.id && icon.contains(x, y) {
            return Some(Drop::Combine(id.clone()));
        }
        let after = x > f64::from(cell.x + cell.w / 2);
        return Some(Drop::Insert { page: g.page, index: i + usize::from(after) });
    }
    g.area.contains(x, y).then_some(Drop::Insert { page: g.page, index: usize::MAX })
}

/// The grid's items: apps that are neither favorites nor in a folder, and
/// the folders: (id, name).
fn grid_items(model: &Launcher) -> Vec<(String, String)> {
    let in_folders: Vec<&String> = model.folders.iter().flat_map(|f| f.apps.iter()).collect();
    let mut out: Vec<(String, String)> = model
        .apps
        .iter()
        .filter(|a| !model.favorites.contains(&a.id) && !in_folders.contains(&&a.id))
        .map(|a| (a.id.clone(), a.name.clone()))
        .collect();
    out.extend(model.folders.iter().map(|f| (f.id.clone(), f.name.clone())));
    out
}

impl Ui {
    /// The grid is shown: an empty search.
    pub(super) fn grid_mode(&self) -> bool {
        self.launcher.as_ref().is_some_and(|v| v.model.query.is_empty())
    }

    /// Lay the grid out again (opened, folders or apps changed, size, the
    /// "Recent" row shown or hidden) with `cols` columns, pages holding
    /// `per_page` and `recent` as the "Recent" row.
    fn grid_refresh(grid: &mut GridView, model: &Launcher, cols: usize, per_page: PerPage, recent: Vec<String>) {
        // The first page holding more again (the row went away): from the
        // saved layout, so that what spilled comes back.
        let saved = if grid.pages.is_empty() || per_page.first > grid.per_page.first { layout::read() } else { grid.pages.clone() };
        let saved = if saved.is_empty() { grid.pages.clone() } else { saved };
        if grid.recent != recent && !grid.pages.is_empty() {
            // What the selection counts changed.
            grid.selected = None;
        }
        grid.pages = layout::arrange(&saved, &grid_items(model), per_page);
        grid.cols = cols;
        grid.per_page = per_page;
        grid.recent = recent;
        grid.page = grid.page.min(grid.pages.len().saturating_sub(1));
    }

    /// The recent apps were cleared ("Clear Recent Apps").
    pub(super) fn grid_clear_recent(&mut self) {
        crate::app_usage::clear_recent();
        tracing::info!("recent apps cleared");
        if let Some(view) = self.launcher.as_mut() {
            view.grid.recent_ids.clear();
        }
        self.draw_launcher();
    }

    /// `app-grid-recent` or `remember-app-usage` changed: the "Recent" row
    /// shows (`on`) or not.
    pub(super) fn grid_recent_setting(&mut self, on: bool) {
        self.recent_apps = on;
        if let Some(view) = self.launcher.as_mut() {
            view.grid.recent_on = on;
            view.grid.recent_ids = if on { crate::app_usage::recent() } else { Vec::new() };
        }
        self.draw_launcher();
    }

    /// The menu of the "Recent" heading, at surface point `at`.
    fn grid_recent_menu(&mut self, (x, y): (f64, f64)) {
        let Some(view) = self.launcher.as_ref() else { return };
        let origin = view
            .output
            .as_ref()
            .and_then(|o| self.output_state.info(o))
            .and_then(|i| i.logical_position)
            .unwrap_or((self.primary_area.0, self.primary_area.1));
        self.dock.menu_targets.clear();
        let items = vec![self.dock_menu_item(tr("Clear Recent Apps").into(), super::dock::DockTarget::ClearRecent)];
        self.dock.menu_over_launcher = true;
        self.dock.menu_from_grid = true;
        self.menu_by_key = false;
        self.open_menu(0, origin.0 + x as i32, origin.1 + y as i32, items, Vec::new());
    }

    fn item_name(model: &Launcher, id: &str) -> String {
        model.apps.iter().find(|a| a.id == id).map(|a| a.name.clone()).or_else(|| model.folders.iter().find(|f| f.id == id).map(|f| f.name.clone())).unwrap_or_default()
    }

    /// Carry out `drop` of `drag`.
    fn grid_apply(&mut self, drag: Drag, drop: Drop) {
        let Some(view) = self.launcher.as_mut() else { return };
        let per_page = view.grid.per_page;
        if drop == Drop::Stay {
            return;
        }
        // Out of its folder first.
        if let Some(folder) = &drag.from_folder {
            let left = view.model.folders.iter().find(|f| f.id == *folder).map_or(0, |f| f.apps.len().saturating_sub(1));
            crate::favorites::remove_from_folder(folder, &drag.id, left);
            if left == 0 {
                view.grid.pages = layout::remove(&view.grid.pages, folder);
            }
            view.model.open_folder = None;
        }
        let mut pages = view.grid.pages.clone();
        match drop {
            Drop::Insert { page, index } => {
                let index = index.min(pages.get(page).map_or(0, Vec::len));
                pages = layout::move_item(&pages, &drag.id, page, index, per_page);
            }
            Drop::Combine(target) => {
                if view.model.folders.iter().any(|f| f.id == target) {
                    tracing::info!(app = drag.id, folder = target, "app put into a folder");
                    crate::favorites::add_to_folder(&target, &drag.id);
                    pages = layout::remove(&pages, &drag.id);
                } else {
                    let name = crate::favorites::folder_name_for(&crate::favorites::categories(&drag.id), &crate::favorites::categories(&target));
                    if let Some(folder) = crate::favorites::create_folder(&name, &[target.clone(), drag.id.clone()]) {
                        tracing::info!(folder, name, "folder made");
                        for p in pages.iter_mut() {
                            if let Some(slot) = p.iter_mut().find(|x| **x == target) {
                                *slot = folder.clone();
                            }
                        }
                        pages = layout::remove(&pages, &drag.id);
                    }
                }
            }
            Drop::Stay => {}
        }
        view.grid.pages = pages;
        self.grid_reload();
    }

    /// Folders (or apps) changed: read them again, keep and save the layout.
    pub(super) fn grid_reload(&mut self) {
        let Some(view) = self.launcher.as_mut() else { return };
        let folders = crate::favorites::folders(&view.model.apps);
        let favorites = crate::favorites::favorites();
        let open = view.model.open_folder.and_then(|i| view.model.folders.get(i).map(|f| f.id.clone()));
        view.model.set_collections(favorites, folders);
        view.model.open_folder = open.and_then(|id| view.model.folders.iter().position(|f| f.id == id));
        let per_page = view.grid.per_page;
        view.grid.pages = layout::arrange(&view.grid.pages, &grid_items(&view.model), per_page);
        view.grid.page = view.grid.page.min(view.grid.pages.len().saturating_sub(1));
        layout::write(&view.grid.pages);
        self.draw_launcher();
    }

    /// Open the item of cell `key`: launch an app, open a folder.
    fn grid_open(&mut self, key: &str) {
        let id = item_id(key);
        let Some(view) = self.launcher.as_mut() else { return };
        if let Some(i) = view.model.folders.iter().position(|f| f.id == id) {
            view.model.enter_folder(i);
            view.grid.selected = None;
            view.grid.folder_page = 0;
            self.draw_launcher();
            return;
        }
        tracing::info!(app = id, recent = is_recent(key), "launching from the app grid");
        if let Err(err) = crate::launcher::launch(id) {
            tracing::warn!(%err, app = id, "launch failed");
        }
        // With animations, the launcher stays for the launch copy.
        if !view.grid.feedback.animate || view.grid.linger.is_some() {
            self.close_launcher(true);
            return;
        }
        view.grid.feedback.launch(key.to_owned(), std::time::Instant::now());
        if let Some(handle) = self.loop_handle.as_ref() {
            let token = handle.insert_source(Timer::from_duration(LAUNCH_LINGER), |_, _, ui: &mut Ui| {
                // Only the launcher this was for (not one opened since).
                if ui.launcher.as_ref().is_some_and(|v| v.grid.linger.is_some()) {
                    ui.close_launcher(true);
                }
                TimeoutAction::Drop
            });
            view.grid.linger = token.ok();
        }
        if view.grid.linger.is_none() {
            self.close_launcher(true);
            return;
        }
        self.draw_launcher();
    }

    /// Draw the launcher again next frame (feedback is moving).
    pub(super) fn grid_next_frame(&mut self) {
        let Some(view) = self.launcher.as_mut() else { return };
        if view.grid.frame_timer.is_some() {
            return;
        }
        let Some(handle) = self.loop_handle.as_ref() else { return };
        let token = handle.insert_source(Timer::from_duration(std::time::Duration::from_millis(16)), |_, _, ui: &mut Ui| {
            if let Some(view) = ui.launcher.as_mut() {
                view.grid.frame_timer = None;
                ui.grid_redraw();
            }
            TimeoutAction::Drop
        });
        view.grid.frame_timer = token.ok();
    }

    /// The right-click menu of the app of cell `key`, at surface point
    /// `at` (else at its icon); folders have none. The "Recent" row's
    /// apps' menus end with "Clear Recent Apps".
    fn grid_menu(&mut self, key: &str, at: Option<(f64, f64)>) {
        let id = item_id(key);
        let Some(view) = self.launcher.as_ref() else { return };
        if view.model.folders.iter().any(|f| f.id == id) {
            return;
        }
        let g = &view.grid;
        let cell = g.folder_cells.iter().chain(&g.cells).find(|(_, i)| i == key).map(|(r, _)| *r);
        let Some((x, y)) = at.or_else(|| cell.map(|r| (f64::from(r.x + r.w / 2), f64::from(r.y + r.h / 2)))) else { return };
        let origin = view
            .output
            .as_ref()
            .and_then(|o| self.output_state.info(o))
            .and_then(|i| i.logical_position)
            .unwrap_or((self.primary_area.0, self.primary_area.1));
        let app = id.strip_suffix(".desktop").unwrap_or(id).to_owned();
        let favorite = crate::favorites::favorites().iter().any(|f| f == id);
        self.dock.menu_targets.clear();
        let mut items = self.app_menu_items(&app, favorite, false);
        if is_recent(key) {
            items.push(None);
            items.push(self.dock_menu_item(tr("Clear Recent Apps").into(), super::dock::DockTarget::ClearRecent));
        }
        self.dock.menu_over_launcher = true;
        self.dock.menu_from_grid = true;
        // Shift+F10 or Menu (no pointer position): a keyboard menu.
        self.menu_by_key = at.is_none();
        self.open_menu(0, origin.0 + x as i32, origin.1 + y as i32, items, Vec::new());
    }

    /// Pointer input on the launcher while it shows the grid; true if it
    /// was used.
    pub(super) fn grid_pointer(&mut self, event: &PointerEvent) -> bool {
        if !self.grid_mode() || !self.launcher.as_ref().is_some_and(|v| v.layer.wl_surface() == &event.surface) {
            return false;
        }
        let (x, y) = event.position;
        match event.kind {
            PointerEventKind::Press { button: BTN_LEFT, .. } => {
                let Some(view) = self.launcher.as_mut() else { return true };
                let g = &view.grid;
                if let Some(title) = g.folder_title.filter(|t| t.contains(x, y)) {
                    let _ = title;
                    if let Some(folder) = view.model.open_folder.and_then(|i| view.model.folders.get(i)) {
                        let _ = self.requests.send(UiRequest::RenameFolder { id: folder.id.clone(), name: folder.name.clone() });
                    }
                    return true;
                }
                if let Some(&(_, q)) = g.folder_dots.iter().find(|(r, _)| r.contains(x, y)).filter(|_| g.folder_box.is_some()) {
                    view.grid.folder_page = q;
                    view.grid.selected = None;
                    self.draw_launcher();
                    return true;
                }
                let in_folder = g.folder_box.is_some();
                let from_folder = view.model.open_folder.and_then(|i| view.model.folders.get(i)).map(|f| f.id.clone());
                let hit = if in_folder { g.folder_cells.iter().find(|(r, _)| r.contains(x, y)) } else { g.cells.iter().find(|(r, _)| r.contains(x, y)) };
                if let Some((_, id)) = hit {
                    let id = id.clone();
                    view.grid.press = Some((id.clone(), (x, y), if in_folder { from_folder } else { None }));
                    view.grid.feedback.press(id, std::time::Instant::now());
                    self.grid_redraw();
                } else if let Some(&(_, page)) = g.dots.iter().find(|(r, _)| r.contains(x, y)) {
                    view.grid.page = page;
                    view.grid.selected = None;
                    self.draw_launcher();
                } else if in_folder && !g.folder_box.is_some_and(|b| b.contains(x, y)) {
                    view.model.leave_folder();
                    self.draw_launcher();
                } else if !in_folder && !view.content.contains(x, y) && !g.area.contains(x, y) {
                    self.close_launcher(true);
                }
            }
            PointerEventKind::Press { button: BTN_RIGHT, .. } => {
                let Some(view) = self.launcher.as_ref() else { return true };
                let on_heading = view.grid.folder_box.is_none() && view.grid.heading.is_some_and(|r| r.contains(x, y));
                match view.grid.item_at(x, y) {
                    Some(key) => self.grid_menu(&key, Some((x, y))),
                    None if on_heading => self.grid_recent_menu((x, y)),
                    None => {}
                }
            }
            PointerEventKind::Leave { .. } => {
                if let Some(view) = self.launcher.as_mut() {
                    view.grid.feedback.set_hover(None, std::time::Instant::now());
                    // Drawn once the pointer frame is done (`grid_flush`).
                    self.grid_pending = true;
                }
            }
            PointerEventKind::Motion { .. } => {
                let threshold = crate::drag_threshold();
                let Some(view) = self.launcher.as_mut() else { return true };
                if view.grid.drag.is_none() {
                    let under = view.grid.item_at(x, y);
                    if view.grid.feedback.hovered() != under.as_ref() {
                        view.grid.feedback.set_hover(under, std::time::Instant::now());
                        // Drawn once the pointer frame is done, only the
                        // cells that changed (`grid_flush`).
                        self.grid_pending = true;
                    }
                }
                let Some(view) = self.launcher.as_mut() else { return true };
                if let Some(drag) = view.grid.drag.as_mut() {
                    drag.at = (x, y);
                    // Dragged out of its folder: the folder closes.
                    let outside = view.grid.folder_box.is_some_and(|b| !b.contains(x, y));
                    if drag.from_folder.is_some() && outside {
                        view.model.open_folder = None;
                    }
                    let area = view.grid.area;
                    let edge = if view.model.open_folder.is_some() {
                        None
                    } else if x < f64::from(area.x - 8) {
                        Some(false)
                    } else if x > f64::from(area.x + area.w + 8) {
                        Some(true)
                    } else {
                        None
                    };
                    self.grid_edge(edge);
                    self.draw_launcher();
                } else if let Some((id, (sx, sy), from)) = view.grid.press.clone() {
                    if ((x - sx).powi(2) + (y - sy).powi(2)).sqrt() >= threshold {
                        view.grid.press = None;
                        view.grid.feedback.release(std::time::Instant::now());
                        // The "Recent" row's apps do not drag (the row
                        // only shows what was launched); the press is
                        // let go, nothing opens.
                        if !is_recent(&id) {
                            view.grid.drag = Some(Drag { id, at: (x, y), from_folder: from });
                        }
                        self.draw_launcher();
                    }
                }
            }
            PointerEventKind::Release { button: BTN_LEFT, .. } => {
                let Some(view) = self.launcher.as_mut() else { return true };
                view.grid.feedback.release(std::time::Instant::now());
                if let Some(drag) = view.grid.drag.take() {
                    self.grid_edge(None);
                    let drop = self.launcher.as_ref().and_then(|v| drop_at(&v.grid, &v.model, &drag));
                    tracing::info!(id = drag.id, ?drop, "app grid drop");
                    match drop {
                        Some(drop) => self.grid_apply(drag, drop),
                        None => self.draw_launcher(),
                    }
                } else if let Some((id, _, _)) = view.grid.press.take() {
                    // Released elsewhere: nothing opens.
                    if view.grid.item_at(x, y).as_deref() == Some(id.as_str()) {
                        self.grid_open(&id);
                    } else {
                        self.draw_launcher();
                    }
                }
            }
            PointerEventKind::Axis { vertical, horizontal, .. } => {
                tracing::debug!(?vertical, ?horizontal, "app grid scroll");
                let delta = if vertical.discrete != 0 { f64::from(vertical.discrete) } else if horizontal.discrete != 0 { f64::from(horizontal.discrete) } else { vertical.absolute + horizontal.absolute };
                if delta.abs() > 0.0 {
                    self.grid_turn(delta > 0.0);
                }
            }
            _ => {}
        }
        true
    }

    /// A dragged icon is now at the grid's side `edge` (or not): after
    /// [`EDGE_WAIT`] the page turns that way, again every [`EDGE_REPEAT`].
    fn grid_edge(&mut self, edge: Option<bool>) {
        let Some(view) = self.launcher.as_mut() else { return };
        if view.grid.edge == edge {
            return;
        }
        view.grid.edge = edge;
        if let (Some(handle), Some(token)) = (self.loop_handle.as_ref(), view.grid.edge_timer.take()) {
            handle.remove(token);
        }
        let (Some(forward), Some(handle)) = (edge, self.loop_handle.as_ref()) else { return };
        use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
        let token = handle.insert_source(Timer::from_duration(EDGE_WAIT), move |_, _, ui: &mut Ui| {
            let still = ui.launcher.as_ref().is_some_and(|v| v.grid.drag.is_some() && v.grid.edge == Some(forward));
            if !still {
                if let Some(v) = ui.launcher.as_mut() {
                    v.grid.edge_timer = None;
                }
                return TimeoutAction::Drop;
            }
            ui.grid_turn(forward);
            TimeoutAction::ToDuration(EDGE_REPEAT)
        });
        if let Some(view) = self.launcher.as_mut() {
            view.grid.edge_timer = token.ok();
        }
    }

    /// The next (or previous) page.
    fn grid_turn(&mut self, forward: bool) {
        let Some(view) = self.launcher.as_mut() else { return };
        if view.model.open_folder.is_some() {
            let last = view.grid.folder_pages.saturating_sub(1);
            let page = if forward { (view.grid.folder_page + 1).min(last) } else { view.grid.folder_page.saturating_sub(1) };
            if page != view.grid.folder_page {
                view.grid.folder_page = page;
                view.grid.selected = None;
                self.draw_launcher();
            }
            return;
        }
        let last = view.grid.pages.len().saturating_sub(1);
        let page = if forward { (view.grid.page + 1).min(last) } else { view.grid.page.saturating_sub(1) };
        if page != view.grid.page {
            view.grid.page = page;
            view.grid.selected = None;
            self.draw_launcher();
        }
    }

    /// Keys while the grid shows; true if used.
    pub(super) fn grid_key(&mut self, keysym: Keysym) -> bool {
        if !self.grid_mode() {
            return false;
        }
        let Some(view) = self.launcher.as_mut() else { return false };
        let in_folder = view.model.open_folder.and_then(|i| view.model.folders.get(i)).map(|f| f.apps.clone());
        // The "Recent" row's apps come first on the first page.
        let row = if in_folder.is_some() { 0 } else { view.grid.row_keys().len() };
        let (count, cols) = match &in_folder {
            Some(apps) => (apps.len(), apps.len().clamp(1, 6)),
            None => (view.grid.page_items().len(), view.grid.cols.max(1)),
        };
        let at = view.grid.selected;
        let pages = view.grid.pages.len();
        let shown = if in_folder.is_some() { Vec::new() } else { view.grid.shown() };
        match keysym {
            Keysym::Right | Keysym::Left | Keysym::Up | Keysym::Down if row + count > 0 => {
                let last = row + count - 1;
                view.grid.selected = Some(match arrow(at, keysym, row, count, cols) {
                    Nav::To(i) => i.min(last),
                    Nav::NextPage if in_folder.is_none() && view.grid.page + 1 < pages => {
                        view.grid.page += 1;
                        0
                    }
                    Nav::PrevPage if in_folder.is_none() && view.grid.page > 0 => {
                        view.grid.page -= 1;
                        view.grid.shown().len().saturating_sub(1)
                    }
                    Nav::NextPage => last,
                    Nav::PrevPage => 0,
                });
            }
            // From the search field to the "Recent" row, the grid and back.
            Keysym::Tab | Keysym::ISO_Left_Tab if in_folder.is_none() => {
                let forward = keysym == Keysym::Tab && !self.mods.shift;
                view.grid.selected = tab(at, row, count, forward);
            }
            Keysym::Page_Down => {
                self.grid_turn(true);
                return true;
            }
            Keysym::Page_Up => {
                self.grid_turn(false);
                return true;
            }
            Keysym::Return | Keysym::KP_Enter => {
                let id = match (&in_folder, at) {
                    (Some(apps), Some(i)) => apps.get(i).cloned(),
                    (None, Some(i)) => shown.get(i).cloned(),
                    _ => None,
                };
                if let Some(id) = id {
                    self.grid_open(&id);
                }
                return true;
            }
            Keysym::Menu => {
                let id = match (&in_folder, at) {
                    (Some(apps), Some(i)) => apps.get(i).cloned(),
                    (None, Some(i)) => shown.get(i).cloned(),
                    _ => None,
                };
                if let Some(id) = id {
                    self.grid_menu(&id, None);
                }
                return true;
            }
            Keysym::F10 if self.mods.shift => return self.grid_key(Keysym::Menu),
            Keysym::Escape if in_folder.is_some() => {
                view.model.leave_folder();
                view.grid.selected = None;
            }
            _ => return false,
        }
        self.draw_launcher();
        true
    }
}

pub(super) type Icons = crate::icon_cache::IconCache;

/// App `id`'s icon, if decoded yet (the cell is drawn without it until it
/// is: a grid of 169 apps would otherwise keep the UI thread for seconds).
fn grid_icon(cache: &mut Icons, id: &str, px: u32) -> Option<helix_draw::Image> {
    let app = id.strip_suffix(".desktop").unwrap_or(id).to_owned();
    let key = format!("app:{app}@{px}");
    cache.get(key, move || crate::icons::app_icon(&app, px))
}

/// Draw one grid cell (an app or a folder) at `cell`.
/// What grid painting needs besides the canvas.
pub(super) struct Paint<'a> {
    pub cache: &'a mut Icons,
    pub text: Option<&'a Text>,
    /// The bold face (the "Recent" heading).
    pub bold: Option<&'a Text>,
    pub scale: f64,
}

/// The center of a cell's icon.
fn icon_center(cell: Rect) -> (f64, f64) {
    (f64::from(cell.x) + f64::from(cell.w) / 2.0, f64::from(cell.y + 10) + f64::from(ICON) / 2.0)
}

/// Draw one grid cell; `focused`: it has the keyboard focus (a focus
/// ring, apart from the highlight that hover and selection share).
pub(super) fn paint_cell(p: &mut Paint, model: &Launcher, canvas: &mut Canvas, cell: Rect, id: &str, look: Look, focused: bool) {
    let (text, scale) = (p.text, p.scale);
    let folder = model.folders.iter().find(|f| f.id == id).cloned();
    let name = Ui::item_name(model, id);
    // The highlight: lit by hover or selection, stronger while pressed.
    let alpha = if look.pressed { 0x59 } else { (look.highlight * f64::from(0x40)).round() as u32 };
    let (bx, by, bw, bh) = (f64::from(cell.x + 4), f64::from(cell.y + 2), f64::from(cell.w - 8), f64::from(cell.h - 4));
    if alpha > 0 {
        if helix_draw::high_contrast() {
            canvas.round_outline(bx, by, bw, bh, 10.0, if look.pressed { 3.0 } else { 2.0 }, 0xFFFFFFFF);
        } else {
            canvas.round_rect(bx, by, bw, bh, 10.0, (alpha << 24) | 0xFFFFFF);
        }
    }
    if focused {
        helix_draw::theme::focus_ring_round(canvas, bx, by, bw, bh, 10.0);
    }
    let iy = cell.y + 10;
    let (cx, cy) = icon_center(cell);
    let k = look.scale;
    let size = f64::from(ICON) * k;
    let (sx, sy) = (cx - size / 2.0, cy - size / 2.0);
    match folder {
        Some(folder) => {
            canvas.round_rect(sx, sy, size, size, 12.0 * k, 0x50FFFFFF);
            let px = (f64::from(MINI) * k * scale).round() as u32;
            let (mini, gap) = (f64::from(MINI) * k, f64::from(MINI + 2) * k);
            for (n, app) in folder.apps.iter().take(4).enumerate() {
                let (mx, my) = (sx + 5.0 * k + (n % 2) as f64 * gap, sy + 5.0 * k + (n / 2) as f64 * gap);
                if let Some(icon) = grid_icon(p.cache, app, px) {
                    canvas.image(mx, my, mini, mini, &icon);
                }
            }
        }
        None => {
            let px = (f64::from(ICON) * scale).round() as u32;
            // (Nothing while it is being decoded; apps without an icon get
            // the generic one.)
            if let Some(icon) = grid_icon(p.cache, id, px) {
                canvas.image(sx, sy, size, size, &icon);
            }
        }
    }
    let tw = text.map_or(0.0, |t| t.width(&name, size::CAPTION)).min((cell.w - 12) as f32);
    label(text, canvas, &name, cell.x as f32 + (cell.w as f32 - tw) / 2.0, (iy + ICON + helix_draw::theme::grow(22)) as f32, size::CAPTION, FG, (cell.w - 12) as f32);
}

/// Paint the grid below the search field (from `top`): on the first page
/// the "Recent" row (a heading, one line of apps) above the grid.
pub(super) fn paint_grid(p: &mut Paint, grid: &mut GridView, model: &Launcher, canvas: &mut Canvas, (w, h): (i32, i32), top: i32) {
    let (text, scale) = (p.text, p.scale);
    let cols = ((w - 96) / cell_w()).clamp(3, 8) as usize;
    let rows = ((h - top - 72) / cell_h()).clamp(2, 5) as usize;
    let (recent, per_page) = recent_layout(grid, model, cols, rows);
    if grid.cols != cols || grid.per_page != per_page || grid.recent != recent {
        Ui::grid_refresh(grid, model, cols, per_page, recent);
    }
    let gw = cols as i32 * cell_w();
    let gx = (w - gw) / 2;
    let row_keys = grid.row_keys();
    // The row and its heading take the first page's first line, and the
    // heading's height more on every page (the dots stay put).
    let extra = if grid.recent.is_empty() { 0 } else { heading_h() + ROW_GAP };
    let gtop = if row_keys.is_empty() { top } else { top + heading_h() + cell_h() + ROW_GAP };
    let lines = if row_keys.is_empty() { rows } else { rows - 1 };
    let items: Vec<String> = grid.page_items().to_vec();
    let (selected, drag, open_folder) = (grid.selected, grid.drag.clone(), model.open_folder);
    let pages = grid.pages.len();
    let current = grid.page;
    let target = drag.as_ref().and_then(|d| drop_at(grid, model, d));
    // The keyboard selection lights its item (in the open folder, in the
    // "Recent" row or on the page).
    let now = std::time::Instant::now();
    let focus = match (selected, open_folder) {
        (Some(i), None) => row_keys.iter().chain(&items).nth(i).cloned(),
        (Some(i), Some(f)) => model.folders.get(f).and_then(|f| f.apps.get(i).cloned()),
        _ => None,
    };
    grid.feedback.set_focus(focus.clone(), now);
    let feedback = grid.feedback.clone();
    let look_of = |id: &str, hot: bool| -> Look {
        if drag.is_some() {
            return Look { highlight: if hot { 1.0 } else { 0.0 }, pressed: false, scale: 1.0 };
        }
        let mut look = feedback.look(&id.to_owned(), now);
        if hot {
            look.highlight = 1.0;
        }
        look
    };
    let mut cells = Vec::new();
    // The "Recent" row: its heading, its apps (cells of their own, keyed
    // apart from the same apps' cells below), a line under it.
    let (mut heading, mut recent_area) = (None, None);
    if !row_keys.is_empty() {
        let hh = heading_h();
        let face = p.bold.or(text);
        let color = if helix_draw::high_contrast() { FG } else { DIM_FG };
        label(face, canvas, &helix_i18n::trc("app grid heading", "Recent"), (gx + 14) as f32, baseline(face, top, hh, size::CAPTION), size::CAPTION, color, (gw - 28) as f32);
        heading = Some(Rect { x: gx, y: top, w: gw, h: hh });
        for (i, key) in row_keys.iter().enumerate() {
            let cell = Rect { x: gx + i as i32 * cell_w(), y: top + hh, w: cell_w(), h: cell_h() };
            cells.push((cell, key.clone()));
            let focused = open_folder.is_none() && focus.as_deref() == Some(key.as_str());
            paint_cell(p, model, canvas, cell, item_id(key), look_of(key, false), focused);
        }
        let line = if helix_draw::high_contrast() { 0x99FFFFFF } else { 0x26FFFFFF };
        canvas.rect(gx + 14, top + hh + cell_h() + ROW_GAP / 2, gw - 28, 1, line);
        recent_area = Some(Rect { x: gx, y: top, w: gw, h: hh + cell_h() + ROW_GAP });
    }
    for (i, id) in items.iter().enumerate() {
        let cell = Rect { x: gx + (i % cols) as i32 * cell_w(), y: gtop + (i / cols) as i32 * cell_h(), w: cell_w(), h: cell_h() };
        cells.push((cell, id.clone()));
        if drag.as_ref().is_some_and(|d| d.id == *id && d.from_folder.is_none()) {
            continue;
        }
        let hot = matches!(&target, Some(Drop::Combine(t)) if t == id);
        let focused = open_folder.is_none() && focus.as_deref() == Some(id.as_str());
        paint_cell(p, model, canvas, cell, id, look_of(id, hot), focused);
    }
    // Where a dragged item would go.
    if let Some(Drop::Insert { page, index }) = &target {
        if *page == current {
            let i = (*index).min(items.len());
            let (cx, cy) = (gx + (i % cols) as i32 * cell_w(), gtop + (i / cols) as i32 * cell_h());
            canvas.rect(cx - 2, cy + 8, 3, cell_h() - 16, accent());
        }
    }
    // Page dots.
    let dots_y = top + extra + rows as i32 * cell_h() + 18;
    let dots_x = (w - pages as i32 * (DOT + 10)) / 2;
    let mut dots = Vec::new();
    for p in 0..pages {
        let r = Rect { x: dots_x + p as i32 * (DOT + 10) - 5, y: dots_y - 10, w: DOT + 10, h: DOT + 20 };
        let color = if p == current { FG } else { 0x80FFFFFF };
        canvas.circle(f64::from(r.x + 5 + DOT / 2), f64::from(dots_y), f64::from(DOT) / 2.0, color);
        dots.push((r, p));
    }
    let area = Rect { x: gx, y: gtop, w: gw, h: lines as i32 * cell_h() };
    // An open folder: a box over the dimmed grid (and "Recent" row).
    let (mut folder_box, mut folder_title, mut folder_cells) = (None, None, Vec::new());
    if let Some(folder) = open_folder.and_then(|i| model.folders.get(i).cloned()) {
        canvas.rect(0, top - 8, w, h - top + 8, 0x80000000);
        let fcols = folder.apps.len().clamp(1, 6) as i32;
        let n = folder.apps.len() as i32;
        let rows_fit = ((h - top - 16 - 52 - 40) / cell_h()).max(1);
        let frows = ((n + fcols - 1) / fcols).min(rows_fit);
        let per = (fcols * rows_fit).max(1) as usize;
        let fpages = folder.apps.len().div_ceil(per).max(1);
        if let Some(i) = selected {
            grid.folder_page = i / per;
        }
        let fpage = grid.folder_page.min(fpages - 1);
        grid.folder_page = fpage;
        grid.folder_pages = fpages;
        let dots_h = if fpages > 1 { 30 } else { 0 };
        let bw = fcols * cell_w() + 32;
        let bh = frows * cell_h() + 72 + dots_h;
        let b = Rect { x: (w - bw) / 2, y: top + 8, w: bw, h: bh };
        theme::popover_round(canvas, f64::from(b.x), f64::from(b.y), f64::from(b.w), f64::from(b.h), theme::RADIUS_POPOVER + 8.0);
        let tw = text.map_or(0.0, |t| t.width(&folder.name, size::ENTRY));
        let title = Rect { x: b.x + (b.w - tw as i32) / 2 - 8, y: b.y + 10, w: tw as i32 + 16, h: 34 };
        label(text, canvas, &folder.name, (title.x + 8) as f32, baseline(text, title.y, title.h, size::ENTRY), size::ENTRY, FG, (b.w - 32) as f32);
        for (k, id) in folder.apps.iter().enumerate().skip(fpage * per).take(per) {
            let i = k - fpage * per;
            let cell = Rect { x: b.x + 16 + (i as i32 % fcols) * cell_w(), y: b.y + 52 + (i as i32 / fcols) * cell_h(), w: cell_w(), h: cell_h() };
            folder_cells.push((cell, id.clone()));
            if drag.as_ref().is_some_and(|d| d.id == *id) {
                continue;
            }
            let focused = focus.as_deref() == Some(id.as_str());
            paint_cell(p, model, canvas, cell, id, look_of(id, false), focused);
        }
        let mut fdots = Vec::new();
        if fpages > 1 {
            let dy = b.y + b.h - 18;
            let dx = b.x + (b.w - fpages as i32 * (DOT + 10)) / 2;
            for q in 0..fpages {
                let r = Rect { x: dx + q as i32 * (DOT + 10) - 5, y: dy - 10, w: DOT + 10, h: DOT + 20 };
                canvas.circle(f64::from(r.x + 5 + DOT / 2), f64::from(dy), f64::from(DOT) / 2.0, if q == fpage { FG } else { 0x80FFFFFF });
                fdots.push((r, q));
            }
        }
        grid.folder_dots = fdots;
        folder_box = Some(b);
        folder_title = Some(title);
    }
    // Launch copies growing out of their icons.
    for (id, progress) in feedback.ghosts(now) {
        let Some(&(cell, _)) = folder_cells.iter().chain(&cells).find(|(_, i)| *i == id) else { continue };
        let px = (f64::from(ICON) * scale).round() as u32;
        if let Some(icon) = grid_icon(p.cache, item_id(&id), px) {
            let (cx, cy) = icon_center(cell);
            let size = f64::from(ICON) * (1.0 + (GHOST_SCALE - 1.0) * progress);
            canvas.image_alpha(cx - size / 2.0, cy - size / 2.0, size, size, &icon, (0.8 * (1.0 - progress)) as f32);
        }
    }
    // The dragged item under the pointer.
    if let Some(d) = &drag {
        let px = (f64::from(ICON) * scale).round() as u32;
        let is_folder = model.folders.iter().any(|f| f.id == d.id);
        if is_folder {
            canvas.round_rect(d.at.0 - f64::from(ICON) / 2.0, d.at.1 - f64::from(ICON) / 2.0, f64::from(ICON), f64::from(ICON), 12.0, 0x80FFFFFF);
        } else if let Some(icon) = grid_icon(p.cache, &d.id, px) {
            canvas.image(d.at.0 - f64::from(ICON) / 2.0, d.at.1 - f64::from(ICON) / 2.0, f64::from(ICON), f64::from(ICON), &icon);
        }
    }
    // Logged when what the page shows changes (tests find the cells by
    // it).
    if grid.cells != cells {
        let shown: Vec<String> = cells.iter().map(|(r, key)| format!("{key}@{},{},{},{}", r.x, r.y, r.w, r.h)).collect();
        tracing::debug!(page = current, row = row_keys.len(), cells = shown.join(" "), "app grid shown");
    }
    grid.row = row_keys.len();
    grid.heading = heading;
    grid.recent_area = recent_area;
    grid.cells = cells;
    grid.dots = dots;
    grid.area = area;
    grid.folder_box = folder_box;
    grid.folder_title = folder_title;
    grid.folder_cells = folder_cells;
}

impl GridView {
    /// What the last paint showed, for the accessibility bus
    /// (`ui/atspi.rs`): the cells (the "Recent" row's first), the open
    /// folder's cells, the selected item, the page dots, the page, and how
    /// many cells the "Recent" row has.
    pub(super) fn a11y(&self) -> super::atspi::GridShown<'_> {
        (&self.cells, &self.folder_cells, self.selected, &self.dots, self.page, self.row)
    }
}

/// The launcher's app grid as last painted, so that a hover or feedback
/// change repaints only the cells whose look changed instead of the whole
/// full-screen launcher (helixdesktop/spec `contracts/wayland/README.md`,
/// "Item feedback").
pub(super) struct GridCache {
    /// The buffer's size in pixels and the scale it was painted at.
    key: (usize, usize, u64),
    /// The frame without the cells (scrim, search field, page dots), and
    /// the whole frame as shown.
    base: Vec<u8>,
    full: Vec<u8>,
    /// Each cell: its place, its item, the look it was painted with and
    /// whether it had the focus ring.
    cells: Vec<(Rect, String, Look, bool)>,
}

impl GridCache {
    /// The cache of a grid just painted (`base` before its cells, `full`
    /// after) on a `pw`×`ph` buffer at `scale`; none while something the
    /// cells alone do not show is on (an open folder, a drag, launch
    /// copies).
    pub(super) fn new(view: &LauncherView, base: Vec<u8>, full: Vec<u8>, (pw, ph, scale): (usize, usize, f64)) -> Option<Self> {
        let g = &view.grid;
        let now = std::time::Instant::now();
        if view.model.open_folder.is_some() || g.drag.is_some() || !g.feedback.ghosts(now).is_empty() {
            return None;
        }
        let focus = g.selected.and_then(|i| g.cells.get(i)).map(|(_, id)| id.clone());
        let cells = g.cells.iter().map(|(r, id)| (*r, id.clone(), g.feedback.look(id, now), focus.as_deref() == Some(id.as_str()))).collect();
        Some(Self { key: (pw, ph, scale.to_bits()), base, full, cells })
    }
}

/// The pixels (x0, y0, x1, y1) a cell `r` covers on a `pw`×`ph` buffer at
/// `scale` (mirrored: flipped left to right), a pixel wider each way.
fn cell_pixels(r: Rect, scale: f64, (pw, ph): (usize, usize), mirrored: bool) -> (usize, usize, usize, usize) {
    let lo = |v: i32| ((f64::from(v) * scale).floor() as i64 - 1).max(0) as usize;
    let hi = |v: i32, max: usize| (((f64::from(v) * scale).ceil() as i64 + 1).max(0) as usize).min(max);
    let (x0, x1) = (lo(r.x).min(pw), hi(r.x + r.w, pw));
    let (x0, x1) = if mirrored { (pw - x1, pw - x0) } else { (x0, x1) };
    (x0, lo(r.y).min(ph), x1, hi(r.y + r.h, ph))
}

/// The cells (indices) of a [`GridCache`] whose look or focus ring
/// changed since they were painted. Cells are told apart by key, so a
/// "Recent" row app's hover does not light the same app below.
fn dirty_cells(cells: &[(Rect, String, Look, bool)], feedback: &Feedback<String>, focus: Option<&str>, now: std::time::Instant) -> Vec<usize> {
    cells.iter().enumerate().filter(|(_, (_, id, look, focused))| feedback.look(id, now) != *look || (focus == Some(id.as_str())) != *focused).map(|(i, _)| i).collect()
}

/// Copy the pixels (x0, y0, x1, y1) of `from` into `to` (ARGB, `pw` wide).
fn copy_pixels(from: &[u8], to: &mut [u8], pw: usize, (x0, y0, x1, y1): (usize, usize, usize, usize)) {
    for y in y0..y1 {
        let (a, b) = ((y * pw + x0) * 4, (y * pw + x1) * 4);
        to[a..b].copy_from_slice(&from[a..b]);
    }
}

impl Ui {
    /// Draw the launcher's grid again after a hover or feedback change:
    /// only the cells whose look changed are painted (on the frame kept
    /// from the last full paint) and damaged. False when that cannot be
    /// done (the whole launcher is to be drawn).
    pub(super) fn grid_repaint_cells(&mut self) -> bool {
        let _busy = crate::watchdog::scope("draw app grid cells");
        let Some(mut view) = self.launcher.take() else { return true };
        let done = self.grid_repaint_in(&mut view);
        let moving = done && view.grid.feedback.moving(std::time::Instant::now());
        self.launcher = Some(view);
        if moving {
            self.grid_next_frame();
        }
        done
    }

    fn grid_repaint_in(&mut self, view: &mut LauncherView) -> bool {
        if !view.configured || !view.model.query.is_empty() || view.model.open_folder.is_some() || view.grid.drag.is_some() {
            return false;
        }
        let now = std::time::Instant::now();
        if !view.grid.feedback.ghosts(now).is_empty() {
            return false;
        }
        let (w, h) = (view.width as usize, view.height as usize);
        let scale = self.scales.of(view.layer.wl_surface());
        let (pw, ph) = (physical(w, scale), physical(h, scale));
        let Some(cache) = view.grid_cache.as_mut().filter(|c| c.key == (pw, ph, scale.to_bits())) else { return false };
        // The keyboard's item, as `paint_grid` lights it.
        let focus = view.grid.selected.and_then(|i| cache.cells.get(i)).map(|(_, id, _, _)| id.clone());
        view.grid.feedback.set_focus(focus.clone(), now);
        let dirty = dirty_cells(&cache.cells, &view.grid.feedback, focus.as_deref(), now);
        if dirty.is_empty() {
            return true;
        }
        let Ok((buffer, data)) = self.buffers.create_buffer(view.layer.wl_surface(), Ui::grid_redraw, pw as i32, ph as i32, (pw * 4) as i32, wl_shm::Format::Argb8888) else { return false };
        data.copy_from_slice(&cache.full);
        let mirrored = super::mirror::on(view.layer.wl_surface());
        let mut canvas = Canvas::scaled(data, w, h, scale).mirrored(mirrored);
        let mut damage = Vec::new();
        let mut paint = Paint { cache: &mut self.icon_cache, text: self.text.as_ref(), bold: self.bold.as_ref(), scale };
        for i in dirty {
            let (r, id, _, _) = cache.cells[i].clone();
            let px = cell_pixels(r, scale, (pw, ph), mirrored);
            copy_pixels(&cache.base, canvas.data, pw, px);
            canvas.clip(r.x, r.y, r.w, r.h);
            let look = view.grid.feedback.look(&id, now);
            let focused = focus.as_deref() == Some(id.as_str());
            paint_cell(&mut paint, &view.model, &mut canvas, r, item_id(&id), look, focused);
            canvas.unclip();
            copy_pixels(canvas.data, &mut cache.full, pw, px);
            cache.cells[i].2 = look;
            cache.cells[i].3 = focused;
            damage.push(px);
        }
        let surface = view.layer.wl_surface();
        self.scales.prepare_damage(&self.qh, surface, (w as i32, h as i32), &damage);
        if buffer.attach_to(surface).is_ok() {
            view.layer.commit();
        }
        true
    }

    /// A pointer frame is done: the grid drawn once if its hover changed.
    pub(super) fn grid_flush(&mut self) {
        if std::mem::take(&mut self.grid_pending) {
            self.grid_redraw();
        }
    }

    /// The grid's look changed (hover, feedback): its changed cells
    /// repainted, else the whole launcher.
    pub(super) fn grid_redraw(&mut self) {
        if !self.grid_repaint_cells() {
            self.draw_launcher();
        }
    }
}

#[cfg(test)]
mod recent_tests {
    use super::*;

    fn apps(n: usize) -> Vec<crate::launcher::App> {
        (0..n).map(|i| crate::launcher::App { id: format!("app{i:02}.desktop"), name: format!("App {i:02}"), search: String::new(), categories: Vec::new() }).collect()
    }

    fn grid(recent: &[&str], on: bool) -> GridView {
        GridView { recent_ids: recent.iter().map(|s| s.to_string()).collect(), recent_on: on, ..GridView::default() }
    }

    #[test]
    fn the_row_and_what_pages_hold() {
        let model = Launcher::new(apps(30));
        let g = grid(&["app03.desktop", "gone.desktop", "app01.desktop", "app03.desktop"], true);
        let (recent, per) = recent_layout(&g, &model, 4, 3);
        assert_eq!(recent, ["app03.desktop", "app01.desktop"], "latest first, installed only, once");
        assert_eq!(per, PerPage { first: 8, rest: 12 }, "the first page a row less");
        // Turned off (or usage not remembered: no recent apps), or nothing
        // launched yet: no row, full pages.
        assert_eq!(recent_layout(&grid(&["app03.desktop"], false), &model, 4, 3), (Vec::new(), PerPage::uniform(12)));
        assert_eq!(recent_layout(&grid(&[], true), &model, 4, 3), (Vec::new(), PerPage::uniform(12)));
        assert_eq!(recent_layout(&grid(&["gone.desktop"], true), &model, 4, 3).0, Vec::<String>::new());
        // One row's worth at most.
        let many: Vec<String> = (0..12).map(|i| format!("app{i:02}.desktop")).collect();
        let g = GridView { recent_ids: many, recent_on: true, ..GridView::default() };
        assert_eq!(recent_layout(&g, &model, 5, 3).0.len(), 5);
        // Two rows a page: the first keeps one.
        assert_eq!(recent_layout(&grid(&["app01.desktop"], true), &model, 4, 2).1, PerPage { first: 4, rest: 8 });
    }

    #[test]
    fn pages_hold_every_app_once_with_the_row() {
        let model = Launcher::new(apps(30));
        let mut g = grid(&["app05.desktop", "app09.desktop"], true);
        let (recent, per) = recent_layout(&g, &model, 4, 3);
        Ui::grid_refresh(&mut g, &model, 4, per, recent);
        assert_eq!(g.pages.iter().map(Vec::len).collect::<Vec<_>>(), [8, 12, 10]);
        let mut all: Vec<&String> = g.pages.iter().flatten().collect();
        all.sort();
        all.dedup();
        assert_eq!(all.len(), 30, "every app exactly once");
        // The row on the first page only; the selection counts it first.
        assert_eq!(g.row_keys(), ["recent:app05.desktop", "recent:app09.desktop"]);
        assert_eq!(g.shown().len(), 10);
        assert_eq!(item_id(&g.shown()[1]), "app09.desktop");
        assert!(is_recent(&g.shown()[0]) && !is_recent(&g.shown()[2]));
        g.page = 1;
        assert!(g.row_keys().is_empty(), "not on other pages");
        assert_eq!(g.shown().len(), 12);
        // The row goes away (cleared): full pages again.
        g.page = 0;
        g.recent_ids.clear();
        let (recent, per) = recent_layout(&g, &model, 4, 3);
        Ui::grid_refresh(&mut g, &model, 4, per, recent);
        assert_eq!(g.pages.iter().flatten().count(), 30);
        assert!(g.row_keys().is_empty());
    }

    #[test]
    fn arrows_across_the_row() {
        // Three recent apps over a grid of 4 columns holding 10 items:
        // indices 0–2 the row, 3–12 the grid.
        let (row, count, cols) = (3, 10, 4);
        let go = |at, key| arrow(Some(at), key, row, count, cols);
        assert_eq!(arrow(None, Keysym::Down, row, count, cols), Nav::To(0), "nothing selected: the first app");
        assert_eq!(go(1, Keysym::Down), Nav::To(4), "down from the row: the grid's first line, same column");
        assert_eq!(go(2, Keysym::Right), Nav::To(3), "right from the row's end: on into the grid");
        assert_eq!(go(3, Keysym::Left), Nav::To(2), "and back");
        assert_eq!(go(5, Keysym::Up), Nav::To(2), "up from the grid's first line: the row, same column");
        assert_eq!(go(6, Keysym::Up), Nav::To(2), "a column the row does not reach: its last app");
        assert_eq!(go(8, Keysym::Up), Nav::To(4), "up within the grid");
        assert_eq!(go(4, Keysym::Down), Nav::To(8));
        assert_eq!(go(1, Keysym::Up), Nav::To(1), "up from the row stays");
        assert_eq!(go(0, Keysym::Left), Nav::PrevPage);
        assert_eq!(go(12, Keysym::Right), Nav::NextPage);
        assert_eq!(go(11, Keysym::Down), Nav::To(12), "down past the end: the last");
        // Without the row: as before.
        assert_eq!(arrow(Some(1), Keysym::Up, 0, 10, 4), Nav::To(0));
        assert_eq!(arrow(Some(1), Keysym::Down, 0, 10, 4), Nav::To(5));
        // A row and an empty page.
        assert_eq!(arrow(Some(1), Keysym::Down, 2, 0, 4), Nav::To(1));
    }

    #[test]
    fn tab_order() {
        assert_eq!(tab(None, 3, 10, true), Some(0), "search → the row");
        assert_eq!(tab(Some(1), 3, 10, true), Some(3), "the row → the grid");
        assert_eq!(tab(Some(7), 3, 10, true), None, "the grid → search");
        assert_eq!(tab(None, 3, 10, false), Some(3), "back: search → the grid");
        assert_eq!(tab(Some(4), 3, 10, false), Some(0));
        assert_eq!(tab(Some(0), 3, 10, false), None);
        assert_eq!(tab(None, 0, 10, true), Some(0), "no row: search → the grid");
        assert_eq!(tab(Some(0), 0, 10, true), None);
    }

    #[test]
    fn no_drop_on_the_row() {
        let model = Launcher::new(apps(6));
        let mut g = grid(&["app01.desktop"], true);
        let r = |x, y| Rect { x, y, w: 100, h: 100 };
        g.recent_area = Some(r(0, 0));
        g.cells = vec![(r(0, 30), "recent:app01.desktop".into()), (r(0, 140), "app00.desktop".into()), (r(100, 140), "app01.desktop".into())];
        g.row = 1;
        g.area = Rect { x: 0, y: 140, w: 400, h: 200 };
        g.pages = vec![vec!["app00.desktop".into(), "app01.desktop".into()]];
        let drag = |at| Drag { id: "app00.desktop".into(), at, from_folder: None };
        assert_eq!(drop_at(&g, &model, &drag((50.0, 50.0))), None, "not onto the row");
        assert_eq!(drop_at(&g, &model, &drag((195.0, 150.0))), Some(Drop::Insert { page: 0, index: 2 }), "grid indices leave the row out");
        assert_eq!(drop_at(&g, &model, &drag((150.0, 180.0))), Some(Drop::Combine("app01.desktop".into())));
    }
}

/// The launcher's first page with the "Recent" row (`grid-recent-…`):
/// four recent apps, the second with the keyboard focus, the third under
/// the pointer; in every look, and mirrored (`grid-recent-rtl`). See
/// `ui/gallery.rs` for running it.
#[cfg(test)]
mod recent_gallery {
    use super::super::gallery::{dir, in_looks, render, DARK};
    use super::*;

    #[test]
    #[ignore]
    fn grid_recent() {
        let Some(dir) = dir() else { return };
        let (text, bold) = (Text::load(), Text::load_matching("sans-serif:bold"));
        let ids = [
            ("org.gnome.Nautilus", "Files"),
            ("org.gnome.TextEditor", "Text Editor"),
            ("org.gnome.Calculator", "Calculator"),
            ("org.gnome.Settings", "Settings"),
            ("org.gnome.Calendar", "Calendar"),
            ("org.gnome.Characters", "Characters"),
            ("org.gnome.clocks", "Clocks"),
            ("org.gnome.Console", "Console"),
            ("org.gnome.Contacts", "Contacts"),
            ("org.gnome.DiskUtility", "Disks"),
            ("org.gnome.Logs", "Logs"),
            ("org.gnome.Maps", "Maps"),
            ("org.gnome.Software", "Software"),
            ("org.gnome.SystemMonitor", "System Monitor"),
            ("org.gnome.Weather", "Weather"),
            ("org.gnome.Loupe", "Image Viewer"),
            ("org.gnome.baobab", "Disk Usage Analyzer"),
            ("org.gnome.font-viewer", "Fonts"),
            ("org.gnome.Snapshot", "Camera"),
            ("firefox", "Firefox"),
            ("org.gnome.Papers", "Document Viewer"),
            ("org.gnome.Totem", "Videos"),
        ];
        let apps: Vec<crate::launcher::App> = ids.iter().map(|(id, name)| crate::launcher::App { id: format!("{id}.desktop"), name: (*name).into(), search: String::new(), categories: Vec::new() }).collect();
        let model = Launcher::new(apps);
        let recent: Vec<String> = ["org.gnome.TextEditor", "firefox", "org.gnome.Console", "org.gnome.Settings"].iter().map(|id| format!("{id}.desktop")).collect();
        let mut icons = Icons::default();
        let mut paint = |canvas: &mut Canvas, (w, h): (usize, usize)| {
            canvas.fill(SCRIM);
            let bw = helix_draw::theme::grow(SEARCH_W).min(w as i32 - 48);
            let by = (h as i32 / 6).max(panel_height() as i32 + 24);
            let field = Rect { x: (w as i32 - bw) / 2, y: by, w: bw, h: helix_draw::theme::grow(SEARCH_H) };
            search_field(canvas, text.as_ref(), field, "", tr("Type to search…"), false);
            let mut grid = GridView::new(helix_draw::motion::Motion::None);
            grid.recent_ids = recent.clone();
            grid.recent_on = true;
            grid.selected = Some(1);
            grid.feedback.set_hover(Some(recent_key("org.gnome.Console.desktop")), std::time::Instant::now());
            let mut p = Paint { cache: &mut icons, text: text.as_ref(), bold: bold.as_ref(), scale: canvas.scale };
            paint_grid(&mut p, &mut grid, &model, canvas, (w as i32, h as i32), by + field.h + 24);
        };
        in_looks(&dir, "grid-recent", (1280, 800), &mut paint);
        helix_draw::set_rtl(true);
        render(&dir, "grid-recent-rtl", DARK, (1280, 800, 1.0), |canvas| paint(canvas, (1280, 800)));
        helix_draw::set_rtl(false);
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    #[test]
    fn cell_pixels_and_copies() {
        let r = Rect { x: 10, y: 4, w: 20, h: 10 };
        assert_eq!(cell_pixels(r, 1.0, (100, 50), false), (9, 3, 31, 15));
        assert_eq!(cell_pixels(r, 1.5, (150, 75), false), (14, 5, 46, 22));
        assert_eq!(cell_pixels(r, 1.0, (100, 50), true), (69, 3, 91, 15), "mirrored");
        assert_eq!(cell_pixels(Rect { x: 95, y: 45, w: 20, h: 20 }, 1.0, (100, 50), false), (94, 44, 100, 50), "kept on the buffer");
        let from = vec![7u8; 10 * 4 * 4];
        let mut to = vec![0u8; 10 * 4 * 4];
        copy_pixels(&from, &mut to, 10, (2, 1, 5, 3));
        let px = |x: usize, y: usize| to[(y * 10 + x) * 4];
        assert_eq!((px(2, 1), px(4, 2), px(5, 2), px(1, 1), px(2, 3)), (7, 7, 0, 0, 0));
    }

    /// Hovering a "Recent" row app repaints that cell alone, not the same
    /// app's cell in the grid.
    #[test]
    fn recent_row_hover_repaints_one_cell() {
        let mut feedback: Feedback<String> = Feedback::with_motion(helix_draw::motion::Motion::None);
        let now = std::time::Instant::now();
        let rest = feedback.look(&"x".to_owned(), now);
        let r = Rect { x: 0, y: 0, w: 10, h: 10 };
        let cells = vec![(r, recent_key("a.desktop"), rest, false), (r, "a.desktop".to_owned(), rest, false), (r, "b.desktop".to_owned(), rest, false)];
        assert!(dirty_cells(&cells, &feedback, None, now).is_empty());
        feedback.set_hover(Some(recent_key("a.desktop")), now);
        assert_eq!(dirty_cells(&cells, &feedback, None, now), [0]);
        assert_eq!(dirty_cells(&cells, &feedback, Some("a.desktop"), now), [0, 1], "the focus ring is the grid cell's");
    }

    /// A hover change at 1920×1080 costs a copy of the frame and two
    /// cells, far under a frame; the whole grid takes much longer.
    /// `cargo test … grid_paint_time -- --ignored --nocapture` prints both.
    #[test]
    #[ignore]
    fn grid_paint_time() {
        let text = crate::text::Text::load();
        let (w, h) = (1920usize, 1080usize);
        let apps: Vec<crate::launcher::App> = crate::launcher::installed();
        let model = Launcher::new(apps.clone());
        let mut cache = crate::icon_cache::IconCache::default();
        let mut data = vec![0u8; w * h * 4];
        // Icons decoded first (as they are by the time a user hovers).
        for app in apps.iter().take(40) {
            let _ = grid_icon(&mut cache, &app.id, 64);
        }
        let cells: Vec<(Rect, String)> = apps.iter().take(40).enumerate().map(|(i, a)| (Rect { x: 96 + (i as i32 % 8) * cell_w(), y: 160 + (i as i32 / 8) * cell_h(), w: cell_w(), h: cell_h() }, a.id.clone())).collect();
        let rest = crate::item_feedback::Look { highlight: 0.0, pressed: false, scale: 1.0 };
        let lit = crate::item_feedback::Look { highlight: 1.0, pressed: false, scale: 1.08 };
        let t = std::time::Instant::now();
        {
            let mut canvas = Canvas::scaled(&mut data, w, h, 1.0);
            canvas.fill(SCRIM);
            let mut p = Paint { cache: &mut cache, text: text.as_ref(), bold: None, scale: 1.0 };
            for (r, id) in &cells {
                paint_cell(&mut p, &model, &mut canvas, *r, id, rest, false);
            }
        }
        let full = t.elapsed();
        let base = data.clone();
        let frame = data.clone();
        let t = std::time::Instant::now();
        let mut out = vec![0u8; w * h * 4];
        out.copy_from_slice(&frame);
        {
            let mut canvas = Canvas::scaled(&mut out, w, h, 1.0);
            let mut p = Paint { cache: &mut cache, text: text.as_ref(), bold: None, scale: 1.0 };
            for (i, look) in [(3, rest), (4, lit)] {
                let (r, id) = &cells[i];
                let px = cell_pixels(*r, 1.0, (w, h), false);
                copy_pixels(&base, canvas.data, w, px);
                paint_cell(&mut p, &model, &mut canvas, *r, id, look, false);
            }
        }
        let hover = t.elapsed();
        println!("whole grid: {full:?}; a hover change: {hover:?}");
        assert!(hover < full, "{hover:?} vs {full:?}");
    }
}
