//! The dock on the desktop (helix-desktop-spec `contracts/wayland/README.md`,
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

const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;
/// Room beside the bar for an item's name.
const TOOLTIP_ROOM_H: i32 = 36;
const TOOLTIP_ROOM_W: i32 = 240;
/// Pointer travel before a press becomes a drag.
const DRAG_START: f64 = 8.0;
/// How far from the bar a dropped favorite is removed.
const UNPIN_DISTANCE: f64 = 60.0;
const DOCK_BG: u32 = 0x001E1E24;
const BADGE: u32 = 0xFFE01B24;
/// Behind an app asking for attention.
const URGENT: u32 = 0x80F57900;
/// How long a dock revealed at the edge waits for the pointer.
const REVEAL_GRACE: f64 = 1.5;

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
}

/// One monitor's dock.
#[derive(Default)]
pub(super) struct DockView {
    layer: Option<LayerSurface>,
    width: u32,
    height: u32,
    configured: bool,
    /// The strip at the screen edge that reveals a hidden dock.
    trigger: Option<LayerSurface>,
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
    press: Option<(usize, (f64, f64), u32)>,
    drag: Option<(usize, (f64, f64))>,
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
}

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
    use gio::prelude::*;
    if id.is_empty() {
        return None;
    }
    let ids: Vec<String> = gio::AppInfo::all().iter().filter_map(|a| a.id().map(|i| i.to_string())).collect();
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
        infos
            .iter()
            .find(|(_, name, _)| want != "primary" && name == want)
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

    /// Running apps in the order they first opened (window ids grow).
    fn dock_running(&self) -> Vec<String> {
        let mut entries: Vec<_> = self
            .windows
            .mru()
            .iter()
            .filter(|e| !self.dock.settings.isolate_workspaces || e.workspace == self.windows.active || e.workspace == ALL_WORKSPACES)
            .filter(|e| !self.dock.settings.isolate_monitors || self.on_view_monitor(e.id))
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
            .filter(|e| !isolate || e.workspace == self.windows.active || e.workspace == ALL_WORKSPACES)
            .filter(|e| !self.dock.settings.isolate_monitors || self.on_view_monitor(e.id))
            .map(|e| e.id)
            .collect()
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
        use gio::prelude::*;
        let installed: Vec<String> = gio::AppInfo::all().into_iter().filter(|a| a.should_show()).filter_map(|a| a.id().map(|i| i.to_string())).collect();
        let running: Vec<String> = self.dock_running().into_iter().map(|a| desktop_id(&a).map_or(a, |d| d.trim_end_matches(".desktop").to_owned())).collect();
        let items = logic::items(&s, &crate::favorites::favorites(), &installed, &running, self.dock.mounts.len());
        let room = if s.edge.vertical() { area.3 } else { area.2 } - 2 * PANEL_HEIGHT as i32;
        let icon = logic::fit_icon_size(s.icon_size, s.icon_size_fixed, items.len(), room);
        let thickness = icon + icon / 3 + 8;
        let length = if s.extend { if s.edge.vertical() { area.3 } else { area.2 } } else { Self::dock_length(icon, &items).min(room) };
        // The bar in layout coordinates (for windows over it).
        let (ax, ay, aw, ah) = area;
        let rect = match s.edge {
            Edge::Bottom => (ax + (aw - length) / 2, ay + ah - thickness, length, thickness),
            Edge::Top => (ax + (aw - length) / 2, ay + PANEL_HEIGHT as i32, length, thickness),
            Edge::Left => (ax, ay + (ah - length) / 2, thickness, length),
            Edge::Right => (ax + aw - thickness, ay + (ah - length) / 2, thickness, length),
        };
        let windows = self.dock_windows();
        let focused_app = self.windows.mru().iter().find(|e| e.id == self.windows.focused()).map(|e| e.app_id.clone());
        let overlapped = logic::windows_hide(s.hide_mode, rect, area, s.edge, &windows, focused_app.as_deref());
        let fullscreen = self.window_geometry.get(&self.windows.focused()).is_some_and(|&(x, y, w, h, min)| !min && logic::covers((x, y, w, h), area));
        let hovered = self.dock.views[self.dock.cur].revealed || self.dock.views[self.dock.cur].pointer_on || self.dock.views[self.dock.cur].drag.is_some();
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
                layer.commit();
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
                layer.commit();
                self.dock.views[self.dock.cur].layer = Some(layer);
                self.dock.views[self.dock.cur].configured = false;
            }
        }
    }

    /// Take the dock down; with `trigger`, leave the edge strip that
    /// reveals it.
    fn dock_hide(&mut self, trigger: bool) {
        self.dock.views[self.dock.cur].layer = None;
        self.dock.views[self.dock.cur].configured = false;
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
        let (w, h) = if self.dock.settings.edge.vertical() { (1, 0) } else { (0, 1) };
        let surface = self.compositor.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(&self.qh, surface, Layer::Top, Some("helix-dock-edge"), Some(&output));
        layer.set_anchor(anchor);
        layer.set_size(w, h);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.commit();
        self.dock.views[self.dock.cur].trigger = Some(layer);
    }

    fn dock_drop_trigger(&mut self) {
        self.dock.views[self.dock.cur].trigger = None;
    }

    pub(super) fn dock_configured(&mut self, layer: &LayerSurface, size: (u32, u32)) -> bool {
        let Some(i) = self.dock.views.iter().position(|v| v.layer.as_ref() == Some(layer) || v.trigger.as_ref() == Some(layer)) else { return false };
        self.dock.cur = i;
        if self.dock.views[self.dock.cur].trigger.as_ref() == Some(layer) {
            // An empty, see-through strip that only takes the pointer.
            let (w, h) = (size.0.max(1) as i32, size.1.max(1) as i32);
            let scale = self.scales.of(layer.wl_surface());
            if let Ok((buffer, data)) = self.pool.create_buffer(physical(w as usize, scale) as i32, physical(h as usize, scale) as i32, (physical(w as usize, scale) * 4) as i32, wl_shm::Format::Argb8888) {
                data.fill(0);
                self.scales.prepare(&self.qh, layer.wl_surface(), w, h);
                if buffer.attach_to(layer.wl_surface()).is_ok() {
                    layer.commit();
                }
            }
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
            Item::Trash => "Trash".into(),
            Item::ShowApps => "Show Apps".into(),
        }
    }

    fn dock_icon(&mut self, item: &Item, px: u32) -> Option<helix_draw::Image> {
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
            Item::ShowApps => return None,
        };
        let id = match item {
            Item::App { id, .. } => Some(id.clone()),
            _ => None,
        };
        self.icon_cache
            .entry(key)
            .or_insert_with(|| {
                let source = source.or_else(|| id.and_then(|id| crate::icons::IconSource::for_app(&desktop_id(&id).map_or(id, |d| d.trim_end_matches(".desktop").to_owned()))));
                source.and_then(|s| crate::icons::load(&s, px))
            })
            .clone()
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

    /// Draw every dock.
    pub(super) fn dock_draw(&mut self) {
        for i in 0..self.dock.views.len() {
            self.dock.cur = i;
            self.dock_draw_view();
        }
        self.dock.cur = 0;
    }

    fn dock_draw_view(&mut self) {
        let Some(layer) = self.dock.views[self.dock.cur].layer.clone() else { return };
        if !self.dock.views[self.dock.cur].configured || self.dock.views[self.dock.cur].width == 0 || self.dock.views[self.dock.cur].height == 0 {
            return;
        }
        let (w, h) = (self.dock.views[self.dock.cur].width as i32, self.dock.views[self.dock.cur].height as i32);
        let scale = self.scales.of(layer.wl_surface());
        let s = self.dock.settings.clone();
        let (icon, t) = (self.dock.views[self.dock.cur].icon, self.dock.views[self.dock.cur].thickness);
        let bar = match s.edge {
            Edge::Bottom => Rect { x: 0, y: h - t, w, h: t },
            Edge::Top => Rect { x: 0, y: 0, w, h: t },
            Edge::Left => Rect { x: 0, y: 0, w: t, h },
            Edge::Right => Rect { x: w - t, y: 0, w: t, h },
        };
        self.dock.views[self.dock.cur].bar = bar;
        // What to draw, gathered first.
        let near = logic::windows_hide(logic::HideMode::AllWindows, self.dock_layout_rect(), self.dock.views[self.dock.cur].area, s.edge, &self.dock_windows(), None);
        let items = self.dock.views[self.dock.cur].items.clone();
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
                _ => (0, 0),
            })
            .collect();
        let entries: Vec<Option<crate::launcher_entry::Entry>> = items
            .iter()
            .map(|i| match i {
                Item::App { id, .. } => self.dock_entry(id).cloned(),
                _ => None,
            })
            .collect();
        let focused_app = self.windows.mru().iter().find(|e| e.id == self.windows.focused()).map(|e| e.app_id.clone()).unwrap_or_default();
        let focused_item = items.iter().position(|i| matches!(i, Item::App { id, .. } if !focused_app.is_empty() && (id.eq_ignore_ascii_case(&focused_app) || desktop_matches(&focused_app, id))));
        let hover_name = self.dock.views[self.dock.cur].hover.and_then(|i| items.get(i)).map(|i| self.dock_item_name(i));
        let (hover, drag) = (self.dock.views[self.dock.cur].hover, self.dock.views[self.dock.cur].drag);

        let Ok((buffer, data)) = self.pool.create_buffer(physical(w as usize, scale) as i32, physical(h as usize, scale) as i32, (physical(w as usize, scale) * 4) as i32, wl_shm::Format::Argb8888) else {
            return;
        };
        let mut canvas = Canvas::scaled(data, w as usize, h as usize, scale);
        canvas.fill(0);
        let text = self.text.as_ref();
        let alpha = (logic::background_opacity(&s, near) * 255.0).round() as u32;
        let radius = if s.extend { 0.0 } else { 12.0 };
        canvas.round_rect(f64::from(bar.x), f64::from(bar.y), f64::from(bar.w), f64::from(bar.h), radius, (alpha << 24) | (DOCK_BG & 0xFFFFFF));
        let vertical = s.edge.vertical();
        let slot = icon + icon / 4;
        let total = Self::dock_length(icon, &items);
        let along_len = if vertical { h } else { w };
        let mut pos = if s.extend && !s.always_center { icon / 8 } else { (along_len - total) / 2 + icon / 8 };
        let mut hits = Vec::new();
        let mut drop_marker = None;
        for (i, item) in items.iter().enumerate() {
            if i > 0 && separated(&items[i - 1], item) {
                let (sx, sy, sw, sh) = if vertical { (bar.x + 8, pos + 4, t - 16, 1) } else { (pos + 4, bar.y + 8, 1, t - 16) };
                canvas.rect(sx, sy, sw, sh, 0x60FFFFFF);
                pos += 9;
            }
            let cell = if vertical { Rect { x: bar.x, y: pos, w: t, h: slot } } else { Rect { x: pos, y: bar.y, w: slot, h: t } };
            if let Some((_, at)) = drag {
                let p = if vertical { at.1 } else { at.0 };
                if drop_marker.is_none() && p < f64::from(pos + slot / 2) {
                    drop_marker = Some(pos);
                }
            }
            let (ix, iy) = (cell.x + (cell.w - icon) / 2, cell.y + (cell.h - icon) / 2);
            if entries[i].as_ref().is_some_and(|e| e.urgent) {
                canvas.round_rect(f64::from(ix - 4), f64::from(iy - 4), f64::from(icon + 8), f64::from(icon + 8), 8.0, URGENT);
            } else if hover == Some(i) || focused_item == Some(i) {
                let tint = if hover == Some(i) { 0x40FFFFFF } else { 0x26FFFFFF };
                canvas.round_rect(f64::from(ix - 4), f64::from(iy - 4), f64::from(icon + 8), f64::from(icon + 8), 8.0, tint);
            }
            if !drag.is_some_and(|(d, _)| d == i) {
                match (item, &icons[i]) {
                    (Item::ShowApps, _) => draw_grid(&mut canvas, ix, iy, icon),
                    (_, Some(image)) => canvas.image(f64::from(ix), f64::from(iy), f64::from(icon), f64::from(icon), image),
                    (_, None) => canvas.round_rect(f64::from(ix + 4), f64::from(iy + 4), f64::from(icon - 8), f64::from(icon - 8), 8.0, 0xFF5E5C64),
                }
            }
            if let Some(progress) = entries[i].as_ref().and_then(|e| e.shown_progress()) {
                // A bar across the icon's bottom.
                let (px0, py0, pw) = (f64::from(ix + 3), f64::from(iy + icon - 7), f64::from(icon - 6));
                canvas.round_rect(px0, py0, pw, 5.0, 2.5, 0xC0202024);
                if progress > 0.0 {
                    canvas.round_rect(px0, py0, (pw * progress).max(5.0), 5.0, 2.5, ACCENT);
                }
            }
            let (windows, badge) = counts[i];
            draw_indicator(&mut canvas, s.indicator, s.edge, cell, windows);
            if badge > 0 {
                let (bx, by) = (f64::from(ix + icon) - 4.0, f64::from(iy) + 4.0);
                canvas.circle(bx, by, 9.0, BADGE);
                let label_text = if badge > 9 { "9+".to_owned() } else { badge.to_string() };
                let tw = text.map_or(6.0, |t| t.width(&label_text, 11.0));
                label(text, &mut canvas, &label_text, (bx - f64::from(tw) / 2.0) as f32, by as f32 + 4.0, 11.0, FG, 20.0);
            }
            hits.push((cell, i));
            pos += slot;
        }
        if let Some((d, at)) = drag {
            let marker = drop_marker.unwrap_or(pos);
            let (mx, my, mw, mh) = if vertical { (bar.x + 6, marker - 1, t - 12, 3) } else { (marker - 1, bar.y + 6, 3, t - 12) };
            canvas.rect(mx, my, mw, mh, ACCENT);
            if let Some(Some(image)) = icons.get(d) {
                canvas.image(at.0 - f64::from(icon) / 2.0, at.1 - f64::from(icon) / 2.0, f64::from(icon), f64::from(icon), image);
            }
        }
        // The hovered item's name, beside the bar.
        if let (Some(i), Some(name), None) = (hover, hover_name, drag) {
            if let Some(&(cell, _)) = hits.iter().find(|(_, n)| *n == i) {
                let tw = text.map_or(80.0, |t| t.width(&name, 13.0)).ceil() as i32 + 28;
                let (lw, lh) = (tw.min(if vertical { TOOLTIP_ROOM_W - 12 } else { w }), 26);
                let (lx, ly) = match s.edge {
                    Edge::Bottom => (cell.x + cell.w / 2 - lw / 2, bar.y - lh - 6),
                    Edge::Top => (cell.x + cell.w / 2 - lw / 2, bar.y + bar.h + 6),
                    Edge::Left => (bar.x + bar.w + 6, cell.y + cell.h / 2 - lh / 2),
                    Edge::Right => (bar.x - lw - 6, cell.y + cell.h / 2 - lh / 2),
                };
                let lx = lx.clamp(0, (w - lw).max(0));
                canvas.round_rect(f64::from(lx), f64::from(ly), f64::from(lw), f64::from(lh), 6.0, 0xF02E2E36);
                let base = baseline(text, ly, lh, 13.0);
                label(text, &mut canvas, &name, (lx + 12) as f32, base, 13.0, FG, (lw - 16) as f32);
            }
        }
        self.dock.views[self.dock.cur].hits = hits;
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
            Edge::Top => (ax + (aw - len) / 2, ay + PANEL_HEIGHT as i32, len, t),
            Edge::Left => (ax, ay + (ah - len) / 2, t, len),
            Edge::Right => (ax + aw - t, ay + (ah - len) / 2, t, len),
        }
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

    /// Pointer input on the dock or its edge strip; true if it was for them.
    pub(super) fn dock_pointer(&mut self, event: &PointerEvent) -> bool {
        let Some(i) = self.dock.views.iter().position(|v| v.layer.as_ref().is_some_and(|l| l.wl_surface() == &event.surface) || v.trigger.as_ref().is_some_and(|t| t.wl_surface() == &event.surface)) else { return false };
        self.dock.cur = i;
        if self.dock.views[self.dock.cur].trigger.as_ref().is_some_and(|t| t.wl_surface() == &event.surface) {
            match event.kind {
                PointerEventKind::Enter { .. } => self.dock_set_timer(self.dock.settings.show_delay, true),
                PointerEventKind::Leave { .. } if !self.dock.views[self.dock.cur].revealed => self.dock_cancel_timer(),
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
                self.dock.views[self.dock.cur].hover = under;
                self.dock_draw();
            }
            PointerEventKind::Leave { .. } => {
                self.dock.views[self.dock.cur].pointer_on = false;
                self.dock.views[self.dock.cur].hover = None;
                if self.dock.views[self.dock.cur].drag.is_none() {
                    self.dock_draw();
                    // Hide (if it should) after the delay.
                    self.dock_set_timer(self.dock.settings.hide_delay, false);
                }
            }
            PointerEventKind::Motion { .. } => {
                if let Some((item, start, button)) = self.dock.views[self.dock.cur].press {
                    let moved = (x - start.0).hypot(y - start.1);
                    if self.dock.views[self.dock.cur].drag.is_none() && button == BTN_LEFT && moved > DRAG_START && matches!(self.dock.views[self.dock.cur].items.get(item), Some(Item::App { .. })) {
                        self.dock.views[self.dock.cur].drag = Some((item, (x, y)));
                    }
                }
                if let Some(drag) = self.dock.views[self.dock.cur].drag.as_mut() {
                    drag.1 = (x, y);
                    self.dock_draw();
                } else if self.dock.views[self.dock.cur].hover != under {
                    self.dock.views[self.dock.cur].hover = under;
                    self.dock_draw();
                }
            }
            PointerEventKind::Press { button, .. } => {
                if let Some(item) = under {
                    self.dock.views[self.dock.cur].press = Some((item, (x, y), button));
                }
            }
            PointerEventKind::Release { button, .. } => {
                let press = self.dock.views[self.dock.cur].press.take();
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
        match self.dock.settings.scroll {
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

    /// Click item `index` with `action`.
    fn dock_activate(&mut self, index: usize, action: logic::ClickAction) {
        let Some(item) = self.dock.views[self.dock.cur].items.get(index).cloned() else { return };
        match item {
            Item::ShowApps => {
                if self.launcher.is_some() {
                    self.close_launcher(true);
                } else {
                    self.open_launcher(true);
                }
            }
            Item::Trash => crate::media::trash_request(false),
            Item::Mount(i) => {
                if let Some(m) = self.dock.mounts.get(i) {
                    crate::media::request(m.root.clone(), crate::media::Request::Open);
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
            Outcome::Focus(w) => self.activate_window(w),
            Outcome::MinimizeAll => self.app_windows_action(&windows, WindowAction::Minimize),
            Outcome::CloseAll => self.app_windows_action(&windows, WindowAction::Close),
            Outcome::Overview => self.open_overview(true),
            Outcome::ChooseWindow => {
                let (x, y) = self.dock_item_point(id);
                self.dock_window_menu(id, x, y);
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

    fn dock_menu_item(&mut self, label: String, target: DockTarget) -> Option<window_menu::Item> {
        self.dock.menu_targets.push(target);
        Some(window_menu::Item { label, choice: window_menu::Choice::Dock(self.dock.menu_targets.len() - 1), checked: None, active: false })
    }

    fn open_dock_menu(&mut self, x: i32, y: i32, items: Vec<Option<window_menu::Item>>) {
        if items.is_empty() {
            return;
        }
        // Above a bottom dock, beside a side dock.
        let rows: i32 = items.iter().map(|i| if i.is_some() { 32 } else { 9 }).sum::<i32>() + 8;
        let (bx, by, bw, bh) = self.dock_layout_rect();
        let (mx, my) = match self.dock.settings.edge {
            Edge::Bottom => (x - 120, by - rows - 8),
            Edge::Top => (x - 120, by + bh + 8),
            Edge::Left => (bx + bw + 8, y - 16),
            Edge::Right => (bx - 248, y - 16),
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
                let windows = self.app_windows(&id);
                for w in &windows {
                    let title = self.windows.mru().iter().find(|e| e.id == *w).map_or_else(String::new, |e| e.label().to_owned());
                    items.push(self.dock_menu_item(title, DockTarget::Window(*w)));
                }
                if !windows.is_empty() {
                    items.push(None);
                }
                let open = if windows.is_empty() { "Open" } else { "New Window" };
                items.push(self.dock_menu_item(open.into(), DockTarget::Launch(id.clone())));
                let actions = desktop_id(&id).map(|d| logic::desktop_actions(d.trim_end_matches(".desktop"))).unwrap_or_default();
                for (action, name) in actions {
                    items.push(self.dock_menu_item(name, DockTarget::Action(id.clone(), action)));
                }
                items.push(None);
                let pin = if favorite { "Unpin" } else { "Pin to Dash" };
                items.push(self.dock_menu_item(pin.into(), DockTarget::TogglePin(id.clone())));
                if which("gnome-software") {
                    items.push(self.dock_menu_item("App Details".into(), DockTarget::Details(id.clone())));
                }
                if !windows.is_empty() {
                    items.push(None);
                    let quit = if windows.len() > 1 { format!("Quit {} Windows", windows.len()) } else { "Quit".to_owned() };
                    items.push(self.dock_menu_item(quit, DockTarget::Quit(id.clone())));
                }
            }
            Item::Mount(i) => {
                let Some(m) = self.dock.mounts.get(i).cloned() else { return };
                items.push(self.dock_menu_item("Open".into(), DockTarget::OpenMount(m.root.clone())));
                let eject = if m.ejectable { "Eject" } else { "Unmount" };
                items.push(self.dock_menu_item(eject.into(), DockTarget::EjectMount(m.root)));
            }
            Item::Trash => {
                items.push(self.dock_menu_item("Open".into(), DockTarget::OpenTrash));
                if self.dock.trash_full {
                    items.push(self.dock_menu_item("Empty Trash".into(), DockTarget::EmptyTrash));
                }
            }
            Item::ShowApps => return,
        }
        self.open_dock_menu(lx, ly, items);
    }

    /// A dock menu entry was chosen.
    pub(super) fn dock_menu_choice(&mut self, index: usize) {
        let Some(target) = self.dock.menu_targets.get(index).cloned() else { return };
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

    /// `switch-to-application-N`: the Nth app of the dash.
    pub(super) fn switch_to_app(&mut self, n: usize) {
        use gio::prelude::*;
        let installed: Vec<String> = gio::AppInfo::all().into_iter().filter(|a| a.should_show()).filter_map(|a| a.id().map(|i| i.to_string())).collect();
        let settings = DockSettings { show_mounts: false, show_trash: false, show_apps_button: false, ..self.dock.settings.clone() };
        let running = self.dock_running();
        let apps: Vec<String> = logic::items(&settings, &crate::favorites::favorites(), &installed, &running, 0)
            .into_iter()
            .filter_map(|i| match i {
                Item::App { id, .. } => Some(id),
                _ => None,
            })
            .collect();
        if let Some(id) = apps.get(n.saturating_sub(1)) {
            let id = id.clone();
            self.app_click(&id, logic::ClickAction::CycleWindows);
        }
    }
}

/// Whether a separator goes between two neighbouring items.
fn separated(a: &Item, b: &Item) -> bool {
    let group = |i: &Item| match i {
        Item::ShowApps => 0,
        Item::App { favorite: true, .. } => 1,
        Item::App { favorite: false, .. } => 2,
        Item::Mount(_) | Item::Trash => 3,
    };
    group(a) != group(b)
}

/// Whether window app id `window_app` is app `id` (by its desktop file).
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

/// Running marks for `n` windows, on the screen-edge side of `cell`.
fn draw_indicator(canvas: &mut Canvas, style: logic::Indicator, edge: Edge, cell: Rect, n: usize) {
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
    for k in 0..count {
        let along = k * (mark + gap) - span / 2;
        let (x, y, w, h) = match edge {
            Edge::Bottom => (cell.x + cell.w / 2 + along, cell.y + cell.h - 6, mark, 3),
            Edge::Top => (cell.x + cell.w / 2 + along, cell.y + 3, mark, 3),
            Edge::Left => (cell.x + 3, cell.y + cell.h / 2 + along, 3, mark),
            Edge::Right => (cell.x + cell.w - 6, cell.y + cell.h / 2 + along, 3, mark),
        };
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
