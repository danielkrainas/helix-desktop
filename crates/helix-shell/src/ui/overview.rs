//! The overview (helixdesktop/spec `contracts/wayland/README.md`,
//! "Overview"): a search field, workspace thumbnails and the active
//! workspace's windows as pictures, on the primary monitor.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use helix_draw::Image;
use helix_protocols::client::helix_screenshot_frame_v1::{self, HelixScreenshotFrameV1};
use helix_protocols::client::helix_window_list_v1::Action as WindowAction;
use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay_client_toolkit::reexports::calloop::RegistrationToken;
use smithay_client_toolkit::reexports::client::protocol::wl_shm;
use smithay_client_toolkit::reexports::client::backend::ObjectId;
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
    /// Hover, press and launch feedback on the dash icons (by app id), as
    /// on the dock ("Item feedback").
    pub(super) dash_feedback: crate::item_feedback::Feedback<String>,
    /// The dash icon under the pointer, and when its tooltip shows (as on
    /// the dock: after a rest, at once when moving on from another).
    dash_hover: Option<String>,
    dash_tooltip_at: Option<Instant>,
    dash_tooltip_shown: bool,
    /// The dash was drawn since its tooltip became due.
    dash_tooltip_due_drawn: bool,
    dash_tooltip_gone: Option<Instant>,
    /// Draws again while the dash's feedback moves or a tooltip is due.
    redraw: Option<RegistrationToken>,
    /// The dash's entries as last painted, and the frame without the dash
    /// ((buffer length, scale), pixels) to repaint it on.
    dash_entries: Vec<super::dash::DashEntry>,
    dash_base: Option<((usize, u64), Vec<u8>)>,
    /// The top of the dash as last painted (its tooltip's, if shown).
    dash_top: i32,
    /// Kept open for an app's launch copy, then closed.
    pub(super) linger: Option<RegistrationToken>,
    /// The last window moved with the keyboard or an accessibility
    /// action, said: "Terminal moved to Workspace 2".
    moved: Option<String>,
}

/// The AT-SPI custom actions of a window picture: moving it to workspace
/// N is `MOVE_TO_WORKSPACE + N`.
pub(super) const MOVE_TO_WORKSPACE: i32 = 0x4858_0100;
/// How many workspaces have such an action.
const MOVE_ACTIONS: u32 = 0x100;

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

/// What the dash shows now: each icon's feedback, launch copies, the
/// keyboard's choice and the tooltip (under the pointer once it has
/// rested there, else the keyboard's choice; none while pressing or
/// dragging). Nothing moves while an icon is dragged.
fn dash_model<'a>(state: &mut OverviewState, dash: &'a [super::dash::DashEntry], focused_app: &'a str, indicator: crate::dock::Indicator) -> super::dash::DashPaint<'a> {
    let now = Instant::now();
    let keyboard = (state.region == Region::Dash).then_some(state.dash_key).flatten().filter(|k| *k < dash.len());
    state.dash_feedback.set_focus(keyboard.map(|k| dash[k].app.clone()), now);
    let index = |app: &str| dash.iter().position(|e| e.app == app);
    let rest = crate::item_feedback::Look { highlight: 0.0, pressed: false, scale: 1.0 };
    let looks = dash.iter().map(|e| if state.dash_drag.is_some() { rest } else { state.dash_feedback.look(&e.app, now) }).collect();
    let ghosts = state.dash_feedback.ghosts(now).into_iter().filter_map(|(app, p)| Some((index(&app)?, p))).collect();
    let due = state.dash_tooltip_at.is_some_and(|t| now >= t);
    state.dash_tooltip_due_drawn |= due;
    let pointed = state.dash_hover.as_deref().filter(|_| due).and_then(index);
    let tooltip = if state.dash_drag.is_some() || state.dash_press.is_some() { None } else { pointed.or(keyboard) };
    state.dash_tooltip_shown = pointed.is_some() && tooltip == pointed;
    super::dash::DashPaint { entries: dash, looks, ghosts, keyboard, focused_app, indicator, tooltip, drag: state.dash_drag.as_ref().map(|(app, at)| (app.clone(), *at)) }
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
pub(super) const DASH_UNPIN: f64 = 60.0;

/// Where a dash icon dropped at `x` goes among the favorites `rects`
/// (left to right): the index of the first one it is left of.
pub(super) fn dash_insert_index(rects: &[Rect], x: f64) -> usize {
    rects.iter().position(|r| x < f64::from(r.x + r.w / 2)).unwrap_or(rects.len())
}

/// Pictures of windows for the overview, shared by its monitors.
#[derive(Default)]
pub(super) struct Shots {
    pictures: HashMap<u32, Image>,
    /// Asked for and not answered yet: at most one request per window.
    requested: HashMap<u32, Asked>,
    /// The timer that gives up on unanswered requests is running.
    sweeping: bool,
}

impl Shots {
    /// The latest picture of window `id`, if any.
    pub(super) fn picture(&self, id: u32) -> Option<&Image> {
        self.pictures.get(&id)
    }

    /// Forget the pictures and give up on the requests.
    fn clear(&mut self) {
        self.pictures.clear();
        self.requested.clear();
    }

    /// Give up on requests older than `SHOT_TIMEOUT`; how long until the
    /// next one is due (none left: `None`).
    fn sweep(&mut self, now: Instant) -> Option<Duration> {
        self.requested.retain(|_, asked| now.saturating_duration_since(asked.since) < SHOT_TIMEOUT);
        self.requested.values().map(|asked| SHOT_TIMEOUT.saturating_sub(now.saturating_duration_since(asked.since))).min()
    }
}

/// A window picture asked for. Dropping it gives up on it: the frame is
/// destroyed and its memory file closed.
struct Asked {
    frame: HelixScreenshotFrameV1,
    /// What the compositor writes the picture to.
    file: File,
    since: Instant,
}

impl Drop for Asked {
    fn drop(&mut self) {
        if self.frame.is_alive() {
            self.frame.destroy();
        }
    }
}

/// How long a window picture may take before it is given up on (the
/// compositor answers in milliseconds; one that never answers must not
/// leave memory files behind).
const SHOT_TIMEOUT: Duration = Duration::from_secs(2);

/// How often pictures are renewed while the overview is open.
const LIVE: std::time::Duration = std::time::Duration::from_millis(500);

/// A window picture on its way: which window (its file is with the
/// request, in `Shots`).
pub(super) struct ShotData {
    window: u32,
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
        let state = OverviewState {
            primary: true,
            selected: Some(self.windows.focused()).filter(|id| *id != 0),
            dash_feedback: crate::item_feedback::Feedback::with_motion(crate::item_feedback::motion()),
            ..OverviewState::default()
        };
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
        self.overview_shots.clear();
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
        let now = Instant::now();
        self.overview_shots.sweep(now);
        for &id in ids {
            if self.overview_shots.requested.contains_key(&id) {
                continue;
            }
            let Some(file) = crate::screenshot::memfd() else { continue };
            let frame = manager.capture_window_id(id, file.as_fd(), &self.qh, ShotData { window: id });
            self.overview_shots.requested.insert(id, Asked { frame, file, since: now });
        }
        self.sweep_shots_later();
    }

    /// Give up on unanswered pictures when they are due (one timer, while
    /// any are waiting).
    fn sweep_shots_later(&mut self) {
        if self.overview_shots.sweeping || self.overview_shots.requested.is_empty() {
            return;
        }
        let Some(handle) = self.loop_handle.clone() else { return };
        use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
        let started = handle.insert_source(Timer::from_duration(SHOT_TIMEOUT), |_, _, ui| match ui.overview_shots.sweep(Instant::now()) {
            Some(next) => TimeoutAction::ToDuration(next.max(Duration::from_millis(10))),
            None => {
                ui.overview_shots.sweeping = false;
                TimeoutAction::Drop
            }
        });
        self.overview_shots.sweeping = started.is_ok();
    }

    /// The switcher closed: give up on the pictures still on their way,
    /// unless the overview shows them.
    pub(super) fn cancel_shots(&mut self) {
        if self.overview.is_none() {
            self.overview_shots.requested.clear();
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
        self.overview_schedule_redraw();
    }

    /// While the dash's feedback moves, draw again next frame; with a
    /// tooltip due, draw again when it is.
    fn overview_schedule_redraw(&mut self) {
        let now = Instant::now();
        let Some(state) = self.overview.as_mut().map(|o| &mut o.state) else { return };
        let after = if state.dash_feedback.moving(now) {
            Some(Duration::from_millis(16))
        } else {
            // Not drawn since it became due (also when the last drawing was
            // just before that): when it is, or at once.
            state.dash_tooltip_at.filter(|_| !state.dash_tooltip_due_drawn).map(|t| t.saturating_duration_since(now))
        };
        let (Some(after), None, Some(handle)) = (after, state.redraw.as_ref(), self.loop_handle.as_ref()) else { return };
        let token = handle.insert_source(Timer::from_duration(after), |_, _, ui: &mut Ui| {
            if let Some(o) = ui.overview.as_mut() {
                o.state.redraw = None;
            }
            ui.overview_dash_changed();
            TimeoutAction::Drop
        });
        state.redraw = token.ok();
    }

    /// Only the dash's look changed (hover, press, feedback moving, its
    /// tooltip): repaint just the dash, else draw the whole overview.
    pub(super) fn overview_dash_changed(&mut self) {
        if !self.overview_repaint_dash() {
            self.draw_overview();
        }
    }

    /// Repaint the dash on the frame kept from the last full drawing
    /// (`dash_base`) and tell the compositor only its strip changed. False
    /// when that cannot be done.
    fn overview_repaint_dash(&mut self) -> bool {
        let _busy = crate::watchdog::scope("draw overview dash");
        let started = Instant::now();
        let Some(mut overlay) = self.overview.take() else { return true };
        let done = self.overview_repaint_dash_in(&mut overlay);
        self.overview = Some(overlay);
        if done {
            tracing::debug!(ms = started.elapsed().as_secs_f64() * 1000.0, "overview dash drawn");
            self.overview_schedule_redraw();
        }
        done
    }

    fn overview_repaint_dash_in(&mut self, overlay: &mut Overlay<OverviewState>) -> bool {
        let state = &mut overlay.state;
        if !overlay.configured || state.dash_drag.is_some() || state.drag.is_some() || state.dash_entries.is_empty() {
            return false;
        }
        let (w, h) = (overlay.width as usize, overlay.height as usize);
        let scale = self.scales.of(overlay.layer.wl_surface());
        let (pw, ph) = (physical(w, scale), physical(h, scale));
        let Some((_, base)) = state.dash_base.as_ref().filter(|(key, _)| *key == (pw * ph * 4, scale.to_bits())) else { return false };
        let Ok((buffer, data)) = self.buffers.create_buffer(overlay.layer.wl_surface(), Ui::draw_overview, pw as i32, ph as i32, (pw * 4) as i32, wl_shm::Format::Argb8888) else { return false };
        data.copy_from_slice(base);
        let mut canvas = Canvas::scaled(data, w, h, scale);
        let focused_app = self.windows.mru().iter().find(|e| e.id == self.windows.focused()).map(|e| e.app_id.clone()).unwrap_or_default();
        let entries = std::mem::take(&mut state.dash_entries);
        let paint = dash_model(state, &entries, &focused_app, self.dock.settings.indicator);
        let (_, top) = super::dash::paint_dash(&mut canvas, self.text.as_ref(), (w as i32, h as i32), &paint);
        state.dash_entries = entries;
        // What the dash covers now and covered before (a tooltip that went).
        let y0 = ((f64::from(top.min(state.dash_top).max(0)) * scale).floor() as usize).saturating_sub(1).min(ph);
        state.dash_top = top;
        let surface = overlay.layer.wl_surface();
        self.scales.prepare_damage(&self.qh, surface, (w as i32, h as i32), &[(0, y0, pw, ph)]);
        if buffer.attach_to(surface).is_ok() {
            overlay.layer.commit();
        }
        true
    }

    /// The pointer is over dash icon `app` (or none): its feedback, and
    /// when its tooltip shows.
    fn dash_set_hover(state: &mut OverviewState, app: Option<String>) -> bool {
        if state.dash_hover == app {
            return false;
        }
        let now = Instant::now();
        if state.dash_tooltip_shown {
            state.dash_tooltip_gone = Some(now);
        }
        let (at, _) = super::dock::tooltip_from(now, state.dash_tooltip_shown, state.dash_tooltip_gone);
        state.dash_tooltip_at = app.as_ref().map(|_| at);
        state.dash_tooltip_due_drawn = false;
        state.dash_feedback.set_hover(app.clone(), now);
        state.dash_hover = app;
        true
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
        let Ok((buffer, data)) = self.buffers.create_buffer(overlay.layer.wl_surface(), Ui::draw_overview, physical(w, scale) as i32, physical(h, scale) as i32, (physical(w, scale) * 4) as i32, wl_shm::Format::Argb8888) else {
            return;
        };
        let mut canvas = Canvas::scaled(data, w, h, scale);
        canvas.fill(SCRIM);
        let indicator = self.dock.settings.indicator;
        let focused_app = self.windows.mru().iter().find(|e| e.id == self.windows.focused()).map(|e| e.app_id.clone()).unwrap_or_default();
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
                    // No picture (yet): a Control box, outlined in high contrast
                    // (black on the black thumbnail otherwise).
                    _ => theme::control_round(&mut canvas, (x0, y0, x1 - x0, y1 - y0), 2.0, theme::Look::default()),
                }
            }
            if state.region == Region::Workspaces && state.workspace_key == Some(n) {
                helix_draw::theme::focus_ring_round(&mut canvas, f64::from(r.x), f64::from(r.y), f64::from(r.w), f64::from(r.h), theme::RADIUS_ROW);
            }
            state.workspaces.push((r, n));
            tx += thumb_w + 16;
        }
        }

        // Room for the dash at the bottom of the primary monitor (painted
        // last, see below).
        state.dash.clear();
        let dash_room = if dash.is_empty() { 0 } else { super::dash::DASH_H + 16 };

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
        if tracing::enabled!(tracing::Level::DEBUG) {
            // Where each window's tile is on screen, for tests:
            // `title@x,y,w,h` (titles' spaces as `_`); the other monitors'
            // as "overview other tiles".
            let (ox, oy) = if primary { (self.primary_area.0, self.primary_area.1 + panel_height() as i32) } else { (state.area.0, state.area.1) };
            let tiles: Vec<String> = state
                .tiles
                .iter()
                .map(|(r, id)| {
                    let title = self.windows.mru().iter().find(|e| e.id == *id).map(|e| e.label().replace(' ', "_")).unwrap_or_default();
                    format!("{title}@{},{},{},{}", ox + r.x, oy + r.y, r.w, r.h)
                })
                .collect();
            if primary {
                tracing::debug!(tiles = tiles.join(" "), "overview tiles");
            } else {
                tracing::debug!(tiles = tiles.join(" "), "overview other tiles");
            }
        }
        // The dash, last: its feedback is repainted on the frame as it was
        // before the dash (`dash_base`), without drawing the rest again.
        state.dash_base = None;
        if !dash.is_empty() {
            if state.drag.is_none() {
                state.dash_base = Some(((canvas.data.len(), scale.to_bits()), canvas.data.to_vec()));
            }
            let paint = dash_model(state, &dash, &focused_app, indicator);
            let (rects, top) = super::dash::paint_dash(&mut canvas, text, (w, h), &paint);
            state.dash_top = top;
            state.dash = dash.iter().zip(rects).map(|(e, r)| (r, e.app.clone(), e.windows.first().copied(), !e.running_only)).collect();
            if tracing::enabled!(tracing::Level::DEBUG) {
                // On screen, as the tiles below: `app@x,y,w,h`.
                let (ox, oy) = (self.primary_area.0, self.primary_area.1 + panel_height() as i32);
                let icons: Vec<String> = state.dash.iter().map(|(r, app, ..)| format!("{app}@{},{},{},{}", ox + r.x, oy + r.y, r.w, r.h)).collect();
                tracing::debug!(dash = icons.join(" "), "overview dash");
            }
        }
        state.dash_entries = dash;
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
                        state.dash_feedback.release(Instant::now());
                        Self::dash_set_hover(state, None);
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
                let dash_hover = state.dash.iter().find(|d| d.0.contains(x, y)).map(|d| d.1.clone());
                let dash_changed = Self::dash_set_hover(state, dash_hover);
                if hover != state.hover {
                    state.hover = hover;
                    self.draw_overview();
                } else if dash_changed {
                    self.overview_dash_changed();
                }
            }
            PointerEventKind::Leave { .. } => {
                if Self::dash_set_hover(state, None) {
                    self.overview_dash_changed();
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
                    state.dash_feedback.press(app.clone(), Instant::now());
                    state.dash_press = Some((app, (x, y), window));
                    self.overview_dash_changed();
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
                    state.dash_feedback.release(Instant::now());
                    if state.dash.iter().any(|d| d.1 == app && d.0.contains(x, y)) {
                        self.dash_activate(&app, window);
                    } else {
                        self.overview_dash_changed();
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
                self.overview_move(id, target as u32);
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

    /// Move `window` to workspace `to` (the keyboard's moves and the
    /// pictures' accessibility actions), and say so.
    fn overview_move(&mut self, window: u32, to: u32) {
        let Some(list) = self.window_list.as_ref().filter(|l| l.version() >= 2) else { return };
        if to >= self.windows.count.max(1) {
            return;
        }
        list.move_window(window, to);
        let title = self.windows.mru().iter().find(|e| e.id == window).map(|e| e.label().to_owned()).unwrap_or_default();
        let workspace = super::workspace_popup::workspace_name(&super::workspace_popup::read_names(), to);
        if let Some(overview) = self.overview.as_mut() {
            overview.state.moved = Some(helix_i18n::trf!("{window} moved to {workspace}", window = title, workspace = workspace));
        }
        self.draw_overview();
    }

    /// An accessibility action on the overview: a window picture's "Move
    /// to …" (target `window:ID`). False if `action` is no such action.
    pub(super) fn overview_action(&mut self, target: &str, action: i32) -> bool {
        let Some(window) = target.strip_prefix("window:").and_then(|id| id.parse::<u32>().ok()) else { return false };
        let Some(to) = u32::try_from(action.wrapping_sub(MOVE_TO_WORKSPACE)).ok().filter(|n| *n < MOVE_ACTIONS) else { return false };
        if self.overview.is_none() {
            return false;
        }
        self.overview_move(window, to);
        true
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
    /// The workspaces a window picture can be moved to (number, name):
    /// its "Move to …" actions.
    pub move_to: &'a [(u32, String)],
    /// The last move, said.
    pub moved: Option<&'a str>,
}

/// The overview on the accessibility bus (helixdesktop/spec
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
        let node = Node::new(Role::ListBoxOption, title).id(format!("window:{id}")).describe(app.as_str()).at((*r).into()).click().focusable().selected(on).focused(on && keyboard_on(Region::Windows));
        // "Move to Workspace 2", … (helixdesktop/desktop#97): the keyboard's
        // and the drag's moves as actions.
        s.move_to.iter().fold(node, |node, (n, name)| node.action(MOVE_TO_WORKSPACE + *n as i32, helix_i18n::trf!("Move to {workspace}", workspace = name)))
    });
    let windows = Node::new(Role::ListBox, tr("Windows")).id("windows").children(pictures);
    out.push(if s.windows.is_empty() { windows.child(Node::label(tr("No open windows"))) } else { windows });
    if let Some(moved) = s.moved {
        out.push(Node::label(moved).id("moved").live());
    }
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
        let all_names = if state.primary { ws_names } else { super::workspace_popup::read_names() };
        let move_to: Vec<(u32, String)> = (0..self.windows.count.clamp(1, MOVE_ACTIONS)).filter(|n| *n != self.windows.active).map(|n| (n, super::workspace_popup::workspace_name(&all_names, n))).collect();
        overview_nodes(&OverviewShown {
            move_to: &move_to,
            moved: state.moved.as_deref(),
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
                // a while); the request counts as on its way until then.
                let window = data.window;
                let id = frame.id();
                // Given up on already: nothing to read.
                let Some(asked) = ui.overview_shots.requested.get(&window).filter(|asked| asked.frame.id() == id) else { return };
                match asked.file.try_clone() {
                    Ok(mut file) => ui.offload(
                        move || {
                            let mut bytes = Vec::new();
                            let read = file.seek(SeekFrom::Start(0)).is_ok() && file.read_to_end(&mut bytes).is_ok();
                            read.then(|| crate::icons::decode(&bytes, 0)).flatten()
                        },
                        move |ui, image| ui.window_shot(window, &id, image),
                    ),
                    Err(_) => ui.window_shot(window, &id, None),
                }
            }
            helix_screenshot_frame_v1::Event::Failed { .. } => {
                frame.destroy();
                ui.window_shot(data.window, &frame.id(), None);
            }
            _ => {}
        }
    }
}

impl Ui {
    /// Window `window`'s picture came (or not), for request `frame`.
    fn window_shot(&mut self, window: u32, frame: &ObjectId, image: Option<helix_draw::Image>) {
        // Unless that request was given up on and another made since.
        if self.overview_shots.requested.get(&window).is_some_and(|asked| asked.frame.id() == *frame) {
            self.overview_shots.requested.remove(&window);
        }
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
