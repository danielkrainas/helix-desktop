//! The dock on the desktop (helixdesktop/spec `contracts/wayland/README.md`,
//! "Dock"): favorites and running apps, drives, the trash and Show Apps,
//! on a screen edge, hiding as its settings say.

use std::time::Duration;

use smithay_client_toolkit::compositor::Region;
use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay_client_toolkit::reexports::calloop::RegistrationToken;
use smithay_client_toolkit::reexports::client::protocol::wl_output;

use super::*;
use crate::dock::{self as logic, DockSettings, Edge, Item, Outcome};
use crate::windows::{WindowAction, ALL_WORKSPACES};
use helix_i18n::tr;

const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;
/// Room beside the bar for an item's name.
/// Room beside the bar for the tooltip (a name and up to six lines).
const TOOLTIP_ROOM_H: i32 = 150;
const TOOLTIP_ROOM_W: i32 = 280;
/// How far from the bar a dropped favorite is removed.
const UNPIN_DISTANCE: f64 = 60.0;
const DOCK_BG: u32 = 0x001E1E24;
const BADGE: u32 = 0xFFE01B24;
/// The dot on an app asking for attention.
const URGENT: u32 = 0xFFF57900;
/// How long the pointer rests on an item before its tooltip shows, and
/// how soon after one hid the next shows at once (moving along the dock).
pub(super) const TOOLTIP_DELAY: Duration = Duration::from_millis(300);
const TOOLTIP_FOLLOW: Duration = Duration::from_millis(500);
/// How long a dock revealed at the edge waits for the pointer.
const REVEAL_GRACE: f64 = 1.5;
/// How deep the edge strip that reveals a hidden dock is (logical).
const EDGE_STRIP: u32 = 4;
/// With `require-pressure-to-show`, how long a pointer resting on the
/// strip waits before the dock shows anyway (at least `show-delay`).
const EDGE_REST: f64 = 0.5;

/// Something a dock menu entry does.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum DockTarget {
    Window(u32),
    Launch(String),
    Action(String, String),
    TogglePin(String),
    Details(String),
    Quit(String),
    OpenMount(String),
    EjectMount(String),
    OpenTrash,
    EmptyTrash,
    /// An entry of the app's quicklist.
    Quick(crate::launcher_entry::Quicklist, i32),
    /// Close a window (its preview's close button).
    CloseWindow(u32),
    /// Show the app's window previews.
    Previews(String),
    /// Forget the app grid's recent apps (its "Recent" row's menus).
    ClearRecent,
}

/// One monitor's dock.
#[derive(Default)]
pub(super) struct DockView {
    layer: Option<LayerSurface>,
    width: u32,
    height: u32,
    configured: bool,
    /// The strip at the screen edge that reveals a hidden dock, and its
    /// size.
    trigger: Option<LayerSurface>,
    trigger_size: Option<(u32, u32)>,
    /// The monitor it is on: output, area (layout coordinates).
    output: Option<wl_output::WlOutput>,
    area: (i32, i32, i32, i32),
    items: Vec<Item>,
    /// Item size and the bar's thickness (logical).
    icon: i32,
    thickness: i32,
    /// The bar, in surface coordinates.
    bar: Rect,
    hits: Vec<(Rect, usize)>,
    hover: Option<usize>,
    /// The pointer is on the dock.
    pointer_on: bool,
    /// Revealed from the screen edge (until the pointer leaves).
    revealed: bool,
    timer: Option<RegistrationToken>,
    /// How far (pixels) the bar is pushed off its edge: 0 when all in.
    offset: f64,
    /// Sliding: since when, from and to which offset, and the ticking timer.
    slide: Option<(std::time::Instant, f64, f64)>,
    slide_timer: Option<RegistrationToken>,
    press: Option<(usize, (f64, f64), u32)>,
    drag: Option<(usize, (f64, f64))>,
    /// Hover, press and launch feedback on the items.
    feedback: crate::item_feedback::Feedback<Item>,
    /// When the hovered item's tooltip shows; the item whose tooltip was
    /// drawn last, and when the last one went away.
    tooltip_since: Option<std::time::Instant>,
    tooltip_shown: Option<usize>,
    tooltip_gone: Option<std::time::Instant>,
    tooltip_timer: Option<RegistrationToken>,
    /// The item with the keyboard focus (switch-panels).
    pub(super) keyboard: Option<usize>,
    /// What the items showed when last drawn (for the accessibility bus).
    states: Vec<logic::ItemState>,
}

#[derive(Default)]
pub(super) struct DockState {
    pub settings: DockSettings,
    pub mounts: Vec<crate::media::MountInfo>,
    pub trash_full: bool,
    /// A dock per monitor it is on.
    views: Vec<DockView>,
    /// The view being worked on.
    cur: usize,
    /// What the open dock menu's entries do.
    pub menu_targets: Vec<DockTarget>,
    /// Apps' launcher entries, by lower-case app id.
    pub entries: std::collections::HashMap<String, crate::launcher_entry::Entry>,
    /// The view whose edge strip the pointer is on, and how hard it pushes.
    pub at_edge: Option<usize>,
    pub pressure: logic::Pressure,
    /// Numbers are drawn on the apps' icons (after a hot key).
    numbers: bool,
    numbers_timer: Option<RegistrationToken>,
    /// Redraws while item feedback moves.
    feedback_timer: Option<RegistrationToken>,
    /// The next menu opens over the launcher (instead of closing it).
    pub menu_over_launcher: bool,
    /// The open menu is an app grid item's.
    pub menu_from_grid: bool,
    /// Every mount and volume the media side reported (`mounts` is what
    /// the settings let the dock show).
    pub all_mounts: Vec<crate::media::MountInfo>,
    /// Urgent apps' icons wiggling, until when (by lower-case app id).
    dancing: std::collections::HashMap<String, std::time::Instant>,
    dance_timer: Option<RegistrationToken>,
}

/// How long an app that turned urgent wiggles, and how far.
const DANCE: Duration = Duration::from_millis(1600);
const DANCE_PX: f64 = 4.0;
/// How long the dock shows for an app that turned urgent.
const URGENT_SHOW: f64 = 2.0;

thread_local! {
    static DESKTOP_IDS: std::cell::RefCell<std::collections::HashMap<String, Option<String>>> = Default::default();
}

/// The installed desktop file id of app `id` (matching case-insensitively,
/// or by `StartupWMClass`); remembered.
pub(super) fn desktop_id(id: &str) -> Option<String> {
    if let Some(known) = DESKTOP_IDS.with(|c| c.borrow().get(id).cloned()) {
        return known;
    }
    let found = find_desktop_id(id);
    DESKTOP_IDS.with(|c| c.borrow_mut().insert(id.to_owned(), found.clone()));
    found
}

/// Forget remembered desktop ids (apps may have been installed).
pub(super) fn forget_desktop_ids() {
    DESKTOP_IDS.with(|c| c.borrow_mut().clear());
}

fn find_desktop_id(id: &str) -> Option<String> {
    if id.is_empty() {
        return None;
    }
    let apps = crate::icons::apps();
    let ids = &apps.ids;
    ids.iter()
        .find(|d| d.strip_suffix(".desktop").unwrap_or(d).eq_ignore_ascii_case(id))
        .cloned()
        .or_else(|| ids.iter().find(|d| crate::dock::desktop_key(d, "StartupWMClass").is_some_and(|c| c.eq_ignore_ascii_case(id))).cloned())
}

impl DockState {
    pub(super) fn output_is(&self, output: &wl_output::WlOutput) -> bool {
        self.views.iter().any(|v| v.output.as_ref() == Some(output))
    }

    /// The view on `output`, if any.
    fn view_on(&self, output: &wl_output::WlOutput) -> Option<usize> {
        self.views.iter().position(|v| v.output.as_ref() == Some(output))
    }
}

impl Ui {
    /// Forget where the pointer was (the dock is about to move).
    pub(super) fn dock_reset_pointer(&mut self) {
        for view in &mut self.dock.views {
            view.pointer_on = false;
            view.revealed = false;
            view.hover = None;
            view.feedback.reset();
        }
    }

    /// A monitor with a dock went away.
    pub(super) fn dock_forget_output(&mut self) {
        let outputs: Vec<_> = self.output_state.outputs().collect();
        self.dock.views.retain(|v| v.output.as_ref().is_some_and(|o| outputs.contains(o)));
        self.dock.cur = 0;
    }

    /// The monitors that get a dock: every one with `multi-monitor`, else
    /// the chosen one.
    fn dock_monitors(&self) -> Vec<(wl_output::WlOutput, (i32, i32, i32, i32))> {
        if !self.dock.settings.multi_monitor {
            return self.dock_monitor().into_iter().collect();
        }
        self.output_state
            .outputs()
            .filter_map(|o| {
                let info = self.output_state.info(&o)?;
                let ((x, y), (w, h)) = (info.logical_position?, info.logical_size?);
                Some((o, (x, y, w, h)))
            })
            .collect()
    }

    /// Whether window `id` is on the current view's monitor (its middle).
    fn on_view_monitor(&self, id: u32) -> bool {
        let (ax, ay, aw, ah) = self.dock.views.get(self.dock.cur).map_or((0, 0, 0, 0), |v| v.area);
        self.window_geometry.get(&id).is_some_and(|&(x, y, w, h, _)| {
            let (cx, cy) = (x + w / 2, y + h / 2);
            cx >= ax && cy >= ay && cx < ax + aw && cy < ay + ah
        })
    }

    fn dock_monitor(&self) -> Option<(wl_output::WlOutput, (i32, i32, i32, i32))> {
        let want = &self.dock.settings.monitor;
        let infos: Vec<_> = self
            .output_state
            .outputs()
            .filter_map(|o| {
                let info = self.output_state.info(&o)?;
                let ((x, y), (w, h)) = (info.logical_position?, info.logical_size?);
                Some((o, info.name.clone().unwrap_or_default(), (x, y, w, h)))
            })
            .collect();
        let primary = |a: &(i32, i32, i32, i32)| (a.0, a.1) == (self.primary_area.0, self.primary_area.1);
        let index = self.dock.settings.monitor_index;
        if index >= 0 {
            if let Some((o, _, a)) = infos.get(index as usize) {
                return Some((o.clone(), *a));
            }
        }
        infos
            .iter()
            .find(|(_, name, _)| index == -2 && want != "primary" && name == want)
            .or_else(|| infos.iter().find(|(_, _, a)| primary(a)))
            .or_else(|| infos.first())
            .map(|(o, _, a)| (o.clone(), *a))
    }

    /// Windows of the current workspace on the dock's monitor.
    fn dock_windows(&self) -> Vec<logic::WindowArea> {
        let active = self.windows.active;
        self.windows
            .mru()
            .iter()
            .filter(|e| e.workspace == active || e.workspace == ALL_WORKSPACES)
            .filter_map(|e| {
                let &(x, y, w, h, minimized) = self.window_geometry.get(&e.id)?;
                Some(logic::WindowArea { rect: (x, y, w, h), app: e.app_id.clone(), minimized })
            })
            .collect()
    }

    /// With `isolate-locations`, the drive or Trash item file manager
    /// window `e` belongs to.
    fn window_location(&self, e: &crate::windows::WindowEntry) -> Option<Item> {
        if !self.dock.settings.isolate_locations {
            return None;
        }
        let fm = file_manager()?;
        if !(e.app_id.eq_ignore_ascii_case(fm) || desktop_matches(&e.app_id, fm)) {
            return None;
        }
        let names: Vec<&str> = self.dock.mounts.iter().map(|m| m.name.as_str()).collect();
        crate::dock::window_location(&e.title, &names, trash_title())
    }

    /// The windows of drive or Trash item `item`, most recently used first.
    fn location_windows(&self, item: &Item) -> Vec<u32> {
        let isolate = self.dock.settings.isolate_workspaces;
        self.windows
            .mru()
            .iter()
            .filter(|e| self.window_location(e).as_ref() == Some(item))
            .filter(|e| self.counts_urgent(e.id) || !isolate || e.workspace == self.windows.active || e.workspace == ALL_WORKSPACES)
            .filter(|e| !self.dock.settings.isolate_monitors || self.on_view_monitor(e.id))
            .map(|e| e.id)
            .collect()
    }

    /// Running apps in the order they first opened (window ids grow).
    fn dock_running(&self) -> Vec<String> {
        let mut entries: Vec<_> = self
            .windows
            .mru()
            .iter()
            .filter(|e| self.counts_urgent(e.id) || !self.dock.settings.isolate_workspaces || e.workspace == self.windows.active || e.workspace == ALL_WORKSPACES)
            .filter(|e| !self.dock.settings.isolate_monitors || self.on_view_monitor(e.id))
            .filter(|e| self.window_location(e).is_none())
            .collect();
        entries.sort_by_key(|e| e.id);
        entries.into_iter().map(|e| e.app_id.clone()).collect()
    }

    /// An app's windows, most recently used first.
    fn app_windows(&self, app: &str) -> Vec<u32> {
        let isolate = self.dock.settings.isolate_workspaces;
        self.windows
            .mru()
            .iter()
            .filter(|e| e.app_id.eq_ignore_ascii_case(app) || desktop_matches(&e.app_id, app))
            .filter(|e| self.counts_urgent(e.id) || !isolate || e.workspace == self.windows.active || e.workspace == ALL_WORKSPACES)
            .filter(|e| !self.dock.settings.isolate_monitors || self.on_view_monitor(e.id))
            .filter(|e| self.window_location(e).is_none())
            .map(|e| e.id)
            .collect()
    }

    /// Window `id` wants attention and counts on every workspace.
    fn counts_urgent(&self, id: u32) -> bool {
        self.dock.settings.urgent_any_workspace && self.attention.contains(&id)
    }

    /// Whether app `app` is urgent: its launcher entry says so, or one of
    /// its windows wants attention.
    pub(super) fn app_urgent(&self, app: &str) -> bool {
        self.dock_entry(app).is_some_and(|e| e.urgent) || self.app_windows(app).iter().any(|w| self.attention.contains(w))
    }

    /// Window `id` wants attention (or no longer does): the dock marks its
    /// app, and a notification says it is ready.
    pub(super) fn window_attention(&mut self, id: u32, wanting: bool) {
        let entry = self.windows.mru().iter().find(|e| e.id == id).cloned();
        let app = entry.as_ref().map(|e| e.app_id.clone()).unwrap_or_default();
        let app_key = desktop_id(&app).map_or_else(|| app.clone(), |d| d.trim_end_matches(".desktop").to_owned());
        let was = self.app_urgent(&app_key);
        let changed = if wanting { self.attention.insert(id) } else { self.attention.remove(&id) };
        if !changed {
            return;
        }
        tracing::info!(id, wanting, app, "window attention");
        let _ = self.requests.send(UiRequest::WindowAttention { id, wanting, title: entry.map(|e| e.title).unwrap_or_default(), app_id: app_key.clone() });
        if wanting && !was {
            self.dock_urgent(&app_key);
        }
        self.dock_update();
        self.dock_draw();
    }

    /// The dock's along-the-edge length for `items` at `icon` size.
    fn dock_length(icon: i32, items: &[Item]) -> i32 {
        let slot = icon + icon / 4;
        let separators = items.windows(2).filter(|p| separated(&p[0], &p[1])).count() as i32;
        slot * items.len() as i32 + separators * 9 + icon / 4
    }

    /// Recompute what the docks show and whether they are up.
    pub(super) fn dock_update(&mut self) {
        let s = self.dock.settings.clone();
        let blocked = !s.enabled || self.lock.session.is_some() || self.overview_shown() || self.launcher.is_some() || self.window_list.is_none();
        let monitors = if blocked { Vec::new() } else { self.dock_monitors() };
        // A view per monitor: those of monitors gone (or not wanted) go.
        self.dock.views.retain(|v| v.output.as_ref().is_some_and(|o| monitors.iter().any(|(m, _)| m == o)));
        for (output, _) in &monitors {
            if self.dock.view_on(output).is_none() {
                self.dock.views.push(DockView { output: Some(output.clone()), ..DockView::default() });
            }
        }
        for (output, area) in monitors {
            if let Some(i) = self.dock.view_on(&output) {
                self.dock.cur = i;
                self.dock_update_view(output, area);
            }
        }
        self.dock.cur = 0;
    }

    /// Update the current view, on `output` at `area`.
    fn dock_update_view(&mut self, output: wl_output::WlOutput, area: (i32, i32, i32, i32)) {
        let s = self.dock.settings.clone();
        let installed = crate::icons::apps().shown.clone();
        let running: Vec<String> = self.dock_running().into_iter().map(|a| desktop_id(&a).map_or(a, |d| d.trim_end_matches(".desktop").to_owned())).collect();
        let items = logic::items(&s, &crate::favorites::favorites(), &installed, &running, self.dock.mounts.len(), if self.lock.session.is_some() { 0 } else { self.ext.dock.len() });
        let room = logic::room(&s, if s.edge.vertical() { area.3 } else { area.2 }, panel_height() as i32);
        let icon = logic::fit_icon_size(s.icon_size, s.icon_size_fixed, items.len(), room);
        let thickness = icon + icon / 3 + 8;
        let length = if s.extend { if s.edge.vertical() { area.3 } else { area.2 } } else { Self::dock_length(icon, &items).min(room) };
        // The bar in layout coordinates (for windows over it).
        let (ax, ay, aw, ah) = area;
        let rect = match s.edge {
            Edge::Bottom => (ax + (aw - length) / 2, ay + ah - thickness, length, thickness),
            Edge::Top => (ax + (aw - length) / 2, ay + panel_height() as i32, length, thickness),
            Edge::Left => (ax, ay + (ah - length) / 2, thickness, length),
            Edge::Right => (ax + aw - thickness, ay + (ah - length) / 2, thickness, length),
        };
        let windows = self.dock_windows();
        let focused_app = self.windows.mru().iter().find(|e| e.id == self.windows.focused()).map(|e| e.app_id.clone());
        let overlapped = logic::windows_hide(s.hide_mode, rect, area, s.edge, &windows, focused_app.as_deref());
        let fullscreen = self.window_geometry.get(&self.windows.focused()).is_some_and(|&(x, y, w, h, min)| !min && logic::covers((x, y, w, h), area));
        let view = &self.dock.views[self.dock.cur];
        let hovered = view.revealed || view.pointer_on || view.drag.is_some() || view.keyboard.is_some();
        let shown = logic::shown(&s, overlapped, fullscreen, hovered);
        tracing::debug!(?rect, overlapped, fullscreen, hovered, shown, windows = windows.len(), mode = ?s.hide_mode, "dock");
        let changed = self.dock.views[self.dock.cur].items != items || self.dock.views[self.dock.cur].icon != icon || self.dock.views[self.dock.cur].output.as_ref() != Some(&output);
        self.dock.views[self.dock.cur].items = items;
        self.dock.views[self.dock.cur].icon = icon;
        self.dock.views[self.dock.cur].thickness = thickness;
        self.dock.views[self.dock.cur].area = area;
        if !shown {
            let may_reveal = if fullscreen { s.autohide_in_fullscreen } else { s.autohide && !s.fixed };
            self.dock_hide(may_reveal);
            self.dock.views[self.dock.cur].output = Some(output);
            return;
        }
        self.dock_drop_trigger();
        let (sw, sh) = match s.edge {
            Edge::Bottom | Edge::Top => (if s.extend { 0 } else { length as u32 }, (thickness + TOOLTIP_ROOM_H) as u32),
            Edge::Left | Edge::Right => ((thickness + TOOLTIP_ROOM_W) as u32, if s.extend { 0 } else { length as u32 }),
        };
        if self.dock.views[self.dock.cur].layer.is_some() && (changed || self.dock.views[self.dock.cur].output.as_ref() != Some(&output)) && self.dock.views[self.dock.cur].output.as_ref() != Some(&output) {
            self.dock.views[self.dock.cur].layer = None;
        }
        self.dock.views[self.dock.cur].output = Some(output.clone());
        let anchor = match (s.edge, s.extend) {
            (Edge::Bottom, false) => Anchor::BOTTOM,
            (Edge::Bottom, true) => Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
            (Edge::Top, false) => Anchor::TOP,
            (Edge::Top, true) => Anchor::TOP | Anchor::LEFT | Anchor::RIGHT,
            (Edge::Left, false) => Anchor::LEFT,
            (Edge::Left, true) => Anchor::LEFT | Anchor::TOP | Anchor::BOTTOM,
            (Edge::Right, false) => Anchor::RIGHT,
            (Edge::Right, true) => Anchor::RIGHT | Anchor::TOP | Anchor::BOTTOM,
        };
        let zone = if s.fixed { thickness } else { 0 };
        match self.dock.views[self.dock.cur].layer.as_ref() {
            Some(layer) => {
                layer.set_anchor(anchor);
                layer.set_size(sw, sh);
                layer.set_exclusive_zone(zone);
                self.dock_set_margin();
                layer.commit();
                self.dock_slide_to(0.0);
                if self.dock.views[self.dock.cur].configured && (sw == 0 || sw == self.dock.views[self.dock.cur].width) && (sh == 0 || sh == self.dock.views[self.dock.cur].height) {
                    self.dock_draw();
                }
            }
            None => {
                let surface = self.compositor.create_surface(&self.qh);
                let layer = self.layer_shell.create_layer_surface(&self.qh, surface, Layer::Top, Some("helix-dock"), Some(&output));
                layer.set_anchor(anchor);
                layer.set_size(sw, sh);
                layer.set_exclusive_zone(zone);
                layer.set_keyboard_interactivity(KeyboardInteractivity::None);
                self.dock.views[self.dock.cur].layer = Some(layer);
                self.dock.views[self.dock.cur].configured = false;
                // It slides in from beyond the edge (not with reduced motion).
                self.dock.views[self.dock.cur].offset = if s.animation_time > 0.0 && !s.reduced_motion { f64::from(thickness) } else { 0.0 };
                self.dock.views[self.dock.cur].slide = None;
                self.dock_set_margin();
                if let Some(layer) = self.dock.views[self.dock.cur].layer.as_ref() {
                    layer.commit();
                }
                self.dock_slide_to(0.0);
            }
        }
    }

    /// Take the dock down; with `trigger`, leave the edge strip that
    /// reveals it.
    fn dock_hide(&mut self, trigger: bool) {
        if self.dock.views[self.dock.cur].layer.is_some() && self.dock.views[self.dock.cur].configured {
            // It slides out, then goes.
            let thickness = f64::from(self.dock.views[self.dock.cur].thickness);
            self.dock_slide_to(thickness);
        } else {
            self.dock_drop_layer();
        }
        self.dock.views[self.dock.cur].hover = None;
        self.dock.views[self.dock.cur].pointer_on = false;
        if !trigger {
            self.dock_drop_trigger();
            return;
        }
        if self.dock.views[self.dock.cur].trigger.is_some() {
            return;
        }
        let Some(output) = self.dock.views[self.dock.cur].output.clone() else { return };
        let anchor = match self.dock.settings.edge {
            Edge::Bottom => Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
            Edge::Top => Anchor::TOP | Anchor::LEFT | Anchor::RIGHT,
            Edge::Left => Anchor::LEFT | Anchor::TOP | Anchor::BOTTOM,
            Edge::Right => Anchor::RIGHT | Anchor::TOP | Anchor::BOTTOM,
        };
        let (w, h) = if self.dock.settings.edge.vertical() { (EDGE_STRIP, 0) } else { (0, EDGE_STRIP) };
        let surface = self.compositor.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(&self.qh, surface, Layer::Top, Some("helix-dock-edge"), Some(&output));
        layer.set_anchor(anchor);
        layer.set_size(w, h);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.commit();
        self.dock.views[self.dock.cur].trigger = Some(layer);
        self.dock.views[self.dock.cur].trigger_size = None;
    }

    fn dock_drop_layer(&mut self) {
        let view = &mut self.dock.views[self.dock.cur];
        view.layer = None;
        view.configured = false;
        view.slide = None;
        view.offset = 0.0;
        if let (Some(handle), Some(token)) = (self.loop_handle.as_ref(), view.slide_timer.take()) {
            handle.remove(token);
        }
    }

    /// Push the current view's surface `offset` pixels off its edge (not
    /// committed).
    fn dock_set_margin(&self) {
        let view = &self.dock.views[self.dock.cur];
        let Some(layer) = view.layer.as_ref() else { return };
        let m = -(view.offset.round() as i32);
        match self.dock.settings.edge {
            Edge::Top => layer.set_margin(m, 0, 0, 0),
            Edge::Right => layer.set_margin(0, m, 0, 0),
            Edge::Bottom => layer.set_margin(0, 0, m, 0),
            Edge::Left => layer.set_margin(0, 0, 0, m),
        }
    }

    /// Slide the current view to `target` (0: all in; its thickness: out,
    /// after which its surface goes), over `animation-time`.
    fn dock_slide_to(&mut self, target: f64) {
        // Reduced motion: shown and hidden at once.
        let total = if self.dock.settings.reduced_motion { 0.0 } else { self.dock.settings.animation_time };
        let view = &mut self.dock.views[self.dock.cur];
        let heading = view.slide.map_or(view.offset, |(_, _, to)| to);
        if heading == target && (view.slide.is_some() || view.offset == target) {
            if view.offset == target && target > 0.0 {
                self.dock_drop_layer();
            }
            return;
        }
        if total <= 0.0 || view.layer.is_none() {
            view.offset = target;
            view.slide = None;
            if target > 0.0 {
                self.dock_drop_layer();
            } else {
                self.dock_set_margin();
            }
            return;
        }
        view.slide = Some((std::time::Instant::now(), view.offset, target));
        if view.slide_timer.is_some() {
            return;
        }
        let Some(handle) = self.loop_handle.as_ref() else { return };
        let output = view.output.clone();
        const TICK: Duration = Duration::from_millis(16);
        let token = handle.insert_source(Timer::from_duration(Duration::ZERO), move |_, _, ui: &mut Ui| {
            let Some(i) = output.as_ref().and_then(|o| ui.dock.view_on(o)) else { return TimeoutAction::Drop };
            let total = ui.dock.settings.animation_time;
            let view = &mut ui.dock.views[i];
            let Some((start, from, to)) = view.slide else {
                view.slide_timer = None;
                return TimeoutAction::Drop;
            };
            let at = logic::slide_offset(from, to, start.elapsed().as_secs_f64(), total);
            view.offset = at.unwrap_or(to);
            ui.dock.cur = i;
            if at.is_none() {
                ui.dock.views[i].slide = None;
                ui.dock.views[i].slide_timer = None;
                if to > 0.0 {
                    ui.dock_drop_layer();
                } else {
                    ui.dock_set_margin();
                    if let Some(layer) = ui.dock.views[i].layer.as_ref() {
                        layer.commit();
                    }
                }
                ui.dock.cur = 0;
                return TimeoutAction::Drop;
            }
            ui.dock_set_margin();
            if let Some(layer) = ui.dock.views[i].layer.as_ref() {
                layer.commit();
            }
            ui.dock.cur = 0;
            TimeoutAction::ToDuration(TICK)
        });
        self.dock.views[self.dock.cur].slide_timer = token.ok();
    }

    fn dock_drop_trigger(&mut self) {
        self.dock.views[self.dock.cur].trigger = None;
    }

    pub(super) fn dock_configured(&mut self, layer: &LayerSurface, size: (u32, u32)) -> bool {
        let Some(i) = self.dock.views.iter().position(|v| v.layer.as_ref() == Some(layer) || v.trigger.as_ref() == Some(layer)) else { return false };
        self.dock.cur = i;
        if self.dock.views[self.dock.cur].trigger.as_ref() == Some(layer) {
            self.dock.views[self.dock.cur].trigger_size = Some(size);
            self.dock_draw_trigger(self.dock.cur);
            return true;
        }
        if self.dock.views[self.dock.cur].layer.as_ref() != Some(layer) {
            return false;
        }
        self.dock.views[self.dock.cur].width = size.0;
        self.dock.views[self.dock.cur].height = size.1;
        self.dock.views[self.dock.cur].configured = true;
        self.dock_draw();
        true
    }

    pub(super) fn dock_closed(&mut self, layer: &LayerSurface) -> bool {
        let Some(i) = self.dock.views.iter().position(|v| v.layer.as_ref() == Some(layer) || v.trigger.as_ref() == Some(layer)) else { return false };
        self.dock.cur = i;
        if self.dock.views[self.dock.cur].layer.as_ref() == Some(layer) {
            self.dock.views[self.dock.cur].layer = None;
            self.dock.views[self.dock.cur].configured = false;
            return true;
        }
        if self.dock.views[self.dock.cur].trigger.as_ref() == Some(layer) {
            self.dock.views[self.dock.cur].trigger = None;
            return true;
        }
        false
    }

    /// The name shown for item `i`.
    fn dock_item_name(&self, item: &Item) -> String {
        match item {
            Item::App { id, .. } => crate::windows::app_names().get(&id.to_lowercase()).cloned().unwrap_or_else(|| id.clone()),
            Item::Mount(i) => self.dock.mounts.get(*i).map_or_else(String::new, |m| m.name.clone()),
            Item::Ext(i) => self.ext.dock.get(*i).map_or_else(String::new, |d| if d.label.is_empty() { d.name.clone() } else { d.label.clone() }),
            Item::Trash => tr("Trash").into(),
            Item::ShowApps => tr("Show Apps").into(),
        }
    }

    fn dock_icon(&mut self, item: &Item, px: u32) -> Option<helix_draw::Image> {
        if let Item::Ext(i) = item {
            let d = self.ext.dock.get(*i)?.clone();
            let rgb = [(FG >> 16) as u8, (FG >> 8) as u8, FG as u8];
            let image = if d.icon_data.is_empty() { super::ext_items::ext_named_icon(&mut self.icon_cache, &d.icon, &d.dir, px, rgb) } else { super::ext_items::ext_data_icon(&mut self.icon_cache, &d.icon_data, px) };
            // Dimmed while its extension is not running.
            return image.map(|mut im| {
                if !d.running {
                    // Premultiplied: every channel halves.
                    for c in &mut im.rgba {
                        *c /= 2;
                    }
                }
                im
            });
        }
        let (key, source) = match item {
            Item::App { id, .. } => (format!("app:{id}@{px}"), None),
            Item::Mount(i) => {
                let names = self.dock.mounts.get(*i)?.icons.clone();
                (format!("mount:{}@{px}", names.join(",")), Some(crate::icons::IconSource::Names(names)))
            }
            Item::Trash => {
                let name = if self.dock.trash_full { "user-trash-full" } else { "user-trash" };
                (format!("named:{name}@{px}"), Some(crate::icons::IconSource::Names(vec![name.into()])))
            }
            Item::ShowApps | Item::Ext(_) => return None,
        };
        let id = match item {
            Item::App { id, .. } => Some(id.clone()),
            _ => None,
        };
        self.icon_cache.get(key, move || match id {
            Some(id) => crate::icons::app_icon(&desktop_id(&id).map_or(id, |d| d.trim_end_matches(".desktop").to_owned()), px),
            None => source.and_then(|s| crate::icons::load(&s, px)),
        })
    }

    /// App `id`'s launcher entry (by its id or desktop file).
    fn dock_entry(&self, id: &str) -> Option<&crate::launcher_entry::Entry> {
        self.dock.entries.get(&id.to_lowercase()).or_else(|| {
            let desktop = desktop_id(id)?;
            self.dock.entries.get(&desktop.trim_end_matches(".desktop").to_lowercase())
        })
    }

    /// Unread notifications of app `id`.
    fn dock_badge(&self, id: &str) -> usize {
        self.notify.list.entries.iter().filter(|e| e.app_id.eq_ignore_ascii_case(id) || desktop_matches(&e.app_id, id)).count()
    }

    /// An empty, see-through strip that only takes the pointer.
    fn dock_draw_trigger(&mut self, index: usize) {
        let Some(layer) = self.dock.views[index].trigger.clone() else { return };
        let Some(size) = self.dock.views[index].trigger_size else { return };
        let (w, h) = (size.0.max(1) as i32, size.1.max(1) as i32);
        let scale = self.scales.of(layer.wl_surface());
        if let Ok((buffer, data)) = self.buffers.create_buffer(layer.wl_surface(), Ui::dock_draw_triggers, physical(w as usize, scale) as i32, physical(h as usize, scale) as i32, (physical(w as usize, scale) * 4) as i32, wl_shm::Format::Argb8888) {
            data.fill(0);
            self.scales.prepare(&self.qh, layer.wl_surface(), w, h);
            if buffer.attach_to(layer.wl_surface()).is_ok() {
                layer.commit();
            }
        }
    }

    fn dock_draw_triggers(&mut self) {
        for index in 0..self.dock.views.len() {
            self.dock_draw_trigger(index);
        }
    }

    /// Draw every dock.
    pub(super) fn dock_draw(&mut self) {
        let _busy = crate::watchdog::scope("draw dock");
        for i in 0..self.dock.views.len() {
            self.dock.cur = i;
            self.dock_draw_view();
        }
        self.dock.cur = 0;
        // Item feedback still moving: draw again next frame.
        let now = std::time::Instant::now();
        let moving = self.dock.views.iter_mut().fold(false, |any, v| v.feedback.moving(now) | any);
        if moving && self.dock.feedback_timer.is_none() {
            if let Some(handle) = self.loop_handle.as_ref() {
                let token = handle.insert_source(Timer::from_duration(Duration::from_millis(16)), |_, _, ui: &mut Ui| {
                    ui.dock.feedback_timer = None;
                    ui.dock_draw();
                    TimeoutAction::Drop
                });
                self.dock.feedback_timer = token.ok();
            }
        }
    }

    /// The item at `index` of the current view, for feedback.
    fn dock_feedback_item(&self, index: Option<usize>) -> Option<Item> {
        index.and_then(|i| self.dock.views[self.dock.cur].items.get(i).cloned())
    }

    fn dock_draw_view(&mut self) {
        let Some(layer) = self.dock.views[self.dock.cur].layer.clone() else { return };
        if !self.dock.views[self.dock.cur].configured || self.dock.views[self.dock.cur].width == 0 || self.dock.views[self.dock.cur].height == 0 {
            return;
        }
        let (w, h) = (self.dock.views[self.dock.cur].width as i32, self.dock.views[self.dock.cur].height as i32);
        let scale = self.scales.of(layer.wl_surface());
        let paint = self.dock_paint_model(scale);
        let Ok((buffer, data)) = self.buffers.create_buffer(layer.wl_surface(), Ui::dock_draw, physical(w as usize, scale) as i32, physical(h as usize, scale) as i32, (physical(w as usize, scale) * 4) as i32, wl_shm::Format::Argb8888) else {
            return;
        };
        let mut canvas = Canvas::scaled(data, w as usize, h as usize, scale).mirrored(super::mirror::on(layer.wl_surface()));
        canvas.fill(0);
        let painted = paint_dock(&mut canvas, self.text.as_ref(), &paint);
        self.dock.views[self.dock.cur].states = (0..paint.items.len())
            .map(|i| logic::ItemState {
                windows: paint.counts[i].0,
                badge: paint.counts[i].1,
                progress: paint.progress[i],
                urgent: paint.urgent[i],
                focused: paint.focused_item == Some(i),
            })
            .collect();
        let bar = painted.bar;
        self.dock.views[self.dock.cur].bar = bar;
        self.dock.views[self.dock.cur].hits = painted.hits;
        let surface = layer.wl_surface();
        // Only the bar takes the pointer; names float over windows.
        if let Ok(region) = Region::new(&self.compositor) {
            region.add(bar.x, bar.y, bar.w, bar.h);
            surface.set_input_region(Some(region.wl_region()));
        }
        self.scales.prepare(&self.qh, surface, w, h);
        if buffer.attach_to(surface).is_ok() {
            layer.commit();
        }
    }

    /// What the current dock view shows now, gathered from the shell's
    /// state.
    fn dock_paint_model(&mut self, scale: f64) -> DockPaint {
        let view = &self.dock.views[self.dock.cur];
        let (w, h) = (view.width as i32, view.height as i32);
        let s = self.dock.settings.clone();
        let (icon, t) = (view.icon, view.thickness);
        let near = logic::windows_hide(logic::HideMode::AllWindows, self.dock_layout_rect(), view.area, s.edge, &self.dock_windows(), None);
        let items = view.items.clone();
        let px = (f64::from(icon) * scale).round() as u32;
        let icons: Vec<Option<helix_draw::Image>> = items.iter().map(|i| self.dock_icon(i, px)).collect();
        let counts: Vec<(usize, usize)> = items
            .iter()
            .map(|i| match i {
                Item::App { id, .. } => {
                    // The app's own count, else its notifications.
                    let own = self.dock_entry(id).and_then(|e| e.shown_count()).map(|c| c.max(0) as usize);
                    (self.app_windows(id).len().min(4), own.unwrap_or_else(|| if s.counters { self.dock_badge(id) } else { 0 }))
                }
                Item::Mount(_) | Item::Trash => (self.location_windows(i).len().min(4), 0),
                Item::Ext(e) => self.ext.dock.get(*e).map_or((0, 0), |d| (usize::from(d.dot), d.count as usize)),
                _ => (0, 0),
            })
            .collect();
        let progress: Vec<Option<f64>> = items
            .iter()
            .map(|i| match i {
                Item::App { id, .. } => self.dock_entry(id).and_then(|e| e.shown_progress()),
                // An extension's item: its progress as an app's launcher
                // entry shows it.
                Item::Ext(e) => self.ext.dock.get(*e).filter(|d| d.progress >= 0.0).map(|d| d.progress.clamp(0.0, 1.0)),
                _ => None,
            })
            .collect();
        let urgent: Vec<bool> = items.iter().map(|i| matches!(i, Item::App { id, .. } if self.app_urgent(id)) || matches!(i, Item::Ext(e) if self.ext.dock.get(*e).is_some_and(|d| d.urgent))).collect();
        // Urgent apps' wiggles.
        let wiggles: Vec<i32> = items
            .iter()
            .map(|i| match i {
                Item::App { id, .. } => self.dance_offset(id).round() as i32,
                _ => 0,
            })
            .collect();
        // Hot key numbers for the first ten apps.
        let numbers: Vec<Option<usize>> = {
            let mut n = 0;
            items
                .iter()
                .map(|i| match i {
                    Item::App { .. } if self.dock.numbers && n < 10 => {
                        n += 1;
                        Some(n)
                    }
                    _ => None,
                })
                .collect()
        };
        let focused_app = self.windows.mru().iter().find(|e| e.id == self.windows.focused()).map(|e| e.app_id.clone()).unwrap_or_default();
        // A file manager window showing a drive or the Trash marks that item.
        let focused_location = self.windows.mru().iter().find(|e| e.id == self.windows.focused()).and_then(|e| self.window_location(e));
        let focused_item = match focused_location {
            Some(place) => items.iter().position(|i| *i == place),
            None => items.iter().position(|i| matches!(i, Item::App { id, .. } if !focused_app.is_empty() && (id.eq_ignore_ascii_case(&focused_app) || desktop_matches(&focused_app, id)))),
        };
        let view = &self.dock.views[self.dock.cur];
        let (hover, drag, keyboard) = (view.hover, view.drag, view.keyboard);
        // The name, and for a running app its windows' titles: under the
        // pointer once the tooltip delay is over, or for the keyboard.
        let now = std::time::Instant::now();
        let tooltip_for = match (hover, keyboard) {
            (Some(i), _) if view.tooltip_since.is_some_and(|t| now >= t) => Some(i),
            (None, Some(i)) => Some(i),
            _ => None,
        };
        let tooltip = tooltip_for.and_then(|i| Some((i, items.get(i)?))).filter(|_| !s.hide_tooltip && drag.is_none()).map(|(i, item)| {
            let name = self.dock_item_name(item);
            let titles: Vec<String> = match item {
                Item::App { id, .. } => {
                    let ids = self.app_windows(id);
                    ids.iter().filter_map(|w| self.windows.mru().iter().find(|e| e.id == *w).map(|e| e.title.clone())).collect()
                }
                _ => Vec::new(),
            };
            (i, logic::tooltip_lines(&name, &titles))
        });
        let view = &mut self.dock.views[self.dock.cur];
        view.tooltip_shown = tooltip.as_ref().map(|(i, _)| *i);
        let motion = helix_draw::motion::Motion::from_settings(s.animation_time > 0.0, s.reduced_motion);
        view.feedback.set_motion(motion);
        let looks = items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                if drag.is_some() {
                    crate::item_feedback::Look { highlight: if hover == Some(i) { 1.0 } else { 0.0 }, pressed: false, scale: 1.0 }
                } else {
                    view.feedback.look(item, now)
                }
            })
            .collect();
        let ghosts = view.feedback.ghosts(now).into_iter().filter_map(|(item, p)| Some((items.iter().position(|x| *x == item)?, p))).collect();
        DockPaint {
            size: (w, h),
            edge: s.edge,
            extend: s.extend,
            always_center: s.always_center,
            indicator: s.indicator,
            icon,
            thickness: t,
            background: (logic::background_opacity(&s, near) * 255.0).round() as u32,
            items,
            icons,
            counts,
            progress,
            urgent,
            wiggles,
            numbers,
            focused_item,
            looks,
            ghosts,
            drag,
            tooltip,
            keyboard,
        }
    }

    /// The bar in layout coordinates.
    fn dock_layout_rect(&self) -> (i32, i32, i32, i32) {
        let (ax, ay, aw, ah) = self.dock.views[self.dock.cur].area;
        let t = self.dock.views[self.dock.cur].thickness;
        let len = if self.dock.settings.extend {
            if self.dock.settings.edge.vertical() { ah } else { aw }
        } else {
            Self::dock_length(self.dock.views[self.dock.cur].icon, &self.dock.views[self.dock.cur].items)
        };
        match self.dock.settings.edge {
            Edge::Bottom => (ax + (aw - len) / 2, ay + ah - t, len, t),
            Edge::Top => (ax + (aw - len) / 2, ay + panel_height() as i32, len, t),
            Edge::Left => (ax, ay + (ah - len) / 2, t, len),
            Edge::Right => (ax + aw - t, ay + (ah - len) / 2, t, len),
        }
    }

    /// The pointer moved onto item `under` (or off the items): its
    /// tooltip shows after [`TOOLTIP_DELAY`], or at once when another
    /// item's tooltip shows or just went (moving along the dock).
    fn dock_set_hover(&mut self, under: Option<usize>) {
        let now = std::time::Instant::now();
        let cur = self.dock.cur;
        let view = &mut self.dock.views[cur];
        if view.hover == under {
            return;
        }
        if view.tooltip_shown.is_some() {
            view.tooltip_gone = Some(now);
        }
        view.hover = under;
        let (since, follow) = tooltip_from(now, view.tooltip_shown.is_some(), view.tooltip_gone);
        view.tooltip_since = under.map(|_| since);
        let old = view.tooltip_timer.take();
        let Some(handle) = self.loop_handle.as_ref() else { return };
        if let Some(token) = old {
            handle.remove(token);
        }
        if under.is_none() || follow {
            return;
        }
        let output = self.dock.views[cur].output.clone();
        let token = handle.insert_source(Timer::from_duration(TOOLTIP_DELAY), move |_, _, ui: &mut Ui| {
            if let Some(i) = output.as_ref().and_then(|o| ui.dock.view_on(o)) {
                ui.dock.views[i].tooltip_timer = None;
                ui.dock_draw();
            }
            TimeoutAction::Drop
        });
        self.dock.views[cur].tooltip_timer = token.ok();
    }

    fn dock_set_timer(&mut self, after: f64, reveal: bool) {
        if let (Some(handle), Some(token)) = (self.loop_handle.as_ref(), self.dock.views[self.dock.cur].timer.take()) {
            handle.remove(token);
        }
        let Some(handle) = self.loop_handle.as_ref() else { return };
        let output = self.dock.views[self.dock.cur].output.clone();
        let token = handle.insert_source(Timer::from_duration(Duration::from_secs_f64(after)), move |_, _, ui: &mut Ui| {
            let Some(i) = output.as_ref().and_then(|o| ui.dock.view_on(o)) else { return TimeoutAction::Drop };
            ui.dock.cur = i;
            ui.dock.views[i].timer = None;
            if !reveal && ui.dock.views[ui.dock.cur].pointer_on {
                return TimeoutAction::Drop;
            }
            ui.dock.views[ui.dock.cur].revealed = reveal;
            ui.dock_update();
            if reveal {
                // It appears under a pointer that has not moved yet: unless
                // the pointer comes onto it, it goes again.
                if let Some(i) = output.as_ref().and_then(|o| ui.dock.view_on(o)) {
                    ui.dock.cur = i;
                    ui.dock_set_timer(REVEAL_GRACE, false);
                    ui.dock.cur = 0;
                }
            }
            TimeoutAction::Drop
        });
        self.dock.views[self.dock.cur].timer = token.ok();
    }

    fn dock_cancel_timer(&mut self) {
        if let (Some(handle), Some(token)) = (self.loop_handle.as_ref(), self.dock.views[self.dock.cur].timer.take()) {
            handle.remove(token);
        }
    }

    /// Pointer motion, unclamped (`zwp_relative_pointer_v1`): pushing
    /// against the edge strip of a hidden dock shows it.
    pub(super) fn dock_relative_motion(&mut self, delta: (f64, f64), utime: u64) {
        let Some(i) = self.dock.at_edge.filter(|_| self.dock.settings.require_pressure) else { return };
        if i >= self.dock.views.len() || self.dock.views[i].revealed {
            return;
        }
        let (edge, threshold) = (self.dock.settings.edge, self.dock.settings.pressure_threshold);
        if self.dock.pressure.push(edge, delta, utime / 1000, threshold) {
            self.dock.pressure.reset();
            self.dock.cur = i;
            tracing::debug!("dock shown by pressure");
            self.dock_set_timer(0.0, true);
        }
    }

    /// Pointer input on the dock or its edge strip; true if it was for them.
    /// The dock item under `x`, `y` on `surface` (a dock's surface).
    pub(super) fn dock_item_at(&self, surface: &wl_surface::WlSurface, x: f64, y: f64) -> Option<Item> {
        let view = self.dock.views.iter().find(|v| v.layer.as_ref().is_some_and(|l| l.wl_surface() == surface))?;
        let index = view.hits.iter().find(|(r, _)| r.contains(x, y)).map(|(_, i)| *i)?;
        view.items.get(index).cloned()
    }

    pub(super) fn dock_pointer(&mut self, event: &PointerEvent) -> bool {
        let Some(i) = self.dock.views.iter().position(|v| v.layer.as_ref().is_some_and(|l| l.wl_surface() == &event.surface) || v.trigger.as_ref().is_some_and(|t| t.wl_surface() == &event.surface)) else { return false };
        self.dock.cur = i;
        if self.dock.views[self.dock.cur].trigger.as_ref().is_some_and(|t| t.wl_surface() == &event.surface) {
            match event.kind {
                PointerEventKind::Enter { .. } => {
                    self.dock.at_edge = Some(i);
                    self.dock.pressure.reset();
                    // With pressure, pushing against the edge shows it at
                    // once; resting there shows it a little later.
                    let delay = if self.dock.settings.require_pressure {
                        self.dock.settings.show_delay.max(EDGE_REST)
                    } else {
                        self.dock.settings.show_delay
                    };
                    self.dock_set_timer(delay, true);
                }
                PointerEventKind::Leave { .. } => {
                    self.dock.at_edge = None;
                    if !self.dock.views[self.dock.cur].revealed {
                        self.dock_cancel_timer();
                    }
                }
                _ => {}
            }
            return true;
        }
        if !self.dock.views[self.dock.cur].layer.as_ref().is_some_and(|l| l.wl_surface() == &event.surface) {
            return false;
        }
        let (x, y) = event.position;
        let under = self.dock.views[self.dock.cur].hits.iter().find(|(r, _)| r.contains(x, y)).map(|(_, i)| *i);
        match event.kind {
            PointerEventKind::Enter { .. } => {
                self.dock.views[self.dock.cur].pointer_on = true;
                self.dock_cancel_timer();
                self.dock_set_hover(under);
                let item = self.dock_feedback_item(under);
                self.dock.views[self.dock.cur].feedback.set_hover(item, std::time::Instant::now());
                self.dock_draw();
            }
            PointerEventKind::Leave { .. } => {
                self.dock.views[self.dock.cur].pointer_on = false;
                self.dock_set_hover(None);
                self.dock.views[self.dock.cur].feedback.set_hover(None, std::time::Instant::now());
                if self.dock.views[self.dock.cur].drag.is_none() {
                    self.dock_draw();
                    // Hide (if it should) after the delay.
                    self.dock_set_timer(self.dock.settings.hide_delay, false);
                }
            }
            PointerEventKind::Motion { .. } => {
                if let Some((item, start, button)) = self.dock.views[self.dock.cur].press {
                    let moved = (x - start.0).hypot(y - start.1);
                    if self.dock.views[self.dock.cur].drag.is_none() && button == BTN_LEFT && moved > crate::drag_threshold() && matches!(self.dock.views[self.dock.cur].items.get(item), Some(Item::App { .. })) {
                        self.dock.views[self.dock.cur].drag = Some((item, (x, y)));
                        self.dock.views[self.dock.cur].feedback.release(std::time::Instant::now());
                    }
                }
                if let Some(drag) = self.dock.views[self.dock.cur].drag.as_mut() {
                    drag.1 = (x, y);
                    self.dock_draw();
                } else if self.dock.views[self.dock.cur].hover != under {
                    self.dock_set_hover(under);
                    let item = self.dock_feedback_item(under);
                    self.dock.views[self.dock.cur].feedback.set_hover(item, std::time::Instant::now());
                    self.dock_draw();
                }
            }
            PointerEventKind::Press { button, .. } => {
                if let Some(item) = under {
                    self.dock.views[self.dock.cur].press = Some((item, (x, y), button));
                    if let Some(it) = self.dock_feedback_item(Some(item)) {
                        self.dock.views[self.dock.cur].feedback.press(it, std::time::Instant::now());
                        self.dock_draw();
                    }
                }
            }
            PointerEventKind::Release { button, .. } => {
                let press = self.dock.views[self.dock.cur].press.take();
                let now = std::time::Instant::now();
                self.dock.views[self.dock.cur].feedback.release(now);
                // A click that launches or activates an app sends out a copy.
                if let Some((item, _, _)) = press.filter(|p| Some(p.0) == under && p.2 != BTN_RIGHT && self.dock.views[self.dock.cur].drag.is_none()) {
                    if let Some(it @ Item::App { .. }) = self.dock_feedback_item(Some(item)) {
                        self.dock.views[self.dock.cur].feedback.launch(it, now);
                    }
                }
                self.dock_draw();
                if let Some((item, _)) = self.dock.views[self.dock.cur].drag.take() {
                    self.dock_drop(item, (x, y));
                    self.dock_draw();
                    return true;
                }
                if let Some((item, _, pressed)) = press.filter(|p| Some(p.0) == under && p.2 == button) {
                    match pressed {
                        BTN_RIGHT => self.dock_menu(item, x, y),
                        BTN_MIDDLE => self.dock_activate(item, self.dock.settings.middle_click),
                        _ => self.dock_activate(item, self.dock.settings.click),
                    }
                }
            }
            PointerEventKind::Axis { vertical, horizontal, .. } => {
                let delta = if vertical.discrete != 0 { f64::from(vertical.discrete) } else if horizontal.discrete != 0 { f64::from(horizontal.discrete) } else { vertical.absolute + horizontal.absolute };
                if delta.abs() > 0.0 {
                    self.dock_scroll(under, delta > 0.0);
                }
            }
        }
        true
    }

    fn dock_scroll(&mut self, under: Option<usize>, forward: bool) {
        let on_app = matches!(under.and_then(|i| self.dock.views[self.dock.cur].items.get(i)), Some(Item::App { .. }));
        let scroll = match self.dock.settings.scroll {
            // `scroll-switch-workspace`: scrolling the action leaves unused
            // switches workspaces.
            logic::ScrollAction::Nothing if self.dock.settings.scroll_workspace => logic::ScrollAction::SwitchWorkspace,
            logic::ScrollAction::CycleWindows if !on_app && self.dock.settings.scroll_workspace => logic::ScrollAction::SwitchWorkspace,
            other => other,
        };
        match scroll {
            logic::ScrollAction::Nothing => {}
            logic::ScrollAction::SwitchWorkspace => {
                let target = if forward { crate::windows::WorkspaceTarget::Right } else { crate::windows::WorkspaceTarget::Left };
                if let (Some(list), Some(ws)) = (self.window_list.as_ref(), self.windows.workspace_target(target)) {
                    list.switch_workspace(ws);
                }
            }
            logic::ScrollAction::CycleWindows => {
                let Some(Item::App { id, .. }) = under.and_then(|i| self.dock.views[self.dock.cur].items.get(i)).cloned() else { return };
                let mut ids = self.app_windows(&id);
                ids.sort_unstable();
                if ids.is_empty() {
                    return;
                }
                let focused = self.windows.focused();
                let at = ids.iter().position(|w| *w == focused);
                let next = match (at, forward) {
                    (Some(i), true) => ids[(i + 1) % ids.len()],
                    (Some(i), false) => ids[(i + ids.len() - 1) % ids.len()],
                    (None, _) => ids[0],
                };
                self.activate_window(next);
            }
        }
    }

    // ----- Keyboard (switch-panels) ---------------------------------------

    /// Whether `surface` is the dock that has the keyboard.
    pub(super) fn dock_keyboard_surface(&self, surface: &wl_surface::WlSurface) -> bool {
        self.dock_keyboard_view().and_then(|i| self.dock.views[i].layer.as_ref()).is_some_and(|l| l.wl_surface() == surface)
    }

    /// The dock view that has the keyboard, if one does.
    pub(super) fn dock_keyboard_view(&self) -> Option<usize> {
        self.dock.views.iter().position(|v| v.keyboard.is_some())
    }

    /// Whether there is a dock to move the keyboard to.
    pub(super) fn dock_reachable(&self) -> bool {
        self.dock.settings.enabled && self.dock.views.iter().any(|v| v.layer.is_some() && !v.items.is_empty())
    }

    /// `switch-panels` reached the dock: its first item (last going
    /// backward) takes the keyboard, and a hidden dock shows while it has it.
    pub(super) fn focus_dock(&mut self, backward: bool) {
        let Some(i) = self.dock.views.iter().position(|v| v.layer.is_some() && !v.items.is_empty()) else { return };
        self.dock.cur = i;
        let n = self.dock.views[i].items.len();
        self.dock.views[i].keyboard = Some(if backward { n - 1 } else { 0 });
        self.dock_cancel_timer();
        if let Some(layer) = self.dock.views[i].layer.as_ref() {
            layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
            layer.commit();
        }
        self.dock_update();
        self.dock_draw();
    }

    /// The dock gives the keyboard back (to the window that had it) and
    /// may hide again.
    pub(super) fn leave_dock(&mut self) {
        let Some(i) = self.dock_keyboard_view() else { return };
        self.dock.cur = i;
        self.dock.views[i].keyboard = None;
        if let Some(layer) = self.dock.views[i].layer.as_ref() {
            layer.set_keyboard_interactivity(KeyboardInteractivity::None);
            layer.commit();
        }
        self.dock_update();
        self.dock_draw();
        if !self.dock.views[i].pointer_on {
            self.dock_set_timer(self.dock.settings.hide_delay, false);
        }
    }

    /// A key while the dock has the keyboard: the arrow keys along the
    /// bar (Left/Right, Up/Down on a side edge), Home/End, Enter or Space
    /// activates as a click does, Shift+F10 or Menu opens the item's menu,
    /// Escape leaves.
    pub(super) fn dock_key(&mut self, event: &KeyEvent) {
        let Some(i) = self.dock_keyboard_view() else { return };
        self.dock.cur = i;
        let n = self.dock.views[i].items.len();
        let Some(at) = self.dock.views[i].keyboard.filter(|_| n > 0) else { return };
        match dock_nav(event.keysym, self.mods.shift, at, n, self.dock.settings.edge.vertical()) {
            DockNav::Move(to) => {
                self.dock.views[i].keyboard = Some(to);
                self.dock_draw();
            }
            DockNav::Activate => {
                // The app (or what the item opens) takes the keyboard.
                self.leave_dock();
                self.dock.cur = i;
                self.dock_activate(at, self.dock.settings.click);
            }
            DockNav::Menu => self.dock_key_menu(at),
            DockNav::Leave => self.leave_dock(),
            DockNav::None => {}
        }
    }

    /// The keyboard's item's menu, at the item, its first entry selected.
    fn dock_key_menu(&mut self, index: usize) {
        let Some(&(cell, _)) = self.dock.views[self.dock.cur].hits.iter().find(|(_, i)| *i == index) else { return };
        let (cx, cy) = (f64::from(cell.x + cell.w / 2), f64::from(cell.y + cell.h / 2));
        self.menu_by_key = true;
        self.dock_menu(index, cx, cy);
        self.menu_by_key = false;
    }

    /// Click item `index` with `action`.
    fn dock_activate(&mut self, index: usize, action: logic::ClickAction) {
        crate::click_feedback();
        let Some(item) = self.dock.views[self.dock.cur].items.get(index).cloned() else { return };
        match item {
            Item::ShowApps => {
                if self.launcher.is_some() {
                    self.close_launcher(true);
                } else {
                    self.open_launcher(true);
                }
            }
            // A drive or the Trash shown in a window: that window.
            Item::Mount(_) | Item::Trash if !self.location_windows(&item).is_empty() => {
                let windows = self.location_windows(&item);
                self.activate_window(windows[0]);
            }
            Item::Trash => crate::media::trash_request(false),
            Item::Mount(i) => {
                if let Some(m) = self.dock.mounts.get(i) {
                    crate::media::request(m.root.clone(), crate::media::Request::Open);
                }
            }
            Item::Ext(i) => {
                if let Some(d) = self.ext.dock.get(i) {
                    let _ = self.requests.send(UiRequest::ExtAction(d.full_id.clone(), crate::ext_host::Action::DockActivate));
                }
            }
            Item::App { id, .. } => self.app_click(&id, action),
        }
    }

    /// Click (or Super+number) on app `id`.
    pub(super) fn app_click(&mut self, id: &str, action: logic::ClickAction) {
        let windows = self.app_windows(id);
        let focused = windows.contains(&self.windows.focused());
        let next = {
            let mut sorted = windows.clone();
            sorted.sort_unstable();
            sorted.iter().position(|w| *w == self.windows.focused()).map(|i| sorted[(i + 1) % sorted.len()])
        };
        match logic::click_outcome(action, &windows, focused, next) {
            Outcome::Nothing => {}
            Outcome::Launch => self.dock_launch(id, None),
            Outcome::Focus(w) if !self.dock.settings.single_window && !focused => {
                // All its windows come up, the newest used on top.
                let mru: Vec<u32> = self.windows.mru().iter().map(|e| e.id).filter(|id| windows.contains(id) && *id != w).collect();
                for other in mru.into_iter().rev() {
                    self.activate_window(other);
                }
                self.activate_window(w);
            }
            Outcome::Focus(w) => self.activate_window(w),
            Outcome::MinimizeAll => self.app_windows_action(&windows, WindowAction::Minimize),
            Outcome::CloseAll => self.app_windows_action(&windows, WindowAction::Close),
            Outcome::Overview => self.open_overview(true),
            Outcome::ChooseWindow => {
                let (x, y) = self.dock_item_point(id);
                if self.dock.settings.previews {
                    self.dock_previews(id, x, y);
                } else {
                    self.dock_window_menu(id, x, y);
                }
            }
        }
    }

    fn app_windows_action(&self, windows: &[u32], action: WindowAction) {
        if let Some(list) = self.window_list.as_ref().filter(|l| l.version() >= 4) {
            for w in windows {
                list.window_action(*w, action);
            }
        }
    }

    fn dock_launch(&self, id: &str, action: Option<&str>) {
        let desktop = desktop_id(id).unwrap_or_else(|| format!("{id}.desktop"));
        tracing::info!(app = desktop, action, "launching from the dock");
        let result = match action {
            Some(action) => crate::dock::launch_action(&desktop, action),
            None => crate::launcher::launch(&desktop),
        };
        if let Err(err) = result {
            tracing::warn!(%err, app = desktop, "launch failed");
        }
    }

    /// Layout coordinates of app `id`'s icon (for a menu), else the bar's
    /// middle.
    fn dock_item_point(&self, id: &str) -> (i32, i32) {
        let (bx, by, bw, bh) = self.dock_layout_rect();
        let found = self.dock.views[self.dock.cur].items.iter().position(|i| matches!(i, Item::App { id: a, .. } if a == id));
        let cell = found.and_then(|n| self.dock.views[self.dock.cur].hits.iter().find(|(_, i)| *i == n)).map(|(r, _)| *r);
        let (sx, sy) = (bx - self.dock.views[self.dock.cur].bar.x, by - self.dock.views[self.dock.cur].bar.y);
        match cell {
            Some(r) => (sx + r.x + r.w / 2, sy + r.y),
            None => (bx + bw / 2, by + bh / 2),
        }
    }

    /// The menu listing app `id`'s windows.
    fn dock_window_menu(&mut self, id: &str, x: i32, y: i32) {
        let mut items = Vec::new();
        self.dock.menu_targets.clear();
        for w in self.app_windows(id) {
            let title = self.windows.mru().iter().find(|e| e.id == w).map_or_else(String::new, |e| e.label().to_owned());
            items.push(self.dock_menu_item(title, DockTarget::Window(w)));
        }
        self.open_dock_menu(x, y, items);
    }

    /// The app's windows as previews, beside its dock icon at (x, y).
    fn dock_previews(&mut self, id: &str, x: i32, y: i32) {
        let windows = self.app_windows(id);
        if windows.is_empty() {
            return;
        }
        self.dock.menu_targets.clear();
        let mut items = Vec::new();
        for w in &windows {
            let title = self.windows.mru().iter().find(|e| e.id == *w).map_or_else(String::new, |e| e.label().to_owned());
            items.push(self.dock_menu_item(title, DockTarget::Window(*w)));
            items.push(self.dock_menu_item(tr("Close").into(), DockTarget::CloseWindow(*w)));
        }
        let (pw, ph) = window_menu::previews_size(windows.len());
        let (bx, by, bw, bh) = self.dock_layout_rect();
        let (mx, my) = match self.dock.settings.edge {
            Edge::Bottom => (x - pw / 2, by - ph - 8),
            Edge::Top => (x - pw / 2, by + bh + 8),
            Edge::Left => (bx + bw + 8, y - 16),
            Edge::Right => (bx - pw - 8, y - 16),
        };
        self.open_menu(0, mx, my, items, Vec::new());
        if let Some(menu) = self.window_menu.as_mut() {
            menu.previews = windows.clone();
        }
        // Fresh pictures (the old ones show until they come).
        self.capture_shots(&windows);
    }

    pub(super) fn dock_menu_item(&mut self, label: String, target: DockTarget) -> Option<window_menu::Item> {
        self.dock.menu_targets.push(target);
        Some(window_menu::Item { label, choice: window_menu::Choice::Dock(self.dock.menu_targets.len() - 1), checked: None, active: false, sub: Vec::new() })
    }

    /// Menu entries for quicklist items (enabled ones; submenus as
    /// submenus).
    fn quick_menu_items(&mut self, list: &crate::launcher_entry::Quicklist, quick: Vec<crate::launcher_entry::QuickItem>) -> Vec<Option<window_menu::Item>> {
        use crate::launcher_entry::QuickItem;
        let mut items = Vec::new();
        for q in quick {
            match q {
                QuickItem::Separator => items.push(None),
                QuickItem::Action { id, label, enabled: true, checked } => {
                    let mut item = self.dock_menu_item(label, DockTarget::Quick(list.clone(), id));
                    if let Some(item) = item.as_mut() {
                        item.checked = checked;
                    }
                    items.push(item);
                }
                QuickItem::Action { .. } => {}
                QuickItem::Submenu { label, items: sub } => {
                    let sub = self.quick_menu_items(list, sub);
                    if sub.iter().any(Option::is_some) {
                        items.push(Some(window_menu::Item { label, choice: window_menu::Choice::Submenu, checked: None, active: false, sub }));
                    }
                }
            }
        }
        items
    }

    pub(super) fn open_dock_menu(&mut self, x: i32, y: i32, items: Vec<Option<window_menu::Item>>) {
        if items.is_empty() {
            return;
        }
        // Above a bottom dock, beside a side dock.
        let rows: i32 = items.iter().map(|i| if i.is_some() { window_menu::item_h() } else { 9 }).sum::<i32>() + 8;
        let (bx, by, bw, bh) = self.dock_layout_rect();
        let (mx, my) = match self.dock.settings.edge {
            Edge::Bottom => (x - window_menu::menu_w() / 2, by - rows - 8),
            Edge::Top => (x - window_menu::menu_w() / 2, by + bh + 8),
            Edge::Left => (bx + bw + 8, y - 16),
            Edge::Right => (bx - window_menu::menu_w() - 8, y - 16),
        };
        self.open_menu(0, mx, my, items, Vec::new());
    }

    /// The right-click menu of item `index`, at surface point (x, y).
    fn dock_menu(&mut self, index: usize, x: f64, y: f64) {
        let Some(item) = self.dock.views[self.dock.cur].items.get(index).cloned() else { return };
        let (bx, by, _, _) = self.dock_layout_rect();
        let (lx, ly) = (bx - self.dock.views[self.dock.cur].bar.x + x as i32, by - self.dock.views[self.dock.cur].bar.y + y as i32);
        self.dock.menu_targets.clear();
        let mut items: Vec<Option<window_menu::Item>> = Vec::new();
        match item {
            Item::App { id, favorite } => {
                let previews = self.dock.settings.previews;
                items.extend(self.app_menu_items(&id, favorite, previews));
            }
            Item::Mount(i) => {
                let Some(m) = self.dock.mounts.get(i).cloned() else { return };
                items.push(self.dock_menu_item(if m.mounted { tr("Open") } else { tr("Mount") }.into(), DockTarget::OpenMount(m.root.clone())));
                if m.mounted {
                    let eject = if m.ejectable { tr("Eject") } else { tr("Unmount") };
                    items.push(self.dock_menu_item(eject.into(), DockTarget::EjectMount(m.root)));
                }
            }
            Item::Ext(i) => {
                // Its menu comes from the extension (`show_ext_menu`).
                if let Some(d) = self.ext.dock.get(i).filter(|d| d.has_menu) {
                    self.ext_dock_menu_at = Some((lx, ly));
                    let _ = self.requests.send(UiRequest::ExtAction(d.full_id.clone(), crate::ext_host::Action::DockMenu { x: lx, y: ly }));
                }
                return;
            }
            Item::Trash => {
                items.push(self.dock_menu_item(tr("Open").into(), DockTarget::OpenTrash));
                if self.dock.trash_full {
                    items.push(self.dock_menu_item(tr("Empty Trash").into(), DockTarget::EmptyTrash));
                }
            }
            Item::ShowApps => return,
        }
        self.open_dock_menu(lx, ly, items);
    }

    /// The menu entries for app `id` (the dock's and the app grid's):
    /// windows, open, actions, quicklist, pin, details, quit. With
    /// `previews`, its windows are one "All Windows" entry.
    pub(super) fn app_menu_items(&mut self, id: &str, favorite: bool, previews: bool) -> Vec<Option<window_menu::Item>> {
        let id = id.to_owned();
        let mut items: Vec<Option<window_menu::Item>> = Vec::new();
        let windows = self.app_windows(&id);
        if previews && !windows.is_empty() {
            items.push(self.dock_menu_item(tr("All Windows").into(), DockTarget::Previews(id.clone())));
        } else {
            for w in &windows {
                let title = self.windows.mru().iter().find(|e| e.id == *w).map_or_else(String::new, |e| e.label().to_owned());
                items.push(self.dock_menu_item(title, DockTarget::Window(*w)));
            }
        }
        if !windows.is_empty() {
            items.push(None);
        }
        let open = if windows.is_empty() { tr("Open") } else { tr("New Window") };
        items.push(self.dock_menu_item(open.into(), DockTarget::Launch(id.clone())));
        let actions = desktop_id(&id).map(|d| logic::desktop_actions(d.trim_end_matches(".desktop"))).unwrap_or_default();
        for (action, name) in actions {
            items.push(self.dock_menu_item(name, DockTarget::Action(id.clone(), action)));
        }
        // What the app puts in its quicklist (disabled entries are
        // left out).
        let quick = self.dock_entry(&id).and_then(|e| Some((e.quicklist.clone()?, e.quick_items.clone())));
        if let Some((list, quick)) = quick.filter(|(_, q)| !q.is_empty()) {
            items.push(None);
            let entries = self.quick_menu_items(&list, quick);
            items.extend(entries);
        }
        items.push(None);
        let pin = if favorite { tr("Unpin") } else { tr("Pin to Dash") };
        items.push(self.dock_menu_item(pin.into(), DockTarget::TogglePin(id.clone())));
        if which("gnome-software") {
            items.push(self.dock_menu_item(tr("App Details").into(), DockTarget::Details(id.clone())));
        }
        if !windows.is_empty() {
            items.push(None);
            let quit = if windows.len() > 1 { helix_i18n::trnf!("Quit {n} Window", "Quit {n} Windows", windows.len()) } else { tr("Quit").to_owned() };
            items.push(self.dock_menu_item(quit, DockTarget::Quit(id.clone())));
        }
        items
    }

    /// A dock menu entry was chosen.
    pub(super) fn dock_menu_choice(&mut self, index: usize) {
        let from_grid = std::mem::take(&mut self.dock.menu_from_grid);
        let Some(target) = self.dock.menu_targets.get(index).cloned() else { return };
        if from_grid {
            // From the app grid: opening something closes the launcher;
            // pinning takes the app out of the grid.
            match &target {
                DockTarget::TogglePin(_) => {}
                DockTarget::Quit(_) => {}
                DockTarget::ClearRecent => {}
                _ => self.close_launcher(true),
            }
        }
        let pinning = matches!(target, DockTarget::TogglePin(_));
        self.dock_menu_choice_do(target);
        if from_grid && pinning {
            self.grid_reload();
        }
    }

    fn dock_menu_choice_do(&mut self, target: DockTarget) {
        match target {
            DockTarget::Window(w) => self.activate_window(w),
            DockTarget::Launch(id) => self.dock_launch(&id, None),
            DockTarget::Action(id, action) => self.dock_launch(&id, Some(&action)),
            DockTarget::TogglePin(id) => {
                let desktop = desktop_id(&id).unwrap_or_else(|| format!("{id}.desktop"));
                crate::favorites::toggle_favorite(&desktop);
                self.dock_update();
            }
            DockTarget::Details(id) => {
                let desktop = desktop_id(&id).unwrap_or_else(|| format!("{id}.desktop"));
                if let Err(err) = std::process::Command::new("gnome-software").arg(format!("--details={desktop}")).spawn() {
                    tracing::warn!(%err, "cannot show the app's details");
                }
            }
            DockTarget::Quit(id) => {
                let windows = self.app_windows(&id);
                self.app_windows_action(&windows, WindowAction::Close);
            }
            DockTarget::OpenMount(root) => crate::media::request(root, crate::media::Request::Open),
            DockTarget::EjectMount(root) => crate::media::request(root, crate::media::Request::Eject),
            DockTarget::OpenTrash => crate::media::trash_request(false),
            DockTarget::EmptyTrash => {
                let _ = self.requests.send(UiRequest::EmptyTrash);
            }
            DockTarget::Quick(list, id) => {
                let _ = self.requests.send(UiRequest::QuickItem(list, id));
            }
            DockTarget::CloseWindow(w) => self.app_windows_action(&[w], WindowAction::Close),
            DockTarget::Previews(id) => {
                let (x, y) = self.dock_item_point(&id);
                self.dock_previews(&id, x, y);
            }
            DockTarget::ClearRecent => self.grid_clear_recent(),
        }
    }

    /// Item `item` dragged and let go at surface point `at`.
    fn dock_drop(&mut self, item: usize, at: (f64, f64)) {
        let Some(Item::App { id, favorite }) = self.dock.views[self.dock.cur].items.get(item).cloned() else { return };
        let bar = self.dock.views[self.dock.cur].bar;
        let vertical = self.dock.settings.edge.vertical();
        let off = if vertical {
            (at.0 - f64::from(bar.x)).max(f64::from(bar.x + bar.w) - at.0).max(0.0) - f64::from(bar.w)
        } else {
            (at.1 - f64::from(bar.y)).max(f64::from(bar.y + bar.h) - at.1).max(0.0) - f64::from(bar.h)
        };
        let desktop = desktop_id(&id).unwrap_or_else(|| format!("{id}.desktop"));
        let favorites = crate::favorites::favorites();
        if off > UNPIN_DISTANCE {
            if favorite {
                tracing::info!(app = desktop, "unpinned by dragging off the dock");
                crate::favorites::toggle_favorite(&desktop);
            }
        } else {
            // The slot it was dropped at, counted among the favorites.
            let p = if vertical { at.1 } else { at.0 };
            let favorite_cells: Vec<(Rect, usize)> = self.dock.views[self.dock.cur].hits.iter().filter(|(_, i)| matches!(self.dock.views[self.dock.cur].items.get(*i), Some(Item::App { favorite: true, .. }))).copied().collect();
            let to = favorite_cells.iter().position(|(r, _)| p < f64::from(if vertical { r.y + r.h / 2 } else { r.x + r.w / 2 })).unwrap_or(favorite_cells.len());
            let shown_favorites: Vec<String> = favorite_cells
                .iter()
                .filter_map(|(_, i)| match self.dock.views[self.dock.cur].items.get(*i) {
                    Some(Item::App { id, .. }) => Some(desktop_id(id).unwrap_or_else(|| format!("{id}.desktop"))),
                    _ => None,
                })
                .collect();
            // Favorites not installed keep their place after the shown ones.
            let hidden: Vec<String> = favorites.iter().filter(|f| !shown_favorites.contains(f)).cloned().collect();
            let mut list = logic::moved_favorites(&shown_favorites, &desktop, to);
            list.extend(hidden.into_iter().filter(|f| *f != desktop));
            if list != favorites {
                tracing::info!(app = desktop, to, "favorites reordered");
                crate::favorites::set_favorites(&list);
            }
        }
        self.dock_update();
    }

    /// The apps of the dock (or of the dash without one), in order.
    fn dock_apps(&self) -> Vec<String> {
        let shown: Vec<String> = self.dock.views.first().map(|v| v.items.iter().filter_map(|i| match i {
            Item::App { id, .. } => Some(id.clone()),
            _ => None,
        }).collect()).unwrap_or_default();
        if !shown.is_empty() {
            return shown;
        }
        let installed = crate::icons::apps().shown.clone();
        let settings = DockSettings { show_mounts: false, show_trash: false, show_apps_button: false, ..self.dock.settings.clone() };
        let running = self.dock_running();
        logic::items(&settings, &crate::favorites::favorites(), &installed, &running, 0, 0)
            .into_iter()
            .filter_map(|i| match i {
                Item::App { id, .. } => Some(id),
                _ => None,
            })
            .collect()
    }

    /// `switch-to-application-N` (`launch`: `open-new-window-application-N`):
    /// the Nth app of the dash.
    pub(super) fn switch_to_app(&mut self, n: usize, launch: bool) {
        if let Some(id) = self.dock_apps().get(n.saturating_sub(1)).cloned() {
            self.app_click(&id, if launch { logic::ClickAction::Launch } else { logic::ClickAction::CycleWindows });
        }
    }

    /// One of the dock's hot keys for app `n` (1–10).
    pub(super) fn dock_hot_key(&mut self, n: usize, which: logic::HotKey) {
        let action = match which {
            logic::HotKey::Plain => self.dock.settings.click,
            logic::HotKey::Shift => self.dock.settings.shift_click,
            logic::HotKey::Ctrl => logic::ClickAction::Launch,
        };
        if let Some(id) = self.dock_apps().get(n.saturating_sub(1)).cloned() {
            self.app_click(&id, action);
        }
        self.dock_flash();
    }

    /// Shows a hidden dock for `secs` seconds (it hides then unless the
    /// pointer is on it).
    fn dock_show_for(&mut self, secs: f64) {
        for i in 0..self.dock.views.len() {
            self.dock.views[i].revealed = true;
        }
        self.dock_update();
        for i in 0..self.dock.views.len() {
            self.dock.cur = i;
            self.dock_set_timer(secs, false);
        }
        self.dock.cur = 0;
    }

    /// The mounts the settings let the dock show.
    pub(super) fn dock_filter_mounts(&mut self) {
        let s = &self.dock.settings;
        self.dock.mounts = self.dock.all_mounts.iter().filter(|m| crate::media::shown(m, s.mounts_network, s.mounts_only_mounted)).cloned().collect();
    }

    /// App `app` turned urgent: the dock shows (`show-dock-urgent-notify`)
    /// and its icon wiggles (`dance-urgent-applications`).
    pub(super) fn dock_urgent(&mut self, app: &str) {
        if !self.dock.settings.enabled {
            return;
        }
        if self.dock.settings.urgent_shows {
            self.dock_show_for(URGENT_SHOW);
        }
        // (Reduced motion: no wiggle; the urgent mark shows it.)
        if self.dock.settings.urgent_dance && self.dock.settings.animation_time > 0.0 && !self.dock.settings.reduced_motion {
            self.dock.dancing.insert(app.to_lowercase(), std::time::Instant::now() + DANCE);
            if self.dock.dance_timer.is_none() {
                if let Some(handle) = self.loop_handle.as_ref() {
                    let token = handle.insert_source(Timer::from_duration(Duration::from_millis(40)), |_, _, ui: &mut Ui| {
                        let now = std::time::Instant::now();
                        ui.dock.dancing.retain(|_, until| *until > now);
                        ui.dock_draw();
                        if ui.dock.dancing.is_empty() {
                            ui.dock.dance_timer = None;
                            TimeoutAction::Drop
                        } else {
                            TimeoutAction::ToDuration(Duration::from_millis(40))
                        }
                    });
                    self.dock.dance_timer = token.ok();
                }
            }
        }
    }

    /// How far app `id`'s icon is moved by its wiggle now.
    fn dance_offset(&self, id: &str) -> f64 {
        let Some(until) = self.dock.dancing.get(&id.to_lowercase()) else { return 0.0 };
        let left = until.saturating_duration_since(std::time::Instant::now()).as_secs_f64();
        let t = DANCE.as_secs_f64() - left;
        // Four swings a second, fading out.
        (t * std::f64::consts::TAU * 4.0).sin() * DANCE_PX * (left / DANCE.as_secs_f64())
    }

    /// After a hot key or `shortcut`: numbers on the icons
    /// (`hotkeys-overlay`) and the dock shown (`hotkeys-show-dock`) for
    /// `shortcut-timeout` seconds.
    pub(super) fn dock_flash(&mut self) {
        let Some(keys) = self.dock.settings.hot_keys.clone() else { return };
        if keys.show_dock {
            self.dock_show_for(keys.timeout);
        }
        if keys.overlay {
            self.dock.numbers = true;
            if let (Some(handle), Some(token)) = (self.loop_handle.as_ref(), self.dock.numbers_timer.take()) {
                handle.remove(token);
            }
            if let Some(handle) = self.loop_handle.as_ref() {
                let token = handle.insert_source(Timer::from_duration(Duration::from_secs_f64(keys.timeout)), |_, _, ui: &mut Ui| {
                    ui.dock.numbers_timer = None;
                    ui.dock.numbers = false;
                    ui.dock_draw();
                    TimeoutAction::Drop
                });
                self.dock.numbers_timer = token.ok();
            }
            self.dock_draw();
        }
    }
}

/// Whether a separator goes between two neighbouring items.
fn separated(a: &Item, b: &Item) -> bool {
    let group = |i: &Item| match i {
        Item::ShowApps => 0,
        Item::App { favorite: true, .. } => 1,
        Item::App { favorite: false, .. } => 2,
        Item::Mount(_) | Item::Ext(_) | Item::Trash => 3,
    };
    group(a) != group(b)
}

/// Whether window app id `window_app` is app `id` (by its desktop file).
/// The file manager: the app opening folders (its id without `.desktop`).
fn file_manager() -> Option<&'static str> {
    static ID: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    ID.get_or_init(|| {
        use gio::prelude::AppInfoExt;
        gio::AppInfo::default_for_type("inode/directory", false).and_then(|a| a.id()).map(|id| id.trim_end_matches(".desktop").to_owned())
    })
    .as_deref()
}

/// The Trash's name as the file manager titles its window.
fn trash_title() -> &'static str {
    static NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NAME.get_or_init(|| {
        use gio::prelude::FileExt;
        gio::File::for_uri("trash:///")
            .query_info("standard::display-name", gio::FileQueryInfoFlags::NONE, gio::Cancellable::NONE)
            .map(|i| i.display_name().to_string())
            .unwrap_or_else(|_| tr("Trash").into())
    })
}

fn desktop_matches(window_app: &str, id: &str) -> bool {
    !window_app.is_empty() && desktop_id(window_app).is_some_and(|d| d.trim_end_matches(".desktop").eq_ignore_ascii_case(id))
}

fn which(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(program).is_file()))
}

/// Show Apps: a 3×3 grid of dots.
fn draw_grid(canvas: &mut Canvas, x: i32, y: i32, icon: i32) {
    let step = f64::from(icon) / 4.0;
    for r in 1..=3 {
        for c in 1..=3 {
            canvas.circle(f64::from(x) + step * f64::from(c), f64::from(y) + step * f64::from(r), f64::from(icon) / 14.0, FG);
        }
    }
}

/// Running marks for `n` windows, on the screen-edge side of `cell`. The
/// focused window's app (`focused`) has them joined into one wide pill in
/// the accent, so it is told from the others by shape as well as color.
pub(super) fn draw_indicator(canvas: &mut Canvas, style: logic::Indicator, edge: Edge, cell: Rect, n: usize, focused: bool) {
    if n == 0 {
        return;
    }
    let n = n as i32;
    let (mark, gap) = match style {
        logic::Indicator::Dots | logic::Indicator::Squares => (5, 4),
        logic::Indicator::Dashes => (10, 3),
        logic::Indicator::Solid => (cell.w.min(cell.h) - 16, 0),
    };
    let count = if style == logic::Indicator::Solid { 1 } else { n };
    let span = count * mark + (count - 1) * gap;
    // Where a mark `len` long and 3 thick starts, `along` from the cell's
    // middle.
    let place = |along: i32, len: i32| match edge {
        Edge::Bottom => (cell.x + cell.w / 2 + along, cell.y + cell.h - 6, len, 3),
        Edge::Top => (cell.x + cell.w / 2 + along, cell.y + 3, len, 3),
        Edge::Left => (cell.x + 3, cell.y + cell.h / 2 + along, 3, len),
        Edge::Right => (cell.x + cell.w - 6, cell.y + cell.h / 2 + along, 3, len),
    };
    if focused {
        let len = span.max(18);
        let (x, y, w, h) = place(-len / 2, len);
        canvas.round_rect(f64::from(x), f64::from(y) - 0.5, f64::from(w), f64::from(h) + 1.0, 2.0, accent());
        return;
    }
    for k in 0..count {
        let (x, y, w, h) = place(k * (mark + gap) - span / 2, mark);
        match style {
            logic::Indicator::Dots => {
                let (cx, cy) = match edge {
                    Edge::Left | Edge::Right => (f64::from(x) + 1.5, f64::from(y) + f64::from(mark) / 2.0),
                    _ => (f64::from(x) + f64::from(mark) / 2.0, f64::from(y) + 1.5),
                };
                canvas.circle(cx, cy, 2.5, FG);
            }
            _ => canvas.rect(x, y, w, h, FG),
        }
    }
}

/// What a key does in the dock (see `Ui::dock_key`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DockNav {
    Move(usize),
    Activate,
    Menu,
    Leave,
    None,
}

/// What `key` (with Shift if `shift`) does with item `at` of `n` focused,
/// on a bar that is `vertical` or not.
fn dock_nav(key: Keysym, shift: bool, at: usize, n: usize, vertical: bool) -> DockNav {
    let (back, forward) = if vertical { (Keysym::Up, Keysym::Down) } else { (Keysym::Left, Keysym::Right) };
    let to = match key {
        k if k == back => at.saturating_sub(1),
        k if k == forward => (at + 1).min(n.saturating_sub(1)),
        Keysym::Home => 0,
        Keysym::End => n.saturating_sub(1),
        Keysym::Escape => return DockNav::Leave,
        Keysym::Return | Keysym::KP_Enter | Keysym::space => return DockNav::Activate,
        Keysym::Menu => return DockNav::Menu,
        Keysym::F10 if shift => return DockNav::Menu,
        _ => return DockNav::None,
    };
    if to == at { DockNav::None } else { DockNav::Move(to) }
}

#[cfg(test)]
mod nav_tests {
    use super::*;

    #[test]
    fn keys_along_the_bar() {
        let nav = |k, shift, at| dock_nav(k, shift, at, 5, false);
        assert_eq!(nav(Keysym::Right, false, 0), DockNav::Move(1));
        assert_eq!(nav(Keysym::Left, false, 0), DockNav::None, "stops at the start");
        assert_eq!(nav(Keysym::Right, false, 4), DockNav::None, "and at the end");
        assert_eq!(nav(Keysym::End, false, 1), DockNav::Move(4));
        assert_eq!(nav(Keysym::Home, false, 3), DockNav::Move(0));
        assert_eq!(nav(Keysym::Down, false, 1), DockNav::None, "not along a bottom bar");
        assert_eq!(dock_nav(Keysym::Down, false, 1, 5, true), DockNav::Move(2), "along a side bar");
        assert_eq!(nav(Keysym::Return, false, 2), DockNav::Activate);
        assert_eq!(nav(Keysym::space, false, 2), DockNav::Activate);
        assert_eq!(nav(Keysym::F10, true, 2), DockNav::Menu);
        assert_eq!(nav(Keysym::F10, false, 2), DockNav::None);
        assert_eq!(nav(Keysym::Menu, false, 2), DockNav::Menu);
        assert_eq!(nav(Keysym::Escape, false, 2), DockNav::Leave);
    }
}

/// When the tooltip of an item the pointer just reached shows, and
/// whether at once: at once while another is shown or soon after one went
/// (moving along the dock), else after [`TOOLTIP_DELAY`].
pub(super) fn tooltip_from(now: std::time::Instant, shown: bool, gone: Option<std::time::Instant>) -> (std::time::Instant, bool) {
    let follow = shown || gone.is_some_and(|g| now.duration_since(g) < TOOLTIP_FOLLOW);
    (if follow { now } else { now + TOOLTIP_DELAY }, follow)
}

#[cfg(test)]
mod tooltip_tests {
    use super::*;

    #[test]
    fn tooltips_wait_then_follow() {
        let t = std::time::Instant::now();
        assert_eq!(tooltip_from(t, false, None), (t + TOOLTIP_DELAY, false), "resting first");
        assert_eq!(tooltip_from(t, true, None), (t, true), "moving on from a shown one");
        assert_eq!(tooltip_from(t + Duration::from_millis(200), false, Some(t)), (t + Duration::from_millis(200), true), "just after one went");
        let later = t + Duration::from_secs(2);
        assert_eq!(tooltip_from(later, false, Some(t)), (later + TOOLTIP_DELAY, false), "long after");
    }
}

/// What one dock shows, gathered from the shell's state (see
/// `Ui::dock_paint_model`), so that drawing it needs nothing else.
pub(super) struct DockPaint {
    /// The surface's size (logical).
    pub size: (i32, i32),
    pub edge: Edge,
    pub extend: bool,
    pub always_center: bool,
    pub indicator: logic::Indicator,
    /// Item size and the bar's thickness.
    pub icon: i32,
    pub thickness: i32,
    /// The bar's opacity, 0 to 255.
    pub background: u32,
    pub items: Vec<Item>,
    pub icons: Vec<Option<helix_draw::Image>>,
    /// Per item: windows (up to 4) and the badge count.
    pub counts: Vec<(usize, usize)>,
    pub progress: Vec<Option<f64>>,
    pub urgent: Vec<bool>,
    pub wiggles: Vec<i32>,
    /// Hot key numbers shown on the first ten apps.
    pub numbers: Vec<Option<usize>>,
    /// The item of the focused window's app.
    pub focused_item: Option<usize>,
    /// Hover, press and launch feedback per item.
    pub looks: Vec<crate::item_feedback::Look>,
    /// Launch copies growing out of items: (item, progress 0 to 1).
    pub ghosts: Vec<(usize, f64)>,
    pub drag: Option<(usize, (f64, f64))>,
    /// The tooltip shown: the item and its lines.
    pub tooltip: Option<(usize, Vec<String>)>,
    /// The item with the keyboard focus (switch-panels).
    pub keyboard: Option<usize>,
}

/// Where a drawn dock's bar and items are.
pub(super) struct DockPainted {
    pub bar: Rect,
    pub hits: Vec<(Rect, usize)>,
}

/// Draw a dock. The three states an app can be in are told apart by shape
/// as well as color: the item under the pointer (or pressed) gets a light
/// square behind its icon; the focused window's app has its running marks
/// joined into one wide accent pill; an app wanting attention gets an
/// orange dot at its icon's top left corner (and wiggles).
pub(super) fn paint_dock(canvas: &mut Canvas, text: Option<&Text>, m: &DockPaint) -> DockPainted {
    let (w, h) = m.size;
    let (icon, t) = (m.icon, m.thickness);
    let bar = match m.edge {
        Edge::Bottom => Rect { x: 0, y: h - t, w, h: t },
        Edge::Top => Rect { x: 0, y: 0, w, h: t },
        Edge::Left => Rect { x: 0, y: 0, w: t, h },
        Edge::Right => Rect { x: w - t, y: 0, w: t, h },
    };
    let radius = if m.extend { 0.0 } else { theme::RADIUS_POPOVER };
    canvas.round_rect(f64::from(bar.x), f64::from(bar.y), f64::from(bar.w), f64::from(bar.h), radius, (m.background << 24) | (DOCK_BG & 0xFFFFFF));
    if helix_draw::high_contrast() {
        canvas.round_outline(f64::from(bar.x), f64::from(bar.y), f64::from(bar.w), f64::from(bar.h), radius, 1.0, 0xFFFFFFFF);
    }
    let vertical = m.edge.vertical();
    let slot = icon + icon / 4;
    let total = Ui::dock_length(icon, &m.items);
    let along_len = if vertical { h } else { w };
    let mut pos = if m.extend && !m.always_center { icon / 8 } else { (along_len - total) / 2 + icon / 8 };
    let mut hits = Vec::new();
    let mut centers: Vec<(f64, f64)> = Vec::with_capacity(m.items.len());
    let mut drop_marker = None;
    for (i, item) in m.items.iter().enumerate() {
        if i > 0 && separated(&m.items[i - 1], item) {
            let (sx, sy, sw, sh) = if vertical { (bar.x + 8, pos + 4, t - 16, 1) } else { (pos + 4, bar.y + 8, 1, t - 16) };
            canvas.rect(sx, sy, sw, sh, 0x60FFFFFF);
            pos += 9;
        }
        let cell = if vertical { Rect { x: bar.x, y: pos, w: t, h: slot } } else { Rect { x: pos, y: bar.y, w: slot, h: t } };
        if let Some((_, at)) = m.drag {
            let p = if vertical { at.1 } else { at.0 };
            if drop_marker.is_none() && p < f64::from(pos + slot / 2) {
                drop_marker = Some(pos);
            }
        }
        let wiggle = m.wiggles[i];
        let (ix, iy) = (cell.x + (cell.w - icon) / 2 + if vertical { 0 } else { wiggle }, cell.y + (cell.h - icon) / 2 + if vertical { wiggle } else { 0 });
        // Hover and press: a light square behind the icon (only those).
        let look = m.looks[i];
        paint_item_highlight(canvas, ix, iy, icon, look);
        if m.keyboard == Some(i) {
            helix_draw::theme::focus_ring_round(canvas, f64::from(ix - 4), f64::from(iy - 4), f64::from(icon + 8), f64::from(icon + 8), 8.0);
        }
        let (cx, cy) = (f64::from(ix) + f64::from(icon) / 2.0, f64::from(iy) + f64::from(icon) / 2.0);
        centers.push((cx, cy));
        if !m.drag.is_some_and(|(d, _)| d == i) {
            let size = f64::from(icon) * look.scale;
            let (sx, sy) = (cx - size / 2.0, cy - size / 2.0);
            match (item, &m.icons[i]) {
                (Item::ShowApps, _) => draw_grid(canvas, sx.round() as i32, sy.round() as i32, size.round() as i32),
                (_, Some(image)) => paint_item_icon(canvas, (cx, cy), icon, look.scale, image),
                // Being decoded (apps without one get the generic icon).
                (_, None) => {}
            }
        }
        if let Some(progress) = m.progress[i] {
            // A bar across the icon's bottom.
            let (px0, py0, pw) = (f64::from(ix + 3), f64::from(iy + icon - 7), f64::from(icon - 6));
            canvas.round_rect(px0, py0, pw, 5.0, 2.5, 0xC0202024);
            if progress > 0.0 {
                canvas.round_rect(px0, py0, (pw * progress).max(5.0), 5.0, 2.5, accent());
            }
        }
        let (windows, badge) = m.counts[i];
        draw_indicator(canvas, m.indicator, m.edge, cell, windows, m.focused_item == Some(i));
        if m.urgent[i] {
            // Wanting attention: an orange dot with a dark edge.
            let (ux, uy) = (f64::from(ix) + 3.0, f64::from(iy) + 3.0);
            canvas.circle(ux, uy, 6.5, 0xFF1E1E24);
            canvas.circle(ux, uy, 5.0, URGENT);
        }
        if let Some(number) = m.numbers[i] {
            // The app's hot key number, at the icon's top left.
            let (nx, ny) = (f64::from(ix) + 4.0, f64::from(iy) + 4.0);
            canvas.circle(nx, ny, 10.0, 0xF0505058);
            let label_text = (number % 10).to_string();
            let tw = text.map_or(6.0, |t| t.width(&label_text, size::CAPTION));
            label(text, canvas, &label_text, (nx - f64::from(tw) / 2.0) as f32, ny as f32 + 5.0, size::CAPTION, FG, 20.0);
        }
        if badge > 0 {
            let (bx, by) = (f64::from(ix + icon) - 4.0, f64::from(iy) + 4.0);
            canvas.circle(bx, by, 9.0, BADGE);
            let label_text = if badge > 9 { "9+".to_owned() } else { badge.to_string() };
            let tw = text.map_or(6.0, |t| t.width(&label_text, size::TINY));
            label(text, canvas, &label_text, (bx - f64::from(tw) / 2.0) as f32, by as f32 + 4.0, size::TINY, FG, 20.0);
        }
        hits.push((cell, i));
        pos += slot;
    }
    // Launch copies growing out of their items.
    for &(k, p) in &m.ghosts {
        if let (Some(Some(image)), Some(&center)) = (m.icons.get(k), centers.get(k)) {
            paint_launch_copy(canvas, center, icon, image, p);
        }
    }
    if let Some((d, at)) = m.drag {
        let marker = drop_marker.unwrap_or(pos);
        let (mx, my, mw, mh) = if vertical { (bar.x + 6, marker - 1, t - 12, 3) } else { (marker - 1, bar.y + 6, 3, t - 12) };
        canvas.rect(mx, my, mw, mh, accent());
        if let Some(Some(image)) = m.icons.get(d) {
            canvas.image(at.0 - f64::from(icon) / 2.0, at.1 - f64::from(icon) / 2.0, f64::from(icon), f64::from(icon), image);
        }
    }
    // The item's name (and windows), beside the bar.
    if let Some((i, lines)) = &m.tooltip {
        if let Some(&(cell, _)) = hits.iter().find(|(_, n)| n == i) {
            let (tw, lh) = tooltip_size(text, lines);
            let lw = tw.min(if vertical { TOOLTIP_ROOM_W - 12 } else { w });
            let (lx, ly) = match m.edge {
                Edge::Bottom => (cell.x + cell.w / 2 - lw / 2, bar.y - lh - 6),
                Edge::Top => (cell.x + cell.w / 2 - lw / 2, bar.y + bar.h + 6),
                Edge::Left => (bar.x + bar.w + 6, cell.y + cell.h / 2 - lh / 2),
                Edge::Right => (bar.x - lw - 6, cell.y + cell.h / 2 - lh / 2),
            };
            let lx = lx.clamp(0, (w - lw).max(0));
            paint_tooltip(canvas, text, Rect { x: lx, y: ly, w: lw, h: lh }, lines);
        }
    }
    DockPainted { bar, hits }
}

/// The highlight behind an item's icon at (`ix`, `iy`), `icon` wide, for
/// its feedback `look` ("Item feedback"): a light square while lit,
/// darker while pressed; an outline in high contrast.
pub(super) fn paint_item_highlight(canvas: &mut Canvas, ix: i32, iy: i32, icon: i32, look: crate::item_feedback::Look) {
    let lit = (look.highlight * f64::from(0x40)).round() as u32;
    let alpha = if look.pressed { 0x59 } else { lit };
    if alpha == 0 {
        return;
    }
    let (x, y, d) = (f64::from(ix - 4), f64::from(iy - 4), f64::from(icon + 8));
    if helix_draw::high_contrast() {
        canvas.round_outline(x, y, d, d, 8.0, if look.pressed { 2.0 } else { 1.0 }, 0xFFFFFFFF);
    } else {
        canvas.round_rect(x, y, d, d, 8.0, (alpha << 24) | 0xFFFFFF);
    }
}

/// An item's icon centered on `center`, `icon` wide at `scale` (its
/// feedback size).
pub(super) fn paint_item_icon(canvas: &mut Canvas, center: (f64, f64), icon: i32, scale: f64, image: &helix_draw::Image) {
    let size = f64::from(icon) * scale;
    canvas.image(center.0 - size / 2.0, center.1 - size / 2.0, size, size, image);
}

/// A launch copy of an item's icon at progress `p` (0 to 1): growing out
/// of `center` while it fades.
pub(super) fn paint_launch_copy(canvas: &mut Canvas, center: (f64, f64), icon: i32, image: &helix_draw::Image, p: f64) {
    let size = f64::from(icon) * (1.0 + (crate::item_feedback::GHOST_SCALE - 1.0) * p);
    canvas.image_alpha(center.0 - size / 2.0, center.1 - size / 2.0, size, size, image, (0.8 * (1.0 - p)) as f32);
}

/// The size an item tooltip of `lines` needs (unclamped width, height).
pub(super) fn tooltip_size(text: Option<&Text>, lines: &[String]) -> (i32, i32) {
    let px = size::CAPTION;
    let widest = lines.iter().map(|l| text.map_or(80.0, |t| t.width(l, px))).fold(0.0f32, f32::max);
    let lh = helix_draw::theme::grow(26) + (lines.len() as i32 - 1) * helix_draw::theme::grow(18);
    (widest.ceil() as i32 + 28, lh)
}

/// An item tooltip in `r`: the name, then (dimmer) one line each for its
/// windows.
pub(super) fn paint_tooltip(canvas: &mut Canvas, text: Option<&Text>, r: Rect, lines: &[String]) {
    let Some(first) = lines.first() else { return };
    let px = size::CAPTION;
    let line_h = helix_draw::theme::grow(18);
    theme::tooltip(canvas, r.x, r.y, r.w, r.h);
    let base = baseline(text, r.y, helix_draw::theme::grow(26), px);
    label(text, canvas, first, (r.x + 12) as f32, base, px, FG, (r.w - 16) as f32);
    for (n, line) in lines.iter().enumerate().skip(1) {
        label(text, canvas, line, (r.x + 12) as f32, base + (n as i32 * line_h) as f32, px, DIM_FG, (r.w - 16) as f32);
    }
}

impl Ui {
    /// Each dock shown, for the accessibility bus (`ui/atspi.rs`): its
    /// surface, monitor area and size, and its items where they are.
    pub(super) fn dock_a11y(&self) -> Vec<super::atspi::DockShown<'_>> {
        let views = self.dock.views.iter().filter(|v| v.configured && v.width > 0);
        views
            .filter_map(|v| {
                let items = v.hits.iter().filter_map(|(r, i)| Some((*r, v.items.get(*i)?, v.states.get(*i).copied().unwrap_or_default()))).collect();
                Some((v.layer.as_ref()?.wl_surface(), v.area, (v.width, v.height), items, v.keyboard))
            })
            .collect()
    }
}
