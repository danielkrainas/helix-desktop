//! The overview (helix-desktop-spec `contracts/wayland/README.md`,
//! "Overview"): a search field, workspace thumbnails and the active
//! workspace's windows as pictures, on the primary monitor.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::AsFd;

use helix_draw::Image;
use helix_protocols::client::helix_screenshot_frame_v1::{self, HelixScreenshotFrameV1};
use helix_protocols::client::helix_window_list_v1::Action as WindowAction;
use smithay_client_toolkit::reexports::client::protocol::wl_shm;
use smithay_client_toolkit::reexports::client::{Connection, Dispatch, Proxy, QueueHandle};
use smithay_client_toolkit::seat::keyboard::{KeyEvent, Keysym};
use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerEventKind, BTN_LEFT};
use smithay_client_toolkit::shell::wlr_layer::{Anchor, KeyboardInteractivity, Layer, LayerSurface};
use smithay_client_toolkit::shell::WaylandSurface;

use super::{baseline, label, physical, Canvas, Overlay, Rect, Ui, ACCENT, BANNER_BORDER, DIM_FG, FG, FIELD_BG, SCRIM};
use crate::UiRequest;

/// Workspace thumbnail height (logical pixels).
const THUMB_H: i32 = 90;
/// Room under each window picture for its title.
const TITLE_H: i32 = 28;
const GAP: i32 = 24;
/// Pointer travel before a press becomes a drag.
const DRAG_START: f64 = 8.0;
/// The window list's "on every workspace".
const ALL_WORKSPACES: u32 = u32::MAX;

/// A window's place, from the compositor: (x, y, w, h, minimized).
pub(super) type Geometry = (i32, i32, i32, i32, bool);

#[derive(Default)]
pub(super) struct OverviewState {
    /// On the primary monitor (with the search field and workspaces).
    primary: bool,
    /// The monitor's area in layout coordinates (other monitors).
    area: (i32, i32, i32, i32),
    pub(super) selected: Option<u32>,
    hover: Option<u32>,
    /// A press on a window picture, where.
    press: Option<(u32, (f64, f64))>,
    /// A window being dragged, and the pointer.
    drag: Option<(u32, (f64, f64))>,
    tiles: Vec<(Rect, u32)>,
    closes: Vec<(Rect, u32)>,
    workspaces: Vec<(Rect, u32)>,
    search: Rect,
}

/// Pictures of windows for the overview, shared by its monitors.
#[derive(Default)]
pub(super) struct Shots {
    pictures: HashMap<u32, Image>,
    /// Asked for and not answered yet.
    requested: HashSet<u32>,
}

/// How often pictures are renewed while the overview is open.
const LIVE: std::time::Duration = std::time::Duration::from_millis(500);

/// A window picture on its way: the file it is written to.
pub(super) struct ShotData {
    window: u32,
    file: File,
}

/// Rows of tiles for windows of `sizes` in an area of `w`×`h`: each
/// window's rectangle (relative to the area), scaled down (never up) to fit,
/// keeping its proportions, with room for a title below.
pub(super) fn arrange(sizes: &[(i32, i32)], w: i32, h: i32) -> Vec<Rect> {
    let n = sizes.len();
    if n == 0 || w <= 0 || h <= 0 {
        return Vec::new();
    }
    let mut best: Option<(f64, Vec<Rect>)> = None;
    for rows in 1..=n {
        let cols = n.div_ceil(rows);
        let cell_w = (w - GAP * (cols as i32 - 1)) / cols as i32;
        let cell_h = (h - GAP * (rows as i32 - 1)) / rows as i32 - TITLE_H;
        if cell_w <= 8 || cell_h <= 8 {
            continue;
        }
        let mut rects = Vec::with_capacity(n);
        let mut worst = f64::MAX;
        for (i, (sw, sh)) in sizes.iter().enumerate() {
            let (sw, sh) = (f64::from((*sw).max(1)), f64::from((*sh).max(1)));
            let scale = (f64::from(cell_w) / sw).min(f64::from(cell_h) / sh).min(1.0);
            worst = worst.min(scale);
            let (tw, th) = ((sw * scale) as i32, (sh * scale) as i32);
            let (row, col) = (i / cols, i % cols);
            // The last row is centered when it is not full.
            let in_row = if row == n / cols && !n.is_multiple_of(cols) { n % cols } else { cols };
            let row_w = in_row as i32 * cell_w + (in_row as i32 - 1) * GAP;
            let x0 = (w - row_w) / 2 + col as i32 * (cell_w + GAP);
            let y0 = row as i32 * (cell_h + TITLE_H + GAP);
            rects.push(Rect { x: x0 + (cell_w - tw) / 2, y: y0 + (cell_h - th) / 2, w: tw, h: th });
        }
        if best.as_ref().is_none_or(|(s, _)| worst > *s) {
            best = Some((worst, rects));
        }
    }
    best.map(|(_, r)| r).unwrap_or_default()
}

impl Ui {
    /// Windows the overview shows: the active workspace's (and those on
    /// every workspace), in a stable order.
    fn overview_windows(&self) -> Vec<u32> {
        let mut ids: Vec<u32> = self
            .windows
            .mru()
            .iter()
            .filter(|e| e.workspace == self.windows.active || e.workspace == ALL_WORKSPACES)
            .map(|e| e.id)
            .collect();
        ids.sort_unstable();
        ids
    }

    /// The monitors other than the primary one: (output, area).
    fn other_monitors(&self) -> Vec<(smithay_client_toolkit::reexports::client::protocol::wl_output::WlOutput, (i32, i32, i32, i32))> {
        self.output_state
            .outputs()
            .filter_map(|o| {
                let info = self.output_state.info(&o)?;
                let ((x, y), (w, h)) = (info.logical_position?, info.logical_size?);
                Some((o, (x, y, w, h)))
            })
            .filter(|(_, area)| *area != self.primary_area)
            .collect()
    }

    /// The overview windows shown on the monitor of `area` (the primary
    /// one also gets windows on no known monitor).
    fn windows_on(&self, area: (i32, i32, i32, i32), primary: bool) -> Vec<u32> {
        let others: Vec<(i32, i32, i32, i32)> = self.other_monitors().into_iter().map(|(_, a)| a).collect();
        let inside = |a: &(i32, i32, i32, i32), (x, y): (i32, i32)| x >= a.0 && y >= a.1 && x < a.0 + a.2 && y < a.1 + a.3;
        self.overview_windows()
            .into_iter()
            .filter(|id| {
                let Some(g) = self.window_geometry.get(id) else { return primary };
                let center = (g.0 + g.2 / 2, g.1 + g.3 / 2);
                if primary {
                    !others.iter().any(|a| inside(a, center))
                } else {
                    inside(&area, center)
                }
            })
            .collect()
    }

    pub(super) fn overview_shown(&self) -> bool {
        self.overview.is_some()
    }

    /// Show the overview (`notify`: tell the D-Bus side, which did not ask).
    pub(super) fn open_overview(&mut self, notify: bool) {
        if self.overview.is_some() || self.lock.session.is_some() {
            return;
        }
        let was_shown = self.launcher.is_some();
        self.close_launcher(false);
        let state = OverviewState { primary: true, selected: Some(self.windows.focused()).filter(|id| *id != 0), ..OverviewState::default() };
        let overlay = self.new_overlay("helix-overview", KeyboardInteractivity::Exclusive, state);
        self.overview = Some(overlay);
        // The other monitors show their windows.
        for (output, area) in self.other_monitors() {
            let surface = self.compositor.create_surface(&self.qh);
            let layer = self.layer_shell.create_layer_surface(&self.qh, surface, Layer::Overlay, Some("helix-overview".to_owned()), Some(&output));
            layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
            layer.set_exclusive_zone(-1);
            layer.set_keyboard_interactivity(KeyboardInteractivity::None);
            layer.commit();
            let state = OverviewState { area, ..OverviewState::default() };
            self.overview_others.push(Overlay { layer, width: 0, height: 0, configured: false, hits: Vec::new(), state });
        }
        self.request_shots();
        // Pictures renew while it is open.
        if let Some(handle) = self.loop_handle.clone() {
            let _ = handle.insert_source(smithay_client_toolkit::reexports::calloop::timer::Timer::from_duration(LIVE), |_, _, ui| {
                if ui.overview.is_none() {
                    return smithay_client_toolkit::reexports::calloop::timer::TimeoutAction::Drop;
                }
                ui.renew_shots();
                smithay_client_toolkit::reexports::calloop::timer::TimeoutAction::ToDuration(LIVE)
            });
        }
        if notify && !was_shown {
            let _ = self.requests.send(UiRequest::Overview(true));
        }
        self.draw_panel();
    }

    pub(super) fn close_overview(&mut self, notify: bool) {
        self.overview_others.clear();
        self.overview_shots = Shots::default();
        if self.overview.take().is_some() {
            if notify {
                let _ = self.requests.send(UiRequest::Overview(false));
            }
            self.draw_panel();
        }
    }

    /// Ask for pictures of every listed window not pictured yet.
    pub(super) fn request_shots(&mut self) {
        if self.overview.is_none() {
            return;
        }
        let ids: Vec<u32> = self.windows.mru().iter().map(|e| e.id).filter(|id| !self.overview_shots.pictures.contains_key(id)).collect();
        self.capture_shots(&ids);
    }

    /// Ask for pictures of `ids` (those not already on their way).
    fn capture_shots(&mut self, ids: &[u32]) {
        let Some(manager) = self.screenshots.clone().filter(|m| m.version() >= 5) else { return };
        for &id in ids {
            if !self.overview_shots.requested.insert(id) {
                continue;
            }
            let Some(file) = crate::screenshot::memfd() else { continue };
            let Ok(fd) = file.try_clone() else { continue };
            manager.capture_window_id(id, fd.as_fd(), &self.qh, ShotData { window: id, file });
        }
    }

    /// Renew the pictures of the windows shown (live overview); the old
    /// ones stay until the new ones come.
    fn renew_shots(&mut self) {
        let ids = self.overview_windows();
        self.capture_shots(&ids);
    }

    /// After switching workspace: pictures of the windows now shown.
    pub(super) fn refresh_shots(&mut self) {
        self.renew_shots();
        self.request_shots();
        self.draw_overview();
    }

    pub(super) fn overview_configured(&mut self, layer: &LayerSurface, size: (u32, u32)) -> bool {
        let overview = match self.overview.as_mut().filter(|o| &o.layer == layer) {
            Some(o) => o,
            None => match self.overview_others.iter_mut().find(|o| &o.layer == layer) {
                Some(o) => o,
                None => return false,
            },
        };
        overview.width = size.0;
        overview.height = size.1;
        overview.configured = true;
        self.draw_overview();
        true
    }

    pub(super) fn overview_closed(&mut self, layer: &LayerSurface) -> bool {
        if self.overview.as_ref().is_some_and(|o| &o.layer == layer) {
            self.close_overview(true);
            return true;
        }
        if let Some(i) = self.overview_others.iter().position(|o| &o.layer == layer) {
            // Its monitor went away.
            self.overview_others.remove(i);
            return true;
        }
        false
    }

    pub(super) fn draw_overview(&mut self) {
        let Some(mut overlay) = self.overview.take() else { return };
        if overlay.configured && overlay.width > 0 {
            self.paint_overview(&mut overlay);
        }
        self.overview = Some(overlay);
        let mut others = std::mem::take(&mut self.overview_others);
        for overlay in others.iter_mut().filter(|o| o.configured && o.width > 0) {
            self.paint_overview(overlay);
        }
        self.overview_others = others;
    }

    fn paint_overview(&mut self, overlay: &mut Overlay<OverviewState>) {
        let ids = if overlay.state.primary {
            self.windows_on(self.primary_area, true)
        } else {
            self.windows_on(overlay.state.area, false)
        };
        let (w, h) = (overlay.width as usize, overlay.height as usize);
        let scale = self.scales.of(overlay.layer.wl_surface());
        if !overlay.state.primary && self.overview_pool.is_none() {
            self.overview_pool = smithay_client_toolkit::shm::slot::SlotPool::new(w * h * 4, &self.shm).ok();
        }
        let pool = if overlay.state.primary { &mut self.pool } else if let Some(p) = self.overview_pool.as_mut() { p } else { return };
        let Ok((buffer, data)) = pool.create_buffer(physical(w, scale) as i32, physical(h, scale) as i32, (physical(w, scale) * 4) as i32, wl_shm::Format::Argb8888) else {
            return;
        };
        let mut canvas = Canvas::scaled(data, w, h, scale);
        canvas.fill(SCRIM);
        let text = self.text.as_ref();
        let state = &mut overlay.state;
        let (w, h) = (w as i32, h as i32);

        let primary = state.primary;
        // Search field.
        let field = Rect { x: (w - 400.min(w - 32)) / 2, y: 24, w: 400.min(w - 32), h: 40 };
        state.workspaces.clear();
        let ty = if primary { field.y + field.h + 24 } else { -THUMB_H - 8 };
        if primary {
        canvas.rect(field.x, field.y, field.w, field.h, FIELD_BG);
        label(text, &mut canvas, "Type to search…", (field.x + 16) as f32, baseline(text, field.y, field.h, 15.0), 15.0, DIM_FG, (field.w - 32) as f32);
        state.search = field;

        // Workspace thumbnails: the primary monitor, its windows in place.
        let (px, py, pw, ph) = self.primary_area;
        let (pw, ph) = (pw.max(1), ph.max(1));
        let thumb_w = THUMB_H * pw / ph;
        let count = self.windows.count.max(1) as i32;
        let strip_w = count * thumb_w + (count - 1) * 16;
        let mut tx = (w - strip_w) / 2;
        for n in 0..count as u32 {
            let r = Rect { x: tx, y: ty, w: thumb_w, h: THUMB_H };
            let dropping = state.drag.is_some_and(|(_, (x, y))| r.contains(x, y));
            let border = if n == self.windows.active || dropping { ACCENT } else { BANNER_BORDER };
            canvas.rect(r.x - 2, r.y - 2, r.w + 4, r.h + 4, border);
            canvas.rect(r.x, r.y, r.w, r.h, 0xFF1E1E24);
            let s = f64::from(THUMB_H) / f64::from(ph);
            // Bottom of the stack first.
            for entry in self.windows.mru().iter().rev().filter(|e| e.workspace == n || e.workspace == ALL_WORKSPACES) {
                let Some(&(gx, gy, gw, gh, minimized)) = self.window_geometry.get(&entry.id) else { continue };
                if minimized {
                    continue;
                }
                let x = f64::from(r.x) + f64::from(gx - px) * s;
                let y = f64::from(r.y) + f64::from(gy - py) * s;
                let (ww, wh) = (f64::from(gw) * s, f64::from(gh) * s);
                // Only what lies on the thumbnail.
                let (x0, y0) = (x.max(f64::from(r.x)), y.max(f64::from(r.y)));
                let (x1, y1) = ((x + ww).min(f64::from(r.x + r.w)), (y + wh).min(f64::from(r.y + r.h)));
                if x1 <= x0 || y1 <= y0 {
                    continue;
                }
                match self.overview_shots.pictures.get(&entry.id) {
                    Some(shot) if (x0, y0, x1, y1) == (x, y, x + ww, y + wh) => canvas.image(x, y, ww, wh, shot),
                    _ => canvas.rect(x0 as i32, y0 as i32, (x1 - x0) as i32, (y1 - y0) as i32, 0xFF55555F),
                }
            }
            state.workspaces.push((r, n));
            tx += thumb_w + 16;
        }
        }

        // The active workspace's windows on this monitor.
        let area = Rect { x: 32, y: ty + THUMB_H + 32, w: w - 64, h: h - (ty + THUMB_H + 32) - 24 };
        let sizes: Vec<(i32, i32)> = ids
            .iter()
            .map(|id| match (self.overview_shots.pictures.get(id), self.window_geometry.get(id)) {
                (Some(shot), Some(g)) => (g.2.max(1), g.2.max(1) * shot.height as i32 / shot.width.max(1) as i32),
                (_, Some(g)) => (g.2, g.3),
                _ => (640, 400),
            })
            .collect();
        let rects = arrange(&sizes, area.w, area.h);
        state.tiles.clear();
        state.closes.clear();
        if ids.is_empty() {
            let msg = "No open windows";
            let tw = text.map_or(0.0, |t| t.width(msg, 15.0));
            label(text, &mut canvas, msg, (w as f32 - tw) / 2.0, (area.y + 40) as f32, 15.0, DIM_FG, area.w as f32);
        }
        for (id, rect) in ids.iter().zip(rects) {
            let r = Rect { x: area.x + rect.x, y: area.y + rect.y, w: rect.w, h: rect.h };
            let dragged = state.drag.is_some_and(|(d, _)| d == *id);
            let highlighted = state.selected == Some(*id) || state.hover == Some(*id);
            if highlighted && !dragged {
                canvas.rect(r.x - 4, r.y - 4, r.w + 8, r.h + 8, ACCENT);
            }
            match self.overview_shots.pictures.get(id) {
                Some(shot) if !dragged => canvas.image(f64::from(r.x), f64::from(r.y), f64::from(r.w), f64::from(r.h), shot),
                _ => canvas.rect(r.x, r.y, r.w, r.h, if dragged { 0xFF2A2A30 } else { 0xFF44444C }),
            }
            let title = self.windows.mru().iter().find(|e| e.id == *id).map(|e| e.label().to_owned()).unwrap_or_default();
            let tw = text.map_or(0.0, |t| t.width(&title, 13.0)).min(r.w.max(120) as f32);
            label(text, &mut canvas, &title, r.x as f32 + (r.w as f32 - tw) / 2.0, (r.y + r.h + 20) as f32, 13.0, FG, r.w.max(120) as f32);
            if highlighted && !dragged {
                let c = Rect { x: r.x + r.w - 14, y: r.y - 14, w: 28, h: 28 };
                canvas.rect(c.x, c.y, c.w, c.h, 0xFF2E2E36);
                let xw = text.map_or(0.0, |t| t.width("×", 18.0));
                label(text, &mut canvas, "×", c.x as f32 + (28.0 - xw) / 2.0, baseline(text, c.y, c.h, 18.0), 18.0, FG, 28.0);
                state.closes.push((c, *id));
            }
            state.tiles.push((r, *id));
        }
        // The dragged window follows the pointer.
        if let Some((id, (x, y))) = state.drag {
            if let Some(shot) = self.overview_shots.pictures.get(&id) {
                let dw = 160.0;
                let dh = dw * shot.height as f64 / shot.width.max(1) as f64;
                canvas.image(x - dw / 2.0, y - dh / 2.0, dw, dh, shot);
            }
        }
        let surface = overlay.layer.wl_surface();
        self.scales.prepare(&self.qh, surface, w, h);
        if buffer.attach_to(surface).is_ok() {
            overlay.layer.commit();
        }
    }

    /// Pointer input on the overview; true if it was for it.
    pub(super) fn overview_pointer(&mut self, event: &PointerEvent) -> bool {
        let overview = match self.overview.as_mut().filter(|o| o.layer.wl_surface() == &event.surface) {
            Some(o) => o,
            None => match self.overview_others.iter_mut().find(|o| o.layer.wl_surface() == &event.surface) {
                Some(o) => o,
                None => return false,
            },
        };
        let (x, y) = event.position;
        let state = &mut overview.state;
        let tile_at = |state: &OverviewState| state.tiles.iter().find(|(r, _)| r.contains(x, y)).map(|(_, id)| *id);
        match event.kind {
            PointerEventKind::Motion { .. } => {
                if let Some((id, (sx, sy))) = state.press {
                    if state.drag.is_none() && ((x - sx).powi(2) + (y - sy).powi(2)).sqrt() >= DRAG_START {
                        state.drag = Some((id, (x, y)));
                    }
                }
                if let Some(drag) = state.drag.as_mut() {
                    drag.1 = (x, y);
                    self.draw_overview();
                    return true;
                }
                let hover = tile_at(state).or_else(|| state.closes.iter().find(|(r, _)| r.contains(x, y)).map(|(_, id)| *id));
                if hover != state.hover {
                    state.hover = hover;
                    self.draw_overview();
                }
            }
            PointerEventKind::Press { button: BTN_LEFT, .. } => {
                if let Some(&(_, id)) = state.closes.iter().find(|(r, _)| r.contains(x, y)) {
                    if let Some(list) = self.window_list.as_ref().filter(|l| l.version() >= 4) {
                        list.window_action(id, WindowAction::Close);
                    }
                } else if let Some(id) = tile_at(state) {
                    state.press = Some((id, (x, y)));
                } else if let Some(&(_, n)) = state.workspaces.iter().find(|(r, _)| r.contains(x, y)) {
                    if let Some(list) = self.window_list.as_ref().filter(|l| l.version() >= 2) {
                        list.switch_workspace(n);
                    }
                } else if state.search.contains(x, y) {
                    self.close_overview(false);
                    self.open_launcher(false);
                } else {
                    self.close_overview(true);
                }
            }
            PointerEventKind::Release { button: BTN_LEFT, .. } => {
                let press = state.press.take();
                if let Some((id, _)) = state.drag.take() {
                    let target = state.workspaces.iter().find(|(r, _)| r.contains(x, y)).map(|(_, n)| *n);
                    if let (Some(n), Some(list)) = (target, self.window_list.as_ref().filter(|l| l.version() >= 2)) {
                        list.move_window(id, n);
                    }
                    self.draw_overview();
                } else if let Some((id, _)) = press.filter(|(id, _)| tile_at(state) == Some(*id)) {
                    self.close_overview(true);
                    self.activate_window(id);
                }
            }
            _ => {}
        }
        true
    }

    pub(super) fn overview_key(&mut self, event: &KeyEvent) {
        let ids = self.overview_windows();
        let Some(overview) = self.overview.as_mut() else { return };
        let state = &mut overview.state;
        let pos = state.selected.and_then(|s| ids.iter().position(|id| *id == s));
        match event.keysym {
            Keysym::Escape => self.close_overview(true),
            Keysym::Return | Keysym::KP_Enter => {
                if let Some(id) = state.selected.filter(|s| ids.contains(s)) {
                    self.close_overview(true);
                    self.activate_window(id);
                }
            }
            Keysym::Left | Keysym::Up | Keysym::Right | Keysym::Down | Keysym::Tab => {
                if ids.is_empty() {
                    return;
                }
                let back = matches!(event.keysym, Keysym::Left | Keysym::Up);
                let next = match pos {
                    None => 0,
                    Some(p) if back => (p + ids.len() - 1) % ids.len(),
                    Some(p) => (p + 1) % ids.len(),
                };
                state.selected = Some(ids[next]);
                self.draw_overview();
            }
            _ => {
                // Typing searches, in the launcher.
                let typed = event.utf8.clone().filter(|t| !t.is_empty() && !t.chars().any(char::is_control));
                if let Some(text) = typed {
                    self.close_overview(false);
                    self.open_launcher(false);
                    if let Some(view) = self.launcher.as_mut() {
                        view.model.type_text(&text);
                    }
                    self.draw_launcher();
                }
            }
        }
    }
}

impl Dispatch<HelixScreenshotFrameV1, ShotData> for Ui {
    fn event(ui: &mut Self, frame: &HelixScreenshotFrameV1, event: helix_screenshot_frame_v1::Event, data: &ShotData, _: &Connection, _: &QueueHandle<Self>) {
        let image = match event {
            helix_screenshot_frame_v1::Event::Done { .. } => {
                let mut bytes = Vec::new();
                let mut file = &data.file;
                let read = file.seek(SeekFrom::Start(0)).is_ok() && file.read_to_end(&mut bytes).is_ok();
                read.then(|| crate::icons::decode(&bytes, 0)).flatten()
            }
            helix_screenshot_frame_v1::Event::Failed { .. } => None,
            _ => return,
        };
        frame.destroy();
        if ui.overview.is_some() {
            ui.overview_shots.requested.remove(&data.window);
            if let Some(image) = image {
                ui.overview_shots.pictures.insert(data.window, image);
            }
            ui.draw_overview();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiles_fit_and_keep_proportions() {
        let rects = arrange(&[(800, 600), (400, 300), (1920, 1080)], 1200, 700);
        assert_eq!(rects.len(), 3);
        for r in &rects {
            assert!(r.x >= 0 && r.y >= 0 && r.x + r.w <= 1200 && r.y + r.h + TITLE_H <= 700, "{r:?}");
        }
        // 400×300 is not enlarged.
        assert!(rects[1].w <= 400 && rects[1].h <= 300);
        // Proportions kept (within rounding).
        let ratio = rects[2].w as f64 / rects[2].h as f64;
        assert!((ratio - 1920.0 / 1080.0).abs() < 0.05, "{ratio}");
        assert!(arrange(&[], 100, 100).is_empty());
    }
}
