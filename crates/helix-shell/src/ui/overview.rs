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

use super::{accent, baseline, label, panel_height, physical, size, theme, Canvas, Overlay, Rect, Ui, DIM_FG, FG, SCRIM};
use crate::UiRequest;
use helix_i18n::tr;

/// Workspace thumbnail height (logical pixels).
const THUMB_H: i32 = 90;
/// Room under each window picture for its title.
const TITLE_H: i32 = 50;
const GAP: i32 = 24;
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
    /// Where the keyboard is: the windows, the workspace thumbnails or
    /// the dash; the thumbnail and dash icon chosen there.
    region: Region,
    workspace_key: Option<u32>,
    dash_key: Option<usize>,
    /// A press on a window picture, where.
    press: Option<(u32, (f64, f64))>,
    /// A window being dragged, and the pointer.
    drag: Option<(u32, (f64, f64))>,
    tiles: Vec<(Rect, u32)>,
    closes: Vec<(Rect, u32)>,
    workspaces: Vec<(Rect, u32)>,
    search: Rect,
    /// Dash icons: (where, app id, newest window, favorite).
    dash: Vec<(Rect, String, Option<u32>, bool)>,
    /// A press on a dash icon (app, where, its newest window), and a dash
    /// icon being dragged (app, pointer).
    dash_press: Option<(String, (f64, f64), Option<u32>)>,
    dash_drag: Option<(String, (f64, f64))>,
}

/// A window's app icon on its picture (logical px).
pub(super) const TILE_ICON: i32 = 48;

/// What a window in the overview shows.
pub(super) struct WindowTile<'a> {
    /// Its picture (none yet, or while dragged).
    pub picture: Option<&'a helix_draw::Image>,
    pub icon: Option<&'a helix_draw::Image>,
    pub title: &'a str,
    /// Under the pointer; chosen with the keyboard (the focus ring).
    pub hovered: bool,
    pub focused: bool,
    /// The close button shows (hovered or chosen).
    pub close: bool,
}

/// Paint window `t` at `r`: its picture rounded with a soft shadow (a
/// rounded light outline under the pointer, the focus ring when chosen
/// from the keyboard), its app's icon over the picture's bottom center,
/// its title under that (in a pill while lit), and the round close button
/// at the top right corner when `t.close`. Returns the close button's
/// place.
pub(super) fn paint_window_tile(canvas: &mut Canvas, text: Option<&crate::text::Text>, r: Rect, t: &WindowTile) -> Option<Rect> {
    let (x, y, w, h) = (f64::from(r.x), f64::from(r.y), f64::from(r.w), f64::from(r.h));
    let radius = theme::RADIUS_CARD;
    theme::shadow(canvas, x, y, w, h, radius);
    match t.picture {
        Some(shot) => canvas.image_rounded(x, y, w, h, radius, shot, 1.0),
        None => canvas.round_rect(x, y, w, h, radius, theme::CONTROL),
    }
    if helix_draw::high_contrast() {
        canvas.round_outline(x, y, w, h, radius, 1.0, 0xFFFF_FFFF);
    }
    if t.hovered {
        canvas.round_outline(x + 1.0, y + 1.0, w - 2.0, h - 2.0, radius, 3.0, if helix_draw::high_contrast() { 0xFFFF_FFFF } else { 0x80FF_FFFF });
    }
    if t.focused {
        // Outside the hover outline.
        theme::focus_ring_round(canvas, x - 3.0, y - 3.0, w + 6.0, h + 6.0, radius + 3.0);
    }
    // The app's icon, half over the picture's bottom edge.
    let icon = f64::from(TILE_ICON);
    let (ix, iy) = (x + w / 2.0 - icon / 2.0, y + h - icon * 0.6);
    if let Some(image) = t.icon {
        canvas.image(ix, iy, icon, icon, image);
    }
    // The title under it, in a pill while lit.
    let px = size::CAPTION;
    let room = (r.w.max(120) - 16) as f32;
    let tw = text.map_or(0.0, |f| f.width(t.title, px)).min(room);
    let ty = iy + icon + 6.0;
    let th = f64::from(helix_draw::theme::grow(24));
    if t.hovered || t.focused {
        let pw = f64::from(tw) + 20.0;
        theme::popover_round(canvas, x + w / 2.0 - pw / 2.0, ty, pw, th, th / 2.0);
    }
    label(text, canvas, t.title, (x + w / 2.0) as f32 - tw / 2.0, baseline(text, ty as i32, th as i32, px), px, FG, room);
    if !t.close {
        return None;
    }
    let d = 28.0;
    let (cx, cy) = (x + w - 4.0, y + 4.0);
    theme::round_control(canvas, cx, cy, d, theme::Look::default(), false);
    theme::close_symbol(canvas, cx, cy, 10.0, FG);
    Some(Rect { x: (cx - d / 2.0) as i32, y: (cy - d / 2.0) as i32, w: d as i32, h: d as i32 })
}

/// The overview's parts the keyboard moves between (Tab, Shift+Tab).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum Region {
    #[default]
    Windows,
    Workspaces,
    Dash,
}

/// Which way an arrow key points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Dir {
    Left,
    Right,
    Up,
    Down,
}

/// The rectangle nearest `from` in direction `dir` among `rects` (its
/// center must lie that way; closest along that way first, then across).
pub(super) fn nearest<T: Copy>(rects: &[(Rect, T)], from: Rect, dir: Dir) -> Option<T> {
    let c = |r: &Rect| (f64::from(r.x) + f64::from(r.w) / 2.0, f64::from(r.y) + f64::from(r.h) / 2.0);
    let (fx, fy) = c(&from);
    rects
        .iter()
        .filter_map(|(r, v)| {
            let (x, y) = c(r);
            let (along, across) = match dir {
                Dir::Left => (fx - x, (y - fy).abs()),
                Dir::Right => (x - fx, (y - fy).abs()),
                Dir::Up => (fy - y, (x - fx).abs()),
                Dir::Down => (y - fy, (x - fx).abs()),
            };
            (along > 1.0).then_some((along + 2.0 * across, *v))
        })
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, v)| v)
}

/// How far above the dash a dropped favorite is unpinned.
const DASH_UNPIN: f64 = 60.0;

/// Where a dash icon dropped at `x` goes among the favorites `rects`
/// (left to right): the index of the first one it is left of.
pub(super) fn dash_insert_index(rects: &[Rect], x: f64) -> usize {
    rects.iter().position(|r| x < f64::from(r.x + r.w / 2)).unwrap_or(rects.len())
}

/// Pictures of windows for the overview, shared by its monitors.
#[derive(Default)]
pub(super) struct Shots {
    pictures: HashMap<u32, Image>,
    /// Asked for and not answered yet.
    requested: HashSet<u32>,
}

impl Shots {
    /// The latest picture of window `id`, if any.
    pub(super) fn picture(&self, id: u32) -> Option<&Image> {
        self.pictures.get(&id)
    }
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
        // Not over the lock screen or the screenshot UI.
        if self.overview.is_some() || self.lock.session.is_some() || self.picker.is_some() {
            return;
        }
        let was_shown = self.launcher.is_some();
        self.close_launcher(false);
        let state = OverviewState { primary: true, selected: Some(self.windows.focused()).filter(|id| *id != 0), ..OverviewState::default() };
        let overlay = self.new_overlay("helix-overview", KeyboardInteractivity::Exclusive, state);
        // The panel stays in sight above it.
        overlay.layer.set_margin(panel_height() as i32, 0, 0, 0);
        overlay.layer.commit();
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
        self.dock_update();
    }

    pub(super) fn close_overview(&mut self, notify: bool) {
        self.overview_others.clear();
        self.overview_shots = Shots::default();
        if self.overview.take().is_some() {
            if notify {
                let _ = self.requests.send(UiRequest::Overview(false));
            }
            self.draw_panel();
            self.dock_update();
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
    pub(super) fn capture_shots(&mut self, ids: &[u32]) {
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
        let _busy = crate::watchdog::scope("draw overview");
        let started = std::time::Instant::now();
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
        tracing::debug!(ms = started.elapsed().as_secs_f64() * 1000.0, "overview drawn");
    }

    fn paint_overview(&mut self, overlay: &mut Overlay<OverviewState>) {
        let ids = if overlay.state.primary {
            self.windows_on(self.primary_area, true)
        } else {
            self.windows_on(overlay.state.area, false)
        };
        let (w, h) = (overlay.width as usize, overlay.height as usize);
        let scale = self.scales.of(overlay.layer.wl_surface());
        let dash = {
            let _step = crate::watchdog::scope("overview dash");
            if overlay.state.primary { self.dash_entries(scale) } else { Vec::new() }
        };
        let icons = self.overview_icons(&ids, (f64::from(TILE_ICON) * scale).ceil() as u32);
        let wallpaper = self.overview_wallpaper(scale);
        let wall_color = {
            let [r, g, b] = self.desktop_background.primary;
            0xFF00_0000 | (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
        };
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
        let fw = helix_draw::theme::grow(super::SEARCH_W).min(w - 32);
        let field = Rect { x: (w - fw) / 2, y: 24, w: fw, h: helix_draw::theme::grow(super::SEARCH_H) };
        state.workspaces.clear();
        let ty = if primary { field.y + field.h + 24 } else { -THUMB_H - 8 };
        if primary {
        super::search_field(&mut canvas, text, field, "", tr("Type to search…"), false);
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
            let (rx, ry, rw, rh) = (f64::from(r.x), f64::from(r.y), f64::from(r.w), f64::from(r.h));
            match &wallpaper {
                Some(image) if !helix_draw::high_contrast() => canvas.image_rounded(rx, ry, rw, rh, theme::RADIUS_ROW, image, 1.0),
                _ => canvas.round_rect(rx, ry, rw, rh, theme::RADIUS_ROW, if helix_draw::high_contrast() { 0xFF00_0000 } else { wall_color }),
            }
            if n == self.windows.active || dropping {
                canvas.round_outline(rx, ry, rw, rh, theme::RADIUS_ROW, 2.0, accent());
            } else {
                canvas.round_outline(rx, ry, rw, rh, theme::RADIUS_ROW, 1.0, if helix_draw::high_contrast() { 0xFFFF_FFFF } else { theme::BORDER });
            }
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
                    Some(shot) if (x0, y0, x1, y1) == (x, y, x + ww, y + wh) => canvas.image_rounded(x, y, ww, wh, 2.0, shot, 1.0),
                    _ => canvas.round_rect(x0, y0, x1 - x0, y1 - y0, 2.0, theme::CONTROL),
                }
            }
            if state.region == Region::Workspaces && state.workspace_key == Some(n) {
                helix_draw::theme::focus_ring_round(&mut canvas, f64::from(r.x), f64::from(r.y), f64::from(r.w), f64::from(r.h), theme::RADIUS_ROW);
            }
            state.workspaces.push((r, n));
            tx += thumb_w + 16;
        }
        }

        // The dash, at the bottom of the primary monitor.
        state.dash.clear();
        let dash_room = if dash.is_empty() { 0 } else { super::dash::DASH_H + 16 };
        if !dash.is_empty() {
            use super::dash::{DASH_GAP, DASH_H, DASH_ICON};
            let gaps = dash.windows(2).filter(|p| p[1].running_only && !p[0].running_only).count() as i32;
            let n = dash.len() as i32;
            let dw = n * DASH_ICON + (n - 1) * DASH_GAP + gaps * 16 + 2 * 12;
            let (dx, dy) = ((w - dw) / 2, h - DASH_H - 16);
            theme::popover_round(&mut canvas, f64::from(dx), f64::from(dy), f64::from(dw), f64::from(DASH_H), theme::RADIUS_POPOVER + 6.0);
            let mut x = dx + 12;
            for (i, entry) in dash.iter().enumerate() {
                if i > 0 && entry.running_only && !dash[i - 1].running_only {
                    // A separator between favorites and the rest.
                    theme::vseparator(&mut canvas, x + 2, dy + 14, DASH_ICON - 8);
                    x += 16;
                }
                let r = Rect { x, y: dy + 8, w: DASH_ICON, h: DASH_ICON };
                let dragged = state.dash_drag.as_ref().is_some_and(|(app, _)| *app == entry.app);
                match &entry.icon {
                    _ if dragged => {}
                    Some(icon) => canvas.image(f64::from(r.x), f64::from(r.y), f64::from(r.w), f64::from(r.h), icon),
                    None => {}
                }
                if !entry.windows.is_empty() {
                    canvas.circle(f64::from(r.x + r.w / 2), f64::from(dy + DASH_H - 7), 2.5, FG);
                }
                if state.region == Region::Dash && state.dash_key == Some(state.dash.len()) {
                    helix_draw::theme::focus_ring_round(&mut canvas, f64::from(r.x - 4), f64::from(r.y - 4), f64::from(r.w + 8), f64::from(r.h + 8), 8.0);
                }
                state.dash.push((r, entry.app.clone(), entry.windows.first().copied(), !entry.running_only));
                x += DASH_ICON + DASH_GAP;
            }
            // A dragged icon: where it would go, and the icon itself.
            if let Some((app, (px, py))) = state.dash_drag.clone() {
                let favorites: Vec<Rect> = state.dash.iter().filter(|d| d.3 && d.1 != app).map(|d| d.0).collect();
                if py >= f64::from(dy) - DASH_UNPIN {
                    let i = dash_insert_index(&favorites, px);
                    let mx = favorites.get(i).map_or_else(|| favorites.last().map_or(dx + 12, |r| r.x + r.w + DASH_GAP / 2), |r| r.x - DASH_GAP / 2);
                    canvas.rect(mx - 1, dy + 6, 3, DASH_H - 12, accent());
                }
                if let Some(icon) = dash.iter().find(|e| e.app == app).and_then(|e| e.icon.as_ref()) {
                    canvas.image(px - f64::from(DASH_ICON) / 2.0, py - f64::from(DASH_ICON) / 2.0, f64::from(DASH_ICON), f64::from(DASH_ICON), icon);
                }
            }
        }

        // The active workspace's windows on this monitor.
        let area = Rect { x: 32, y: ty + THUMB_H + 32, w: w - 64, h: h - (ty + THUMB_H + 32) - 24 - dash_room };
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
            let msg = tr("No open windows");
            let tw = text.map_or(0.0, |t| t.width(msg, size::BODY));
            label(text, &mut canvas, msg, (w as f32 - tw) / 2.0, (area.y + 40) as f32, size::BODY, DIM_FG, area.w as f32);
        }
        for ((id, rect), icon) in ids.iter().zip(rects).zip(&icons) {
            let r = Rect { x: area.x + rect.x, y: area.y + rect.y, w: rect.w, h: rect.h };
            let dragged = state.drag.is_some_and(|(d, _)| d == *id);
            let title = self.windows.mru().iter().find(|e| e.id == *id).map(|e| e.label().to_owned()).unwrap_or_default();
            let tile = WindowTile {
                picture: self.overview_shots.pictures.get(id).filter(|_| !dragged),
                icon: icon.as_ref(),
                title: &title,
                hovered: state.hover == Some(*id) && !dragged,
                focused: state.selected == Some(*id) && !dragged && state.region == Region::Windows,
                close: (state.selected == Some(*id) || state.hover == Some(*id)) && !dragged,
            };
            if let Some(c) = paint_window_tile(&mut canvas, text, r, &tile) {
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

    /// The desktop's wallpaper small enough for the workspace thumbnails
    /// (made once per wallpaper and scale).
    fn overview_wallpaper(&mut self, scale: f64) -> Option<std::sync::Arc<helix_draw::Image>> {
        let image = self.desktop_background.image.clone()?;
        let key = (std::sync::Arc::as_ptr(&image) as usize, (f64::from(THUMB_H) * scale).ceil() as u32);
        if let Some((k, thumb)) = &self.overview_wall {
            if *k == key {
                return Some(thumb.clone());
            }
        }
        // Shrinking a large wallpaper takes long (a second for a big
        // picture): on a worker, the overview drawn again when it is done.
        if self.overview_wall_wanted != Some(key) {
            self.overview_wall_wanted = Some(key);
            let height = key.1;
            self.offload(
                move || std::sync::Arc::new(crate::wallpaper::thumbnail(&image, height * 2, height)),
                move |ui, thumb| {
                    if ui.overview_wall_wanted != Some(key) {
                        return;
                    }
                    ui.overview_wall_wanted = None;
                    ui.overview_wall = Some((key, thumb));
                    if ui.overview.is_some() {
                        ui.draw_overview();
                    }
                },
            );
        }
        // Meanwhile: the same picture at another scale, else none (the
        // wallpaper's colour).
        self.overview_wall.as_ref().filter(|(k, _)| k.0 == key.0).map(|(_, thumb)| thumb.clone())
    }

    /// The overview's window icons, `px` large.
    fn overview_icons(&mut self, ids: &[u32], px: u32) -> Vec<Option<helix_draw::Image>> {
        ids.iter().map(|id| self.window_icon(*id, px)).collect()
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
                if let Some((app, (sx, sy), _)) = state.dash_press.clone() {
                    if ((x - sx).powi(2) + (y - sy).powi(2)).sqrt() >= crate::drag_threshold() {
                        state.dash_press = None;
                        state.dash_drag = Some((app, (x, y)));
                    }
                }
                if let Some(drag) = state.dash_drag.as_mut() {
                    drag.1 = (x, y);
                    self.draw_overview();
                    return true;
                }
                if let Some((id, (sx, sy))) = state.press {
                    if state.drag.is_none() && ((x - sx).powi(2) + (y - sy).powi(2)).sqrt() >= crate::drag_threshold() {
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
                } else if let Some((_, app, window, _)) = state.dash.iter().find(|d| d.0.contains(x, y)).cloned() {
                    state.dash_press = Some((app, (x, y), window));
                } else if state.search.contains(x, y) {
                    self.close_overview(false);
                    self.open_launcher(false);
                } else {
                    self.close_overview(true);
                }
            }
            PointerEventKind::Release { button: BTN_LEFT, .. } => {
                if let Some((app, _)) = state.dash_drag.take() {
                    let shown: Vec<(Rect, String)> = state.dash.iter().filter(|d| d.3 && d.1 != app).map(|d| (d.0, d.1.clone())).collect();
                    let rects: Vec<Rect> = shown.iter().map(|(r, _)| *r).collect();
                    let dash_top = state.dash.first().map_or(0.0, |d| f64::from(d.0.y - 8));
                    let unpin = y < dash_top - DASH_UNPIN;
                    // Before the icon it lands left of (favorites not shown,
                    // apps not installed, keep their places).
                    let before = shown.get(dash_insert_index(&rects, x)).map(|(_, a)| format!("{a}.desktop"));
                    let desktop = format!("{app}.desktop");
                    let mut list = crate::favorites::favorites();
                    let was = list.iter().position(|f| f.eq_ignore_ascii_case(&desktop));
                    let id = was.map_or(desktop, |i| list.remove(i));
                    if unpin {
                        tracing::info!(app = id, "unpinned from the dash");
                    } else {
                        let index = before.and_then(|b| list.iter().position(|f| f.eq_ignore_ascii_case(&b))).unwrap_or(list.len());
                        list.insert(index, id.clone());
                        tracing::info!(app = id, index, "dash reordered");
                    }
                    if was.is_some() || !unpin {
                        crate::favorites::set_favorites(&list);
                    }
                    self.draw_overview();
                    return true;
                }
                if let Some((app, _, window)) = state.dash_press.take() {
                    if state.dash.iter().any(|d| d.1 == app && d.0.contains(x, y)) {
                        self.dash_activate(&app, window);
                    }
                    return true;
                }
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
        let (ctrl, alt, shift) = (self.mods.ctrl, self.mods.alt, self.mods.shift);
        let active = self.windows.active;
        let count = self.windows.count.max(1);
        let Some(overview) = self.overview.as_mut() else { return };
        let state = &mut overview.state;
        let selected = state.selected.filter(|s| ids.contains(s));
        let dir = match event.keysym {
            Keysym::Left => Some(Dir::Left),
            Keysym::Right => Some(Dir::Right),
            Keysym::Up => Some(Dir::Up),
            Keysym::Down => Some(Dir::Down),
            _ => None,
        };
        // Moving the chosen window to the neighbouring workspace:
        // Ctrl+Alt+←/→ or Shift+Page Up/Down.
        let move_by = match event.keysym {
            Keysym::Left if ctrl && alt => Some(-1),
            Keysym::Right if ctrl && alt => Some(1),
            Keysym::Page_Up if shift => Some(-1),
            Keysym::Page_Down if shift => Some(1),
            _ => None,
        };
        if let (Some(by), Some(id), Region::Windows) = (move_by, selected, state.region) {
            let target = i64::from(active) + by;
            if target >= 0 && target < i64::from(count) {
                if let Some(list) = self.window_list.as_ref().filter(|l| l.version() >= 2) {
                    list.move_window(id, target as u32);
                }
            }
            return;
        }
        match event.keysym {
            Keysym::Escape => self.close_overview(true),
            Keysym::Tab | Keysym::ISO_Left_Tab => {
                // The parts in order; ones with nothing in them are passed.
                let order = [Region::Windows, Region::Workspaces, Region::Dash];
                let has = |r: Region| match r {
                    Region::Windows => !ids.is_empty(),
                    Region::Workspaces => !state.workspaces.is_empty(),
                    Region::Dash => !state.dash.is_empty(),
                };
                let back = event.keysym == Keysym::ISO_Left_Tab || shift;
                let at = order.iter().position(|r| *r == state.region).unwrap_or(0);
                for k in 1..=order.len() {
                    let i = if back { (at + order.len() * 2 - k) % order.len() } else { (at + k) % order.len() };
                    if has(order[i]) {
                        state.region = order[i];
                        break;
                    }
                }
                match state.region {
                    Region::Windows if selected.is_none() => state.selected = ids.first().copied(),
                    Region::Workspaces if state.workspace_key.is_none() => state.workspace_key = Some(active),
                    Region::Dash if state.dash_key.is_none() => state.dash_key = Some(0),
                    _ => {}
                }
                self.draw_overview();
            }
            Keysym::Return | Keysym::KP_Enter | Keysym::space => match state.region {
                Region::Windows => {
                    if let Some(id) = selected {
                        self.close_overview(true);
                        self.activate_window(id);
                    }
                }
                Region::Workspaces => {
                    if let (Some(n), Some(list)) = (state.workspace_key, self.window_list.as_ref().filter(|l| l.version() >= 2)) {
                        list.switch_workspace(n);
                    }
                }
                Region::Dash => {
                    let chosen = state.dash_key.and_then(|i| state.dash.get(i)).map(|d| (d.1.clone(), d.2));
                    if let Some((app, window)) = chosen {
                        self.dash_activate(&app, window);
                    }
                }
            },
            Keysym::Delete | Keysym::KP_Delete if state.region == Region::Windows => self.overview_close_selected(selected),
            Keysym::w if ctrl && state.region == Region::Windows => self.overview_close_selected(selected),
            _ if dir.is_some() => {
                let dir = dir.unwrap_or(Dir::Right);
                match state.region {
                    Region::Windows => {
                        let from = selected.and_then(|id| state.tiles.iter().find(|(_, t)| *t == id).map(|(r, _)| *r));
                        let next = match from {
                            Some(from) => nearest(&state.tiles, from, dir),
                            None => ids.first().copied(),
                        };
                        if let Some(next) = next {
                            state.selected = Some(next);
                        }
                    }
                    Region::Workspaces => {
                        let n = state.workspace_key.unwrap_or(active);
                        state.workspace_key = Some(match dir {
                            Dir::Left | Dir::Up => n.saturating_sub(1),
                            Dir::Right | Dir::Down => (n + 1).min(count - 1),
                        });
                    }
                    Region::Dash => {
                        let i = state.dash_key.unwrap_or(0);
                        state.dash_key = Some(match dir {
                            Dir::Left | Dir::Up => i.saturating_sub(1),
                            Dir::Right | Dir::Down => (i + 1).min(state.dash.len().saturating_sub(1)),
                        });
                    }
                }
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
                    self.search_changed();
                    self.draw_launcher();
                }
            }
        }
    }

    /// Close the window chosen with the keyboard; the choice moves to a
    /// neighbour.
    fn overview_close_selected(&mut self, selected: Option<u32>) {
        let Some(id) = selected else { return };
        if let Some(list) = self.window_list.as_ref().filter(|l| l.version() >= 4) {
            list.window_action(id, WindowAction::Close);
        }
        let ids = self.overview_windows();
        if let Some(overview) = self.overview.as_mut() {
            overview.state.selected = ids.iter().copied().find(|w| *w != id);
        }
        self.draw_overview();
    }
}

/// What the overview shows on the accessibility bus, as painted.
pub(super) struct OverviewShown<'a> {
    /// The search field (primary monitor only).
    pub search: Option<Rect>,
    /// Workspace thumbnails: place, name, the active one.
    pub workspaces: &'a [(Rect, String, bool)],
    /// Window pictures: place, window, title, app.
    pub windows: &'a [(Rect, u32, String, String)],
    /// Dash icons: place, app, name, running.
    pub dash: &'a [(Rect, String, String, bool)],
    /// The window chosen, where the keyboard is, and its thumbnail and
    /// dash icon there.
    pub selected: Option<u32>,
    pub region: Region,
    pub workspace_key: Option<u32>,
    pub dash_key: Option<usize>,
}

/// The overview on the accessibility bus (helix-desktop-spec
/// `contracts/dbus/org.a11y.atspi.md`, "Window overview"): the search
/// field; a `Workspaces` list box (the active workspace selected); a
/// `Windows` list box of the window pictures, each named by its title
/// and described by its app (the chosen one selected); and the `Dash`
/// tool bar of app buttons. The keyboard's place (Tab between the parts,
/// arrows inside) has the focus.
pub(super) fn overview_nodes(s: &OverviewShown) -> Vec<crate::a11y::Node> {
    use crate::a11y::{Node, Role};
    use helix_i18n::tr;
    let mut out = Vec::new();
    let keyboard_on = |r: Region| s.region == r;
    if let Some(search) = s.search {
        let nothing = s.selected.is_none() || !s.windows.iter().any(|w| Some(w.1) == s.selected);
        out.push(Node::text_field(Role::SearchInput, tr("Search"), "", search.into()).id("search").focused(keyboard_on(Region::Windows) && nothing));
    }
    if !s.workspaces.is_empty() {
        let thumbs = s.workspaces.iter().enumerate().map(|(n, (r, name, active))| {
            let focused = keyboard_on(Region::Workspaces) && s.workspace_key == Some(n as u32);
            Node::new(Role::ListBoxOption, name).id(format!("workspace:{n}")).at((*r).into()).click().focusable().selected(*active).focused(focused)
        });
        out.push(Node::new(Role::ListBox, tr("Workspaces")).id("workspaces").children(thumbs));
    }
    let pictures = s.windows.iter().map(|(r, id, title, app)| {
        let on = s.selected == Some(*id);
        Node::new(Role::ListBoxOption, title).id(format!("window:{id}")).describe(app.as_str()).at((*r).into()).click().focusable().selected(on).focused(on && keyboard_on(Region::Windows))
    });
    let windows = Node::new(Role::ListBox, tr("Windows")).id("windows").children(pictures);
    out.push(if s.windows.is_empty() { windows.child(Node::label(tr("No open windows"))) } else { windows });
    if !s.dash.is_empty() {
        let icons = s.dash.iter().enumerate().map(|(i, (r, app, name, running))| {
            let focused = keyboard_on(Region::Dash) && s.dash_key == Some(i);
            let node = Node::button(name, (*r).into()).id(app.clone()).focusable().focused(focused);
            if *running { node.describe(tr("Running")) } else { node }
        });
        out.push(Node::new(Role::Toolbar, tr("Dash")).id("dash").children(icons));
    }
    out
}

impl Ui {
    /// The overview surfaces (primary first), for the accessibility bus.
    pub(super) fn overview_a11y(&self) -> Vec<super::atspi::SurfaceAt<'_>> {
        let primary = self.overview.iter().map(|o| (o, (self.primary_area.0, self.primary_area.1)));
        let others = self.overview_others.iter().map(|o| (o, (o.state.area.0, o.state.area.1)));
        primary.chain(others).filter(|(o, _)| o.configured && o.width > 0).map(|(o, (x, y))| (&o.layer, (x, y, o.width, o.height))).collect()
    }

    /// What overview surface `layer` shows, for the accessibility bus.
    pub(super) fn overview_a11y_nodes(&self, layer: &LayerSurface, names: Option<&HashMap<String, String>>) -> Vec<crate::a11y::Node> {
        let Some(o) = self.overview.iter().chain(self.overview_others.iter()).find(|o| &o.layer == layer) else { return Vec::new() };
        let state = &o.state;
        let app_name = |app: &str| {
            let key = app.strip_suffix(".desktop").unwrap_or(app).to_lowercase();
            names.and_then(|n| n.get(&key)).cloned().unwrap_or_else(|| app.to_owned())
        };
        let ws_names = if state.primary { super::workspace_popup::read_names() } else { Vec::new() };
        let workspaces: Vec<(Rect, String, bool)> = state.workspaces.iter().map(|(r, n)| (*r, super::workspace_popup::workspace_name(&ws_names, *n), *n == self.windows.active)).collect();
        let windows: Vec<(Rect, u32, String, String)> = state
            .tiles
            .iter()
            .map(|(r, id)| {
                let entry = self.windows.mru().iter().find(|e| e.id == *id);
                let title = entry.map(|e| e.label().to_owned()).unwrap_or_default();
                (*r, *id, title, entry.map(|e| app_name(&e.app_id)).unwrap_or_default())
            })
            .collect();
        let dash: Vec<(Rect, String, String, bool)> = state.dash.iter().map(|(r, app, newest, _)| (*r, app.clone(), app_name(app), newest.is_some())).collect();
        overview_nodes(&OverviewShown {
            search: state.primary.then_some(state.search),
            workspaces: &workspaces,
            windows: &windows,
            dash: &dash,
            selected: state.selected,
            region: state.region,
            workspace_key: state.workspace_key,
            dash_key: state.dash_key,
        })
    }
}

#[cfg(test)]
mod nav_tests {
    use super::*;

    #[test]
    fn arrows_go_to_the_nearest_window_that_way() {
        let r = |x, y| Rect { x, y, w: 100, h: 80 };
        // A 2×2 grid and one off to the right, higher.
        let tiles = [(r(0, 0), 1), (r(200, 0), 2), (r(0, 200), 3), (r(200, 200), 4), (r(400, 120), 5)];
        assert_eq!(nearest(&tiles, r(0, 0), Dir::Right), Some(2));
        assert_eq!(nearest(&tiles, r(0, 0), Dir::Down), Some(3));
        assert_eq!(nearest(&tiles, r(0, 0), Dir::Left), None, "nothing that way");
        assert_eq!(nearest(&tiles, r(200, 200), Dir::Up), Some(2));
        assert_eq!(nearest(&tiles, r(200, 200), Dir::Right), Some(5));
        assert_eq!(nearest(&tiles, r(400, 120), Dir::Left), Some(4), "the row nearer across wins");
    }
}

impl Dispatch<HelixScreenshotFrameV1, ShotData> for Ui {
    fn event(ui: &mut Self, frame: &HelixScreenshotFrameV1, event: helix_screenshot_frame_v1::Event, data: &ShotData, _: &Connection, _: &QueueHandle<Self>) {
        match event {
            helix_screenshot_frame_v1::Event::Done { .. } => {
                frame.destroy();
                // Read and decoded on a worker (a large window's PNG takes
                // a while).
                let window = data.window;
                match data.file.try_clone() {
                    Ok(mut file) => ui.offload(
                        move || {
                            let mut bytes = Vec::new();
                            let read = file.seek(SeekFrom::Start(0)).is_ok() && file.read_to_end(&mut bytes).is_ok();
                            read.then(|| crate::icons::decode(&bytes, 0)).flatten()
                        },
                        move |ui, image| ui.window_shot(window, image),
                    ),
                    Err(_) => ui.window_shot(window, None),
                }
            }
            helix_screenshot_frame_v1::Event::Failed { .. } => {
                frame.destroy();
                ui.window_shot(data.window, None);
            }
            _ => {}
        }
    }
}

impl Ui {
    /// Window `window`'s picture came (or not).
    fn window_shot(&mut self, window: u32, image: Option<helix_draw::Image>) {
        self.overview_shots.requested.remove(&window);
        if let Some(image) = image {
            self.overview_shots.pictures.insert(window, image);
        }
        if self.overview.is_some() {
            self.draw_overview();
        }
        if self.window_menu.as_ref().is_some_and(|m| m.previews.contains(&window)) {
            self.draw_window_menu();
        }
        if self.switcher.as_ref().is_some_and(|s| s.state.switcher.entries.iter().any(|e| e.id == window)) {
            self.draw_switcher();
        }
    }
}

#[cfg(test)]
mod dash_tests {
    use super::*;

    #[test]
    fn dash_drop_index() {
        let r = |x| Rect { x, y: 0, w: 48, h: 48 };
        let rects = [r(0), r(58), r(116)];
        assert_eq!(dash_insert_index(&rects, 10.0), 0);
        assert_eq!(dash_insert_index(&rects, 30.0), 1);
        assert_eq!(dash_insert_index(&rects, 500.0), 3);
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
