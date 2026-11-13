//! The app grid (helix-desktop-spec `contracts/wayland/README.md`, "App
//! grid"): the launcher with an empty search, as pages of icons that can
//! be rearranged and put into folders by dragging.

use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerEventKind, BTN_LEFT};

use super::*;
use crate::app_grid::{self as layout, Pages};

const CELL_W: i32 = 120;
const CELL_H: i32 = 118;
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
    /// The selected item of the page (or of the open folder).
    selected: Option<usize>,
    cols: usize,
    per_page: usize,
    press: Option<(String, (f64, f64), Option<String>)>,
    drag: Option<Drag>,
    /// What the last paint showed.
    cells: Vec<(Rect, String)>,
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
}

/// How long a dragged icon is held at the side before the page turns,
/// and then between turns.
const EDGE_WAIT: std::time::Duration = std::time::Duration::from_millis(700);
const EDGE_REPEAT: std::time::Duration = std::time::Duration::from_millis(900);

impl GridView {
    fn page_items(&self) -> &[String] {
        self.pages.get(self.page).map_or(&[], Vec::as_slice)
    }
}

/// Where `drag` would land now.
fn drop_at(view: &LauncherView, drag: &Drag) -> Option<Drop> {
    let g = &view.grid;
    let (x, y) = drag.at;
    if let Some(b) = g.folder_box {
        if drag.from_folder.is_some() && b.contains(x, y) {
            return Some(Drop::Stay);
        }
    }
    if let Some(&(_, page)) = g.dots.iter().find(|(r, _)| r.contains(x, y)) {
        return Some(Drop::Insert { page, index: usize::MAX });
    }
    let is_folder = view.model.folders.iter().any(|f| f.id == drag.id);
    for (i, (cell, id)) in g.cells.iter().enumerate() {
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

    /// Lay the grid out again (opened, folders or apps changed, size).
    fn grid_refresh(view: &mut LauncherView, cols: usize, rows: usize) {
        let per_page = (cols * rows).max(1);
        let saved = if view.grid.pages.is_empty() { layout::read() } else { view.grid.pages.clone() };
        view.grid.pages = layout::arrange(&saved, &grid_items(&view.model), per_page);
        view.grid.cols = cols;
        view.grid.per_page = per_page;
        view.grid.page = view.grid.page.min(view.grid.pages.len().saturating_sub(1));
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

    fn grid_open(&mut self, id: &str) {
        let Some(view) = self.launcher.as_mut() else { return };
        if let Some(i) = view.model.folders.iter().position(|f| f.id == id) {
            view.model.enter_folder(i);
            view.grid.selected = None;
            view.grid.folder_page = 0;
            self.draw_launcher();
            return;
        }
        tracing::info!(app = id, "launching from the app grid");
        if let Err(err) = crate::launcher::launch(id) {
            tracing::warn!(%err, app = id, "launch failed");
        }
        self.close_launcher(true);
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
                    view.grid.press = Some((id.clone(), (x, y), if in_folder { from_folder } else { None }));
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
            PointerEventKind::Motion { .. } => {
                let threshold = crate::drag_threshold();
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
                        view.grid.drag = Some(Drag { id, at: (x, y), from_folder: from });
                        self.draw_launcher();
                    }
                }
            }
            PointerEventKind::Release { button: BTN_LEFT, .. } => {
                let Some(view) = self.launcher.as_mut() else { return true };
                if let Some(drag) = view.grid.drag.take() {
                    self.grid_edge(None);
                    let drop = self.launcher.as_ref().and_then(|v| drop_at(v, &drag));
                    tracing::info!(id = drag.id, ?drop, "app grid drop");
                    match drop {
                        Some(drop) => self.grid_apply(drag, drop),
                        None => self.draw_launcher(),
                    }
                } else if let Some((id, _, _)) = view.grid.press.take() {
                    self.grid_open(&id);
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
        let (count, cols) = match &in_folder {
            Some(apps) => (apps.len(), apps.len().clamp(1, 6)),
            None => (view.grid.page_items().len(), view.grid.cols.max(1)),
        };
        let at = view.grid.selected;
        let pages = view.grid.pages.len();
        match keysym {
            Keysym::Right | Keysym::Left | Keysym::Up | Keysym::Down if count > 0 => {
                let step: isize = match keysym {
                    Keysym::Right => 1,
                    Keysym::Left => -1,
                    Keysym::Up => -(cols as isize),
                    _ => cols as isize,
                };
                let next = at.map_or(0, |i| i as isize + step);
                if next >= count as isize && in_folder.is_none() && keysym == Keysym::Right && view.grid.page + 1 < pages {
                    view.grid.page += 1;
                    view.grid.selected = Some(0);
                } else if next < 0 && in_folder.is_none() && keysym == Keysym::Left && view.grid.page > 0 {
                    view.grid.page -= 1;
                    view.grid.selected = Some(view.grid.page_items().len().saturating_sub(1));
                } else {
                    view.grid.selected = Some(next.clamp(0, count as isize - 1) as usize);
                }
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
                    (None, Some(i)) => view.grid.page_items().get(i).cloned(),
                    _ => None,
                };
                if let Some(id) = id {
                    self.grid_open(&id);
                }
                return true;
            }
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

pub(super) type Icons = HashMap<String, Option<helix_draw::Image>>;

fn grid_icon(cache: &mut Icons, id: &str, px: u32) -> Option<helix_draw::Image> {
    let app = id.strip_suffix(".desktop").unwrap_or(id).to_owned();
    let key = format!("app:{app}@{px}");
    cache.entry(key).or_insert_with(|| crate::icons::IconSource::for_app(&app).and_then(|s| crate::icons::load(&s, px))).clone()
}

/// Draw one grid cell (an app or a folder) at `cell`.
/// What grid painting needs besides the canvas.
pub(super) struct Paint<'a> {
    pub cache: &'a mut Icons,
    pub text: Option<&'a Text>,
    pub scale: f64,
}

fn paint_cell(p: &mut Paint, model: &Launcher, canvas: &mut Canvas, cell: Rect, id: &str, selected: bool) {
    let (text, scale) = (p.text, p.scale);
    let folder = model.folders.iter().find(|f| f.id == id).cloned();
    let name = Ui::item_name(model, id);
    if selected {
        canvas.round_rect(f64::from(cell.x + 4), f64::from(cell.y + 2), f64::from(cell.w - 8), f64::from(cell.h - 4), 10.0, 0x40FFFFFF);
    }
    let (ix, iy) = (cell.x + (cell.w - ICON) / 2, cell.y + 10);
    match folder {
        Some(folder) => {
            canvas.round_rect(f64::from(ix), f64::from(iy), f64::from(ICON), f64::from(ICON), 12.0, 0x50FFFFFF);
            let px = (f64::from(MINI) * scale).round() as u32;
            for (k, app) in folder.apps.iter().take(4).enumerate() {
                let (mx, my) = (ix + 5 + (k % 2) as i32 * (MINI + 2), iy + 5 + (k / 2) as i32 * (MINI + 2));
                if let Some(icon) = grid_icon(p.cache, app, px) {
                    canvas.image(f64::from(mx), f64::from(my), f64::from(MINI), f64::from(MINI), &icon);
                }
            }
        }
        None => {
            let px = (f64::from(ICON) * scale).round() as u32;
            match grid_icon(p.cache, id, px) {
                Some(icon) => canvas.image(f64::from(ix), f64::from(iy), f64::from(ICON), f64::from(ICON), &icon),
                None => canvas.round_rect(f64::from(ix + 6), f64::from(iy + 6), f64::from(ICON - 12), f64::from(ICON - 12), 10.0, 0xFF5E5C64),
            }
        }
    }
    let tw = text.map_or(0.0, |t| t.width(&name, 13.0)).min((cell.w - 12) as f32);
    label(text, canvas, &name, cell.x as f32 + (cell.w as f32 - tw) / 2.0, (iy + ICON + 22) as f32, 13.0, FG, (cell.w - 12) as f32);
}

/// Paint the grid below the search field (from `top`).
pub(super) fn paint_grid(p: &mut Paint, view: &mut LauncherView, canvas: &mut Canvas, w: i32, h: i32, top: i32) {
    let (text, scale) = (p.text, p.scale);
    let cols = ((w - 96) / CELL_W).clamp(3, 8) as usize;
    let rows = ((h - top - 72) / CELL_H).clamp(2, 5) as usize;
    if view.grid.cols != cols || view.grid.per_page != cols * rows {
        Ui::grid_refresh(view, cols, rows);
    }
    let gw = cols as i32 * CELL_W;
    let gx = (w - gw) / 2;
    let items: Vec<String> = view.grid.page_items().to_vec();
    let (selected, drag, open_folder) = (view.grid.selected, view.grid.drag.clone(), view.model.open_folder);
    let pages = view.grid.pages.len();
    let current = view.grid.page;
    let target = drag.as_ref().and_then(|d| drop_at(view, d));
    let mut cells = Vec::new();
    for (i, id) in items.iter().enumerate() {
        let cell = Rect { x: gx + (i % cols) as i32 * CELL_W, y: top + (i / cols) as i32 * CELL_H, w: CELL_W, h: CELL_H };
        cells.push((cell, id.clone()));
        if drag.as_ref().is_some_and(|d| d.id == *id && d.from_folder.is_none()) {
            continue;
        }
        let hot = matches!(&target, Some(Drop::Combine(t)) if t == id);
        paint_cell(p, &view.model, canvas, cell, id, (selected == Some(i) && open_folder.is_none()) || hot);
    }
    // Where a dragged item would go.
    if let Some(Drop::Insert { page, index }) = &target {
        if *page == current {
            let i = (*index).min(items.len());
            let (cx, cy) = (gx + (i % cols) as i32 * CELL_W, top + (i / cols) as i32 * CELL_H);
            canvas.rect(cx - 2, cy + 8, 3, CELL_H - 16, ACCENT);
        }
    }
    // Page dots.
    let dots_y = top + rows as i32 * CELL_H + 18;
    let dots_x = (w - pages as i32 * (DOT + 10)) / 2;
    let mut dots = Vec::new();
    for p in 0..pages {
        let r = Rect { x: dots_x + p as i32 * (DOT + 10) - 5, y: dots_y - 10, w: DOT + 10, h: DOT + 20 };
        let color = if p == current { FG } else { 0x80FFFFFF };
        canvas.circle(f64::from(r.x + 5 + DOT / 2), f64::from(dots_y), f64::from(DOT) / 2.0, color);
        dots.push((r, p));
    }
    let area = Rect { x: gx, y: top, w: gw, h: rows as i32 * CELL_H };
    // An open folder: a box over the dimmed grid.
    let (mut folder_box, mut folder_title, mut folder_cells) = (None, None, Vec::new());
    if let Some(folder) = open_folder.and_then(|i| view.model.folders.get(i).cloned()) {
        canvas.rect(0, top - 8, w, h - top + 8, 0x80000000);
        let fcols = folder.apps.len().clamp(1, 6) as i32;
        let n = folder.apps.len() as i32;
        let rows_fit = ((h - top - 16 - 52 - 40) / CELL_H).max(1);
        let frows = ((n + fcols - 1) / fcols).min(rows_fit);
        let per = (fcols * rows_fit).max(1) as usize;
        let fpages = folder.apps.len().div_ceil(per).max(1);
        if let Some(i) = selected {
            view.grid.folder_page = i / per;
        }
        let fpage = view.grid.folder_page.min(fpages - 1);
        view.grid.folder_page = fpage;
        view.grid.folder_pages = fpages;
        let dots_h = if fpages > 1 { 30 } else { 0 };
        let bw = fcols * CELL_W + 32;
        let bh = frows * CELL_H + 72 + dots_h;
        let b = Rect { x: (w - bw) / 2, y: top + 8, w: bw, h: bh };
        canvas.round_rect(f64::from(b.x), f64::from(b.y), f64::from(b.w), f64::from(b.h), 14.0, BOX_BG);
        let tw = text.map_or(0.0, |t| t.width(&folder.name, 17.0));
        let title = Rect { x: b.x + (b.w - tw as i32) / 2 - 8, y: b.y + 10, w: tw as i32 + 16, h: 34 };
        label(text, canvas, &folder.name, (title.x + 8) as f32, baseline(text, title.y, title.h, 17.0), 17.0, FG, (b.w - 32) as f32);
        for (k, id) in folder.apps.iter().enumerate().skip(fpage * per).take(per) {
            let i = k - fpage * per;
            let cell = Rect { x: b.x + 16 + (i as i32 % fcols) * CELL_W, y: b.y + 52 + (i as i32 / fcols) * CELL_H, w: CELL_W, h: CELL_H };
            folder_cells.push((cell, id.clone()));
            if drag.as_ref().is_some_and(|d| d.id == *id) {
                continue;
            }
            paint_cell(p, &view.model, canvas, cell, id, selected == Some(k));
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
        view.grid.folder_dots = fdots;
        folder_box = Some(b);
        folder_title = Some(title);
    }
    // The dragged item under the pointer.
    if let Some(d) = &drag {
        let px = (f64::from(ICON) * scale).round() as u32;
        let is_folder = view.model.folders.iter().any(|f| f.id == d.id);
        if is_folder {
            canvas.round_rect(d.at.0 - f64::from(ICON) / 2.0, d.at.1 - f64::from(ICON) / 2.0, f64::from(ICON), f64::from(ICON), 12.0, 0x80FFFFFF);
        } else if let Some(icon) = grid_icon(p.cache, &d.id, px) {
            canvas.image(d.at.0 - f64::from(ICON) / 2.0, d.at.1 - f64::from(ICON) / 2.0, f64::from(ICON), f64::from(ICON), &icon);
        }
    }
    view.grid.cells = cells;
    view.grid.dots = dots;
    view.grid.area = area;
    view.grid.folder_box = folder_box;
    view.grid.folder_title = folder_title;
    view.grid.folder_cells = folder_cells;
}
