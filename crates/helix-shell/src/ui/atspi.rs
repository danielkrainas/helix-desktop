//! The shell UI on the accessibility bus (helixdesktop/spec
//! `contracts/dbus/org.a11y.atspi.md`): which surfaces are windows of the
//! shell's application, and what each shows, from the state the painting
//! uses. Surfaces whose paint code computes its texts (the lock screen,
//! dialogs, quick settings) record their nodes while painting; the others
//! are described from their state and the places their last paint left.
//!
//! An action on a node (Click, Focus on a text field, a slider's value)
//! is a click at the node's place on its surface, so it does what a click
//! does.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use smithay_client_toolkit::reexports::calloop::channel;
use smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface;
use smithay_client_toolkit::reexports::client::Proxy;
use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerEventKind};

use super::notify::ListHit;
use super::*;
use crate::a11y::publish::{Event, Publisher};
use crate::a11y::{Node, Role, Window};
use helix_i18n::tr;
use helix_i18n::trf;

impl From<Rect> for crate::a11y::Rect {
    fn from(r: Rect) -> Self {
        Self::new(r.x, r.y, r.w, r.h)
    }
}

/// Window keys.
const LOCK: &str = "lock";
const PANEL: &str = "panel";
const DIALOG: &str = "dialog";
const SWITCHER: &str = "switcher";
const LAUNCHER: &str = "launcher";
const QUICK: &str = "quick";
const BANNER: &str = "banner";
const NOTIFICATIONS: &str = "notifications";
const DOCK: &str = "dock:";
const OSD: &str = "osd";
const MENU: &str = "menu";
const OVERVIEW: &str = "overview:";
const WORKSPACE_POPUP: &str = "workspace-popup";
const OSK: &str = "osk";
const CLICK_TYPE: &str = "click-type";
const DESKTOP: &str = "desktop";
const PICKER: &str = "screenshot";
const BREAK: &str = "break";
const PANEL_EDITOR: &str = "panel-editor";
const EXTENSIONS: &str = "extensions";
const TOOLTIP: &str = "tooltip";
const MONITOR_LABEL: &str = "monitor:";

/// The launcher's (search and app grid) window name on AT-SPI.
pub(super) fn launcher_title() -> &'static str {
    tr("Launcher")
}

/// The window overview's window name on AT-SPI (one per monitor), told
/// apart from the launcher's.
pub(super) fn overview_title() -> &'static str {
    tr("Window Overview")
}

/// What the app grid last showed: the cells by key (the "Recent" row's
/// first), the open folder's cells, the selected item, the page dots, the
/// page, how many of the cells are the "Recent" row's.
pub(super) type GridShown<'a> = (&'a [(Rect, String)], &'a [(Rect, String)], Option<usize>, &'a [(Rect, usize)], usize, usize);

/// The app grid's nodes: the open folder's apps, else the "Recent" row
/// ("Recent Apps") and the page ("Applications") as groups of buttons,
/// and the pages.
pub(super) fn grid_nodes(model: &crate::launcher::Launcher, (cells, folder_cells, selected, dots, page, row): GridShown<'_>) -> Vec<Node> {
    let name = |key: &str| {
        let id = super::app_grid::item_id(key);
        model.apps.iter().find(|a| a.id == id).map(|a| a.name.clone()).or_else(|| model.folders.iter().find(|f| f.id == id).map(|f| f.name.clone())).unwrap_or_default()
    };
    let mut out = Vec::new();
    if let Some(folder) = model.open_folder.and_then(|i| model.folders.get(i)) {
        let current = selected.and_then(|k| folder.apps.get(k));
        let apps = folder_cells.iter().map(|(r, id)| Node::button(name(id), (*r).into()).id(id.clone()).selected(current == Some(id)).focused(current == Some(id)));
        out.push(Node::new(Role::Group, &folder.name).id(format!("folder:{}", folder.id)).children(apps));
        return out;
    }
    let row = row.min(cells.len());
    let button = |i: usize, (r, key): &(Rect, String)| Node::button(name(key), (*r).into()).id(key.clone()).selected(selected == Some(i)).focused(selected == Some(i));
    if row > 0 {
        let recent = cells[..row].iter().enumerate().map(|(i, c)| button(i, c));
        out.push(Node::new(Role::Group, tr("Recent Apps")).id("recent").children(recent));
    }
    let apps = cells[row..].iter().enumerate().map(|(i, c)| button(row + i, c));
    out.push(Node::new(Role::Group, tr("Applications")).id("grid").children(apps));
    if dots.len() > 1 {
        let pages = dots.iter().map(|(r, p)| Node::button(trf!("Page {}", p + 1), (*r).into()).selected(*p == page));
        out.push(Node::new(Role::Group, tr("Pages")).id("pages").children(pages));
    }
    out
}
/// A dock shown: its surface, monitor area, size, its items' places and
/// what they show, and the item with the keyboard focus.
pub(super) type DockShown<'a> = (&'a WlSurface, (i32, i32, i32, i32), (u32, u32), Vec<(Rect, &'a crate::dock::Item, crate::dock::ItemState)>, Option<usize>);
/// A surface and its place on the screen: x, y, width, height.
pub(super) type SurfaceAt<'a> = (&'a LayerSurface, (i32, i32, u32, u32));
/// The notification list shown: its surface, size, and places.
pub(super) type ListShown<'a> = (&'a WlSurface, (u32, u32), &'a [(Rect, ListHit)], Option<&'a ListHit>);

#[derive(Default)]
pub(super) struct AtSpi {
    /// `None` until started, and with `HELIX_NO_ATSPI` set.
    publisher: Option<Publisher<WlSurface>>,
    /// What paint code recorded for its surface.
    recorded: Vec<(WlSurface, Vec<Node>)>,
    /// App names for the dock (reading them is slow), and when.
    app_names: Option<(HashMap<String, String>, Instant)>,
    /// The range a client is about to change in a text field (field id,
    /// start, end, when), from `EditableText`.
    edit_range: Option<(String, usize, usize, Instant)>,
}

impl Ui {
    /// Serve the shell's windows on the accessibility bus from now on.
    pub(super) fn atspi_start(&mut self, handle: &LoopHandle<'static, Ui>) {
        if std::env::var_os("HELIX_NO_ATSPI").is_some() {
            tracing::info!("HELIX_NO_ATSPI set; the shell stays off the accessibility bus");
            return;
        }
        let (tx, rx) = channel::channel::<Event>();
        if let Err(err) = handle.insert_source(rx, |event, _, ui| {
            if let channel::Event::Msg(event) = event {
                ui.atspi_event(event);
            }
        }) {
            tracing::warn!(%err, "cannot watch accessibility requests; the shell stays off the accessibility bus");
            return;
        }
        let tx = Mutex::new(tx);
        self.atspi.publisher = Some(Publisher::new(Arc::new(move |event| {
            if let Ok(tx) = tx.lock() {
                let _ = tx.send(event);
            }
        })));
    }

    /// Keep what paint code describes for `surface` (only while somebody
    /// reads the trees).
    pub(super) fn atspi_record(&mut self, surface: &WlSurface, nodes: Vec<Node>) {
        if !self.atspi.publisher.as_ref().is_some_and(|p| p.any_active()) {
            return;
        }
        self.atspi.recorded.retain(|(s, _)| s != surface && s.is_alive());
        self.atspi.recorded.push((surface.clone(), nodes));
    }

    fn recorded(&self, surface: &WlSurface) -> Vec<Node> {
        self.atspi.recorded.iter().find(|(s, _)| s == surface).map(|(_, n)| n.clone()).unwrap_or_default()
    }

    /// Bring the published windows in line with the UI (after each
    /// round of events).
    pub(super) fn atspi_sync(&mut self) {
        let Some(mut publisher) = self.atspi.publisher.take() else { return };
        let named = !self.dock_a11y().is_empty() || self.overview.is_some() || self.window_menu.is_some();
        if publisher.any_active() && named {
            let stale = self.atspi.app_names.as_ref().is_none_or(|(_, at)| at.elapsed() > Duration::from_secs(30));
            if stale {
                self.atspi.app_names = Some((crate::windows::app_names(), Instant::now()));
            }
        }
        let shown = self.atspi_shown();
        publisher.sync(shown, |key| self.atspi_window(key));
        self.atspi.publisher = Some(publisher);
    }

    fn atspi_event(&mut self, event: Event) {
        let Some(publisher) = self.atspi.publisher.as_mut() else { return };
        let activated = matches!(event, Event::Activated(_));
        if let Some((surface, target, request)) = publisher.event(event) {
            // Only the user at the machine answers an agent prompt: an
            // agent may drive the accessibility bus (README.md of
            // contracts/agent-access, "Consent").
            if self.is_agent_prompt(&surface) {
                tracing::warn!(?request, "accessibility action on the agent prompt refused");
                return;
            }
            if target.text_input && self.atspi_edit(&surface, &target, &request) {
                return;
            }
            // A window picture's "Move to …" in the overview.
            if let (accesskit::Action::CustomAction, Some(accesskit::ActionData::CustomAction(action))) = (request.action, request.data.as_ref()) {
                if self.overview_action(&target.id, *action) {
                    return;
                }
            }
            match crate::a11y::tree::click_point(&target, request.action, request.data.as_ref()) {
                Some(point) => self.atspi_click(surface, point),
                None => tracing::debug!(?request, "accessibility action not supported"),
            }
        } else if activated {
            // Paint again, recording the nodes.
            self.redraw_all();
        }
    }

    /// `EditableText` on a text field: the client selects the range it
    /// changes (`SetTextSelection`), then sets the whole new text
    /// (`SetValue`). True when `request` was such a request.
    fn atspi_edit(&mut self, surface: &WlSurface, target: &crate::a11y::tree::Target, request: &accesskit::ActionRequest) -> bool {
        use accesskit::{Action, ActionData};
        match (request.action, request.data.as_ref()) {
            (Action::SetTextSelection, Some(ActionData::SetTextSelection(sel))) => {
                let (a, b) = (sel.anchor.character_index, sel.focus.character_index);
                self.atspi.edit_range = Some((target.id.clone(), a.min(b), a.max(b), Instant::now()));
                true
            }
            (Action::CustomAction, Some(ActionData::CustomAction(action))) => {
                // CopyText, CutText, PasteText: on the range selected just before.
                let Some(op) = super::clipboard::Op::of_action(*action) else { return false };
                let range = self.atspi.edit_range.take().filter(|(id, .., at)| *id == target.id && at.elapsed() < Duration::from_secs(1)).map(|(_, s, e, _)| (s, e));
                self.field_clipboard(surface, &target.id, op, range);
                true
            }
            (Action::SetValue, Some(ActionData::Value(value))) => {
                // The range applies to the value set right after it.
                let range = self.atspi.edit_range.take().filter(|(id, .., at)| *id == target.id && at.elapsed() < Duration::from_secs(1)).map(|(_, s, e, _)| (s, e));
                self.atspi_set_text(surface, &target.id, value, range);
                true
            }
            _ => false,
        }
    }

    /// Field `id` on `surface` now holds `value` (see [`edited`]).
    pub(super) fn atspi_set_text(&mut self, surface: &WlSurface, id: &str, value: &str, range: Option<(usize, usize)>) {
        if self.lock.session.is_some() {
            let on_lock = self.lock_primary().is_some_and(|v| v.surface.wl_surface() == surface);
            if id == "password" && on_lock && !self.lock.checking && self.lock.revealed {
                let mut new = edited(self.lock.password.as_str(), true, value, range);
                self.lock.password.set(&new);
                wipe(&mut new);
                self.lock.failed = false;
                self.lock.focus = None;
                self.draw_lock();
            }
            return;
        }
        if let Some(view) = self.launcher.as_mut().filter(|v| v.layer.wl_surface() == surface) {
            if id == "search" {
                view.model.set_query(value);
                self.search_changed();
                self.draw_launcher();
            }
            return;
        }
        if self.dialog.as_ref().is_some_and(|d| d.layer.wl_surface() == surface) {
            if self.dialog.as_ref().is_some_and(|d| matches!(d.state.kind, DialogKind::Files(_))) {
                self.files_set_text(id, value, range);
            } else {
                self.prompt_set_text(id, value, range);
            }
        }
    }

    /// A click at `point` on `surface`, as from the pointer.
    fn atspi_click(&mut self, surface: WlSurface, position: (f64, f64)) {
        // Layout coordinates; the pointer path mirrors them back.
        let position = super::mirror::shown(&surface, position);
        let at = |kind| PointerEvent { surface: surface.clone(), position, kind };
        self.pointer_events(&[
            at(PointerEventKind::Enter { serial: 0 }),
            at(PointerEventKind::Motion { time: 0 }),
            at(PointerEventKind::Press { time: 0, button: BTN_LEFT, serial: 0 }),
            at(PointerEventKind::Release { time: 0, button: BTN_LEFT, serial: 0 }),
            at(PointerEventKind::Leave { serial: 0 }),
        ]);
    }

    /// The surfaces that are windows (key, surface): only the lock screen
    /// while locked.
    fn atspi_shown(&self) -> Vec<(String, WlSurface)> {
        let mut out = Vec::new();
        if self.lock.session.is_some() {
            if let Some(view) = self.lock_primary() {
                out.push((LOCK.to_owned(), view.surface.wl_surface().clone()));
            }
            return out;
        }
        let mut add = |key: &str, surface: &WlSurface| out.push((key.to_owned(), surface.clone()));
        if self.panel_configured {
            add(PANEL, self.panel.wl_surface());
        }
        for (i, (surface, ..)) in self.dock_a11y().into_iter().enumerate() {
            add(&format!("{DOCK}{i}"), surface);
        }
        if let Some(view) = self.banner.as_ref().filter(|v| v.configured) {
            add(BANNER, view.layer.wl_surface());
        }
        if let Some((surface, _)) = self.osd_a11y() {
            add(OSD, surface);
        }
        if let Some((surface, ..)) = self.notification_list_a11y() {
            add(NOTIFICATIONS, surface);
        }
        if let Some(view) = self.launcher.as_ref().filter(|v| v.configured) {
            add(LAUNCHER, view.layer.wl_surface());
        }
        if let Some(o) = self.power_menu.as_ref().filter(|o| o.configured) {
            add(QUICK, o.layer.wl_surface());
        }
        if let Some(o) = self.switcher.as_ref().filter(|o| o.configured) {
            add(SWITCHER, o.layer.wl_surface());
        }
        if let Some(o) = self.dialog.as_ref().filter(|o| o.configured) {
            add(DIALOG, o.layer.wl_surface());
        }
        for (i, (layer, _)) in self.overview_a11y().into_iter().enumerate() {
            add(&format!("{OVERVIEW}{i}"), layer.wl_surface());
        }
        if let Some((layer, ..)) = self.window_menu.as_ref().and_then(|m| m.a11y_surface()) {
            add(MENU, layer.wl_surface());
        }
        if let Some((layer, ..)) = self.workspace_popup_a11y() {
            add(WORKSPACE_POPUP, layer.wl_surface());
        }
        if let Some((layer, ..)) = self.osk_a11y() {
            add(OSK, layer.wl_surface());
        }
        if let Some((layer, ..)) = self.click_type_a11y() {
            add(CLICK_TYPE, layer.wl_surface());
        }
        if let Some((layer, ..)) = self.desktop_icons_a11y() {
            add(DESKTOP, layer.wl_surface());
        }
        if let Some((layer, _)) = self.picker_a11y() {
            add(PICKER, layer.wl_surface());
        }
        if let Some((layer, _)) = self.break_a11y() {
            add(BREAK, layer.wl_surface());
        }
        if let Some((layer, _)) = self.panel_editor_a11y() {
            add(PANEL_EDITOR, layer.wl_surface());
        }
        if let Some((layer, _)) = self.extensions_dialog_a11y() {
            add(EXTENSIONS, layer.wl_surface());
        }
        if let Some((layer, ..)) = self.panel_tooltip_a11y() {
            add(TOOLTIP, layer.wl_surface());
        }
        for (i, label) in self.monitor_labels.iter().enumerate().filter(|(_, l)| l.configured && l.width > 0) {
            add(&format!("{MONITOR_LABEL}{i}"), label.layer.wl_surface());
        }
        out
    }

    /// The lock surface published: the one with the prompt.
    pub(super) fn lock_primary(&self) -> Option<&LockView> {
        let origin = self.lock_prompt_origin()?;
        self.lock.surfaces.iter().find(|v| v.origin == origin && v.width > 0)
    }

    /// The window that has the keyboard.
    fn atspi_active(&self) -> String {
        if let Some(i) = self.dock_keyboard_view().filter(|_| self.lock.session.is_none()) {
            // (The views shown are published in order: dock:N.)
            return format!("{DOCK}{i}");
        }
        // As keys go (see `press_key`).
        let key = if self.lock.session.is_some() {
            LOCK
        } else if self.window_menu.is_some() {
            MENU
        } else if self.picker.is_some() {
            PICKER
        } else if self.dialog.is_some() {
            DIALOG
        } else if self.switcher.is_some() {
            SWITCHER
        } else if self.overview.is_some() {
            return format!("{OVERVIEW}0");
        } else if self.break_screen.is_some() {
            BREAK
        } else if self.panel_editor.is_some() {
            PANEL_EDITOR
        } else if self.extensions_dialog.is_some() {
            EXTENSIONS
        } else if self.launcher.is_some() {
            LAUNCHER
        } else if self.power_menu.is_some() {
            QUICK
        } else if self.notify.view.is_some() {
            NOTIFICATIONS
        } else if self.panel_focus.is_some() {
            PANEL
        } else if self.banner.as_ref().is_some_and(|b| b.focused) {
            BANNER
        } else {
            ""
        };
        key.to_owned()
    }

    /// A full-output overlay of the primary monitor, `w`×`h`.
    fn overlay_bounds(&self, w: u32, h: u32) -> crate::a11y::Rect {
        crate::a11y::Rect::new(self.primary_area.0, self.primary_area.1, w as i32, h as i32)
    }

    /// What window `key` shows now.
    fn atspi_window(&self, key: &str) -> Option<Window> {
        let active = self.atspi_active() == key;
        let (title, bounds, children) = match key {
            LOCK => {
                let view = self.lock_primary()?;
                let bounds = crate::a11y::Rect::new(view.origin.0, view.origin.1, view.width as i32, view.height as i32);
                (tr("Lock Screen").to_owned(), bounds, self.recorded(view.surface.wl_surface()))
            }
            PANEL => (tr("Top Bar").to_owned(), self.overlay_bounds(self.panel_width, panel_height()), self.panel_nodes()),
            DIALOG => {
                let o = self.dialog.as_ref()?;
                let nodes = self.recorded(o.layer.wl_surface());
                let title = nodes.first().map_or_else(|| tr("Dialog").to_owned(), |n| n.name.clone());
                (title, self.overlay_bounds(o.width, o.height), nodes)
            }
            QUICK => {
                let o = self.power_menu.as_ref()?;
                (tr("System Menu").to_owned(), self.overlay_bounds(o.width, o.height), self.recorded(o.layer.wl_surface()))
            }
            SWITCHER => {
                let o = self.switcher.as_ref()?;
                let (title, list) = self.switcher_nodes(o);
                (title, self.overlay_bounds(o.width, o.height), list)
            }
            LAUNCHER => {
                let view = self.launcher.as_ref()?;
                (launcher_title().to_owned(), self.overlay_bounds(view.width, view.height), self.launcher_nodes(view))
            }
            OSD => {
                // A live status: each new level or label is announced.
                let (_, (w, h)) = self.osd_a11y()?;
                let (x, y, aw, ah) = self.primary_area;
                let bounds = crate::a11y::Rect::new(x + (aw - w as i32) / 2, y + ah - h as i32 - 72, w as i32, h as i32);
                let words = self.osd_words().unwrap_or_default();
                (tr("On-screen display").to_owned(), bounds, vec![Node::new(Role::Status, words).id("osd").live()])
            }
            BANNER => {
                let view = self.banner.as_ref()?;
                let (x, y, w, _) = self.primary_area;
                let bounds = crate::a11y::Rect::new(x + (w - view.width as i32) / 2, y + 8, view.width as i32, view.height as i32);
                (tr("Notification").to_owned(), bounds, vec![self.banner_node(view), banner_said(&view.data)])
            }
            NOTIFICATIONS => {
                let (_, (w, h), hits, focus) = self.notification_list_a11y()?;
                (tr("Notifications").to_owned(), self.overlay_bounds(w, h), self.notification_list_nodes(hits, focus))
            }
            MENU => {
                let menu = self.window_menu.as_ref()?;
                let (_, (x, y, w, h)) = menu.a11y_surface()?;
                let title = self.menu_title(menu);
                let node = menu.a11y(&title);
                (title, crate::a11y::Rect::new(x, y, w as i32, h as i32), vec![node])
            }
            WORKSPACE_POPUP => {
                // Centered on the primary monitor.
                let (_, (w, h), nodes) = self.workspace_popup_a11y()?;
                let (x, y, aw, ah) = self.primary_area;
                (tr("Workspace Switcher").to_owned(), crate::a11y::Rect::new(x + (aw - w as i32) / 2, y + (ah - h as i32) / 2, w as i32, h as i32), nodes)
            }
            OSK => {
                // Along the bottom of the primary monitor.
                let (_, (w, h), nodes) = self.osk_a11y()?;
                let (x, y, _, ah) = self.primary_area;
                (tr("On-screen Keyboard").to_owned(), crate::a11y::Rect::new(x, y + ah - h as i32, w as i32, h as i32), nodes)
            }
            CLICK_TYPE => {
                // At the right edge of the primary monitor, 8 in.
                let (_, (w, h), nodes) = self.click_type_a11y()?;
                let (x, y, aw, ah) = self.primary_area;
                (tr("Click Type").to_owned(), crate::a11y::Rect::new(x + aw - w as i32 - 8, y + (ah - h as i32) / 2, w as i32, h as i32), nodes)
            }
            DESKTOP => {
                let (_, (w, h), nodes) = self.desktop_icons_a11y()?;
                (tr("Desktop").to_owned(), self.overlay_bounds(w, h), nodes)
            }
            PICKER => {
                let (layer, (x, y, w, h)) = self.picker_a11y()?;
                (tr("Screenshot").to_owned(), crate::a11y::Rect::new(x, y, w as i32, h as i32), self.recorded(layer.wl_surface()))
            }
            BREAK => {
                let (layer, (w, h)) = self.break_a11y()?;
                let nodes = self.recorded(layer.wl_surface());
                let title = nodes.first().map_or_else(|| tr("Break").to_owned(), |n| n.name.clone());
                (title, self.overlay_bounds(w, h), nodes)
            }
            PANEL_EDITOR => {
                // Below the panel, which stays uncovered.
                let (layer, (w, h)) = self.panel_editor_a11y()?;
                let (x, y, ..) = self.primary_area;
                (tr("Customize Panel").to_owned(), crate::a11y::Rect::new(x, y + panel_height() as i32, w as i32, h as i32), self.recorded(layer.wl_surface()))
            }
            EXTENSIONS => {
                let (layer, (w, h)) = self.extensions_dialog_a11y()?;
                (tr("Extensions").to_owned(), self.overlay_bounds(w, h), self.recorded(layer.wl_surface()))
            }
            TOOLTIP => {
                let (_, bounds, nodes) = self.panel_tooltip_a11y()?;
                (tr("Tooltip").to_owned(), bounds, nodes)
            }
            overview if overview.starts_with(OVERVIEW) => {
                let index: usize = overview.strip_prefix(OVERVIEW)?.parse().ok()?;
                let (layer, (x, y, w, h)) = self.overview_a11y().get(index).copied()?;
                let names = self.atspi.app_names.as_ref().map(|(n, _)| n);
                (overview_title().to_owned(), crate::a11y::Rect::new(x, y, w as i32, h as i32), self.overview_a11y_nodes(layer, names))
            }
            label if label.starts_with(MONITOR_LABEL) => {
                // Top left of its monitor, below the panel.
                let index: usize = label.strip_prefix(MONITOR_LABEL)?.parse().ok()?;
                let o = self.monitor_labels.get(index)?;
                let (number, (x, y), connector) = &o.state;
                let bounds = crate::a11y::Rect::new(x + 12, y + panel_height() as i32 + 12, o.width as i32, o.height as i32);
                let node = Node::label(number.to_string()).id("number").describe(connector.as_str()).at(crate::a11y::Rect::new(0, 0, o.width as i32, o.height as i32));
                (trf!("Monitor {}", number), bounds, vec![node])
            }
            dock => {
                let index: usize = dock.strip_prefix(DOCK)?.parse().ok()?;
                let docks = self.dock_a11y();
                let (_, area, (w, h), items, keyboard) = docks.get(index)?;
                let (w, h) = (*w as i32, *h as i32);
                let (ax, ay, aw, ah) = *area;
                let (x, y) = match self.dock.settings.edge {
                    crate::dock::Edge::Bottom => (ax + (aw - w) / 2, ay + ah - h),
                    crate::dock::Edge::Top => (ax + (aw - w) / 2, ay),
                    crate::dock::Edge::Left => (ax, ay + (ah - h) / 2),
                    crate::dock::Edge::Right => (ax + aw - w, ay + (ah - h) / 2),
                };
                (tr("Dock").to_owned(), crate::a11y::Rect::new(x, y, w, h), self.dock_nodes(items, *keyboard))
            }
        };
        Some(Window { title, bounds, active, children })
    }

    // ----- Panel -------------------------------------------------------------

    /// The panel's items where the last paint put them.
    fn panel_nodes(&self) -> Vec<Node> {
        use crate::panel_layout::{Item, Slot};
        let tray = self.tray_shown();
        let controls = panel_controls(&self.panel_placed, tray.len());
        let focus = self.panel_focus.and_then(|i| controls.get(i).copied());
        let focused = |slot: Slot, x: i32| focus.is_some_and(|(s, fx, _)| s == slot && fx == x);
        let h = panel_height() as i32;
        let mut out = Vec::new();
        for &(slot, x, w) in &self.panel_placed {
            if w <= 0 {
                continue;
            }
            let r = crate::a11y::Rect::new(x, 0, w, h);
            let node = match slot {
                Slot::Item(Item::Apps) | Slot::Item(Item::Workspaces) => {
                    let which = trf!("Workspace {} of {}", self.windows.active + 1, self.windows.count.max(1));
                    let name = if slot == Slot::Item(Item::Apps) { APPS_LABEL.to_owned() } else { tr("Workspaces").to_owned() };
                    Node::button(name, r).toggled(self.launcher.is_some()).describe(which)
                }
                Slot::Item(Item::Clock) => {
                    let clock = self.shown_minute.clone().unwrap_or_else(clock_text);
                    let what = if self.unseen_dot() { tr("Notifications and calendar, unread notifications") } else { tr("Notifications and calendar") };
                    Node::button(clock, r).expanded(self.notify.view.is_some()).describe(what)
                }
                Slot::Item(Item::Tray) => {
                    for (i, item) in tray.iter().enumerate() {
                        let ix = x + i as i32 * (status::ICON + tray::TRAY_GAP);
                        let name = [&item.title, &item.tooltip_title].into_iter().find(|s| !s.trim().is_empty()).map_or_else(|| item.key.clone(), |s| s.trim().to_owned());
                        let button = Node::button(name, crate::a11y::Rect::new(ix, 0, status::ICON, h)).id(item.key.clone());
                        out.push(button.describe(item.tooltip_text.trim()).focused(focused(slot, ix)));
                    }
                    continue;
                }
                Slot::Item(Item::Accessibility) => Node::button(tr("Accessibility"), r),
                Slot::Item(Item::Keyboard) => Node::button(self.source_label.clone().unwrap_or_default(), r).describe(tr("Input source")),
                // One system button (status icons and power).
                Slot::Item(Item::Status) | Slot::Item(Item::Power) => Node::button(tr("System"), r).describe(status_words(&self.status)).expanded(self.power_menu.is_some()),
                Slot::Ext(i) => {
                    let Some(item) = self.ext.panel.get(i) else { continue };
                    let name = [&item.accessible_name, &item.label, &item.tooltip, &item.name].into_iter().find(|s| !s.is_empty()).cloned().unwrap_or_default();
                    // Its tooltip, unless that is its name.
                    let about = if item.tooltip.trim() == name { String::new() } else { item.tooltip.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("\n") };
                    Node::button(name, r).id(item.full_id.clone()).describe(about)
                }
                // The agent indicator: a button opening its menu.
                Slot::Agents => {
                    let names: Vec<String> = self.agents_active.iter().map(|a| if a.name.is_empty() { a.agent.clone() } else { format!("{} ({})", a.name, a.agent) }).collect();
                    Node::button(tr("Agents active"), r).id("agents").describe(names.join("\n")).expanded(!self.agent_menu.is_empty() && self.window_menu.is_some())
                }
                Slot::Privacy => {
                    if self.sharing {
                        out.push(Node::label(tr("Sharing")).id("sharing").at(crate::a11y::Rect::new(x, 0, sharing_w(), h)));
                    }
                    if let Some(elapsed) = self.recording_elapsed() {
                        let rx = recording_x(x, self.sharing);
                        // "Stop recording", described by how long it has run.
                        out.push(Node::button(tr("Stop recording"), crate::a11y::Rect::new(rx, 0, w - (rx - x), h)).id("recording").describe(elapsed));
                    }
                    continue;
                }
            };
            let id = match slot {
                Slot::Item(item) => item.id().to_owned(),
                _ => String::new(),
            };
            let node = if id.is_empty() || !node.id.is_empty() { node } else { node.id(id) };
            out.push(node.focused(focused(slot, x)));
        }
        out
    }

    // ----- Menus -------------------------------------------------------------

    /// A shell menu's name: what it is for (the status icon, extension
    /// item, app, input sources, accessibility, the panel, a window).
    fn menu_title(&self, menu: &window_menu::WindowMenu) -> String {
        use window_menu::Choice;
        let has = |f: &dyn Fn(Choice) -> bool| menu.items().iter().flatten().any(|i| f(i.choice));
        if let Some((key, _)) = &self.tray_menu {
            if let Some(item) = self.tray.iter().find(|i| &i.key == key) {
                let name = [&item.title, &item.tooltip_title].into_iter().find(|s| !s.trim().is_empty()).map_or_else(|| item.key.clone(), |s| s.trim().to_owned());
                return name;
            }
        }
        if let Some((full_id, ..)) = &self.ext_menu {
            let panel = self.ext.panel.iter().find(|p| &p.full_id == full_id).map(|i| [&i.accessible_name, &i.label, &i.name].into_iter().find(|s| !s.is_empty()).cloned().unwrap_or_default());
            let dock = self.ext.dock.iter().find(|d| &d.full_id == full_id).map(|d| if d.label.is_empty() { d.name.clone() } else { d.label.clone() });
            if let Some(name) = panel.or(dock).filter(|n| !n.is_empty()) {
                return name;
            }
        }
        if has(&|c| matches!(c, Choice::InputSource(_) | Choice::ImProperty(_))) {
            return tr("Input Sources").to_owned();
        }
        if has(&|c| matches!(c, Choice::A11y(_))) {
            return tr("Accessibility").to_owned();
        }
        if has(&|c| matches!(c, Choice::StopAgent(_))) {
            return tr("Agents").to_owned();
        }
        if has(&|c| matches!(c, Choice::CustomizePanel | Choice::Extensions)) {
            return tr("Top Bar").to_owned();
        }
        if has(&|c| matches!(c, Choice::Settings(_))) {
            return tr("Background").to_owned();
        }
        if has(&|c| matches!(c, Choice::DesktopIcon(_))) {
            if let Some(name) = self.desktop_icon_menu_name() {
                return name;
            }
        }
        if has(&|c| matches!(c, Choice::Dock(_))) {
            use super::dock::DockTarget;
            let names = self.atspi.app_names.as_ref().map(|(n, _)| n);
            let app_name = |id: &str| {
                let key = id.strip_suffix(".desktop").unwrap_or(id).to_lowercase();
                names.and_then(|n| n.get(&key)).cloned().unwrap_or_else(|| id.to_owned())
            };
            for target in &self.dock.menu_targets {
                match target {
                    DockTarget::Launch(app) | DockTarget::TogglePin(app) | DockTarget::Details(app) | DockTarget::Quit(app) | DockTarget::Previews(app) | DockTarget::Action(app, _) => return app_name(app),
                    DockTarget::OpenTrash | DockTarget::EmptyTrash => return tr("Trash").to_owned(),
                    DockTarget::OpenMount(root) | DockTarget::EjectMount(root) => {
                        if let Some(m) = self.dock.mounts.iter().find(|m| &m.root == root) {
                            return m.name.clone();
                        }
                    }
                    _ => {}
                }
            }
            return tr("Windows").to_owned();
        }
        tr("Window Menu").to_owned()
    }

    // ----- Window switcher ---------------------------------------------------

    fn switcher_nodes(&self, o: &Overlay<SwitcherState>) -> (String, Vec<Node>) {
        let switcher = &o.state.switcher;
        let title = match &o.state.kind {
            SwitchKind::InputSources(_) => tr("Input Sources"),
            SwitchKind::Windows { apps: None } if self.switcher_mode == Some(SwitchMode::Apps) => tr("Applications"),
            SwitchKind::Windows { .. } => tr("Windows"),
        };
        let entries = o.hits.iter().filter_map(|(r, hit)| {
            let OverlayHit::Window(id) = hit else { return None };
            let index = switcher.entries.iter().position(|e| e.id == *id)?;
            let entry = &switcher.entries[index];
            let on = index == switcher.selected;
            Some(Node::new(Role::ListBoxOption, &entry.label).id(format!("{id}")).describe(&entry.detail).at((*r).into()).click().selected(on).focused(on))
        });
        (title.to_owned(), vec![Node::new(Role::ListBox, title).id("list").children(entries)])
    }

    // ----- Launcher ----------------------------------------------------------

    fn launcher_nodes(&self, view: &LauncherView) -> Vec<Node> {
        let model = &view.model;
        let field = Rect { h: helix_draw::theme::grow(super::SEARCH_H), ..view.content };
        let results = !model.query.is_empty();
        let (_, _, selected, _, _, _) = view.grid.a11y();
        let grid_focus = !results && selected.is_some();
        // Painted from SEARCH_TEXT_X at the Entry size (see `search_field`).
        let advances = self.text.as_ref().map(|t| t.advances(&model.query, size::ENTRY)).unwrap_or_default();
        let chars = crate::a11y::Chars { x: (field.x + super::SEARCH_TEXT_X) as f32, y: field.y, h: field.h, advances };
        let search = Node::text_field(Role::SearchInput, tr("Search"), &model.query, field.into()).id("search").chars(chars);
        let mut out = vec![search.focused(!results && !grid_focus)];
        if results {
            out.push(results_node(model, &view.rows));
        } else {
            out.extend(grid_nodes(model, view.grid.a11y()));
        }
        out
    }

    // ----- Notifications -----------------------------------------------------

    fn banner_node(&self, view: &BannerView) -> Node {
        let d = &view.data;
        let body = if d.markup { crate::markup::plain(&d.body) } else { d.body.clone() };
        let body_r = view.hits.iter().find(|(_, h)| *h == BannerHit::Body).map(|(r, _)| (*r).into()).unwrap_or_default();
        // The body is a line of its own (as a description, Orca read it
        // twice when the banner had the focus).
        let mut note = Node::new(Role::Alert, &d.summary).id(format!("note:{}", d.id)).at(body_r).click().focused(view.focused);
        if !d.app_name.is_empty() {
            note = note.child(Node::label(&d.app_name).id("app"));
        }
        if !body.is_empty() {
            note = note.child(Node::label(body).id("body"));
        }
        for (r, hit) in &view.hits {
            let r = crate::a11y::Rect::from(*r);
            if *hit == BannerHit::Body {
                continue;
            }
            note = note.child(match hit {
                BannerHit::Body | BannerHit::Close => Node::button(tr("Close"), r).id("close"),
                BannerHit::Button(key) => Node::button(d.buttons.iter().find(|(k, _)| k == key).map_or(key.as_str(), |(_, l)| l.as_str()), r).id(format!("button:{key}")),
                BannerHit::Link(uri) => Node::new(Role::Link, uri).at(r).click(),
            });
        }
        note
    }

}

/// What a notification banner announces, as a live line beside it: the
/// app, the summary and the body in one announcement (screen readers
/// drop a second one that comes at once).
/// The launcher's search results: the rows shown (`rows`, painted places
/// and result indexes), in a list that says how many results there are
/// and where each row is among them, so that a screen reader reads "3 of
/// 25" whether or not the rest are scrolled into sight.
pub(super) fn results_node(model: &Launcher, rows: &[(Rect, usize)]) -> Node {
    // Each result's place in the list's order (sections, not result
    // indexes).
    let order: Vec<usize> = model.sections().into_iter().filter_map(|i| if let launcher::ListItem::Row(r) = i { Some(r) } else { None }).collect();
    let nodes = rows.iter().map(|&(r, result)| {
        let hit = model.results[result];
        let (label, _) = model.row_label(hit);
        let label = match hit {
            launcher::Hit::More(_) => trf!("Search for “{query}” in {label}", query = model.query.trim(), label = label),
            _ => label.to_owned(),
        };
        let tag = match hit {
            launcher::Hit::Window(_) => tr("Open window").to_owned(),
            launcher::Hit::Folder(_) => tr("Folder").to_owned(),
            launcher::Hit::App(i) if model.is_favorite(i) => tr("Favorite").to_owned(),
            _ => model.search_hit(hit).map(|h| if h.description.is_empty() { h.app.clone() } else { format!("{}, {}", h.description, h.app) }).unwrap_or_default(),
        };
        let on = result == model.selected;
        let node = Node::new(Role::ListBoxOption, label).id(format!("{hit:?}")).describe(tag).at(r.into()).click().selected(on).focused(on);
        match order.iter().position(|o| *o == result) {
            Some(place) => node.in_set(place),
            None => node,
        }
    });
    let list = Node::new(Role::ListBox, tr("Search Results")).id("results").set_size(model.results.len()).children(nodes);
    if rows.is_empty() {
        list.child(Node::label(tr("No matching apps")))
    } else {
        list
    }
}

pub(super) fn banner_said(d: &crate::notifications::Banner) -> Node {
    let body = if d.markup { crate::markup::plain(&d.body) } else { d.body.clone() };
    let mut said = d.summary.clone();
    if !d.app_name.is_empty() {
        said = trf!("{app}: {summary}", app = d.app_name, summary = d.summary);
    }
    if !body.is_empty() {
        said = trf!("{summary}. {body}", summary = said, body = body);
    }
    Node::label(said).id("said").live()
}

impl Ui {
    fn notification_list_nodes(&self, hits: &[(Rect, ListHit)], focus: Option<&ListHit>) -> Vec<Node> {
        let entries = &self.notify.list.entries;
        let summary = |id: u32| entries.iter().find(|e| e.id == id);
        let mut out = Vec::new();
        let mut list = Vec::new();
        // A notification's close button and links come before it.
        let mut pending = Vec::new();
        // The calendar's month title before its buttons and days.
        let mut month_said = false;
        for (r, hit) in hits {
            let r = crate::a11y::Rect::from(*r);
            let focused = focus == Some(hit);
            if matches!(hit, ListHit::Month(_) | ListHit::Day(..)) && !month_said {
                out.push(Node::label(self.calendar.month_title()).id("month"));
                month_said = true;
            }
            match hit {
                ListHit::Entry(id) => {
                    let Some(e) = summary(*id) else { continue };
                    let body = if e.markup { crate::markup::plain(&e.body) } else { e.body.clone() };
                    let name = if e.app_name.is_empty() { e.summary.clone() } else { format!("{}: {}", e.app_name, e.summary) };
                    // When it arrived first ("5 minutes ago. …").
                    let when = gio::glib::DateTime::now_local().ok().map(|now| crate::notifications::arrival(e.arrived, &now, true)).unwrap_or_default();
                    let body = if when.is_empty() { body } else if body.is_empty() { when } else { format!("{when}. {body}") };
                    list.push(Node::new(Role::ListItem, name).id(format!("note:{id}")).describe(body).at(r).click().focusable().focused(focused).children(pending.drain(..)));
                }
                ListHit::Dismiss(_) => pending.push(Node::button(tr("Close"), r).id("close").focusable().focused(focused)),
                ListHit::Link(uri) => pending.push(Node::new(Role::Link, uri).at(r).click().focusable().focused(focused)),
                ListHit::GroupClear(_) => pending.push(Node::button(tr("Clear"), r).id("clear").focusable().focused(focused)),
                ListHit::GroupToggle(key) => {
                    let count = entries.iter().filter(|e| notify::group_key(e) == *key).count();
                    let app = entries.iter().find(|e| notify::group_key(e) == *key).map_or_else(String::new, |e| e.app_name.clone());
                    list.push(Node::button(trf!("{app}, {count} notifications", app = app, count = count), r).id(format!("group:{key}")).focusable().focused(focused).children(pending.drain(..)));
                }
                ListHit::Month(delta) => out.push(Node::button(if *delta < 0 { tr("Previous Month") } else { tr("Next Month") }, r).id(format!("month:{delta}")).focusable().focused(focused)),
                ListHit::Day(y, m, d) => {
                    let selected = self.calendar.selected == (*y, *m, *d);
                    out.push(Node::button(day_name(*y, *m, *d), r).id(format!("day:{y}-{m}-{d}")).selected(selected).focusable().focused(focused));
                }
                ListHit::Media(command) => {
                    let player = self.calendar.data.as_ref().and_then(|d| d.player.as_ref());
                    if *command == crate::shell_keys::Playback::Previous {
                        // What plays, before its buttons.
                        if let Some(p) = player {
                            let title = if p.title.is_empty() { p.identity.clone() } else { p.title.clone() };
                            out.push(Node::label(title).id("media").describe(p.artist.clone()));
                        }
                    }
                    out.push(Node::button(playback_name(*command, player.is_some_and(|p| p.playing)), r).id(format!("{command:?}")).focusable().focused(focused));
                }
                ListHit::Clear => out.push(Node::button(tr("Clear"), r).id("clear-all").focusable().focused(focused)),
                ListHit::DoNotDisturb => out.push(Node::new(Role::Switch, tr("Do Not Disturb")).id("dnd").at(r).click().toggled(self.notify.do_not_disturb).focusable().focused(focused)),
                ListHit::Inside | ListHit::Outside => {}
            }
        }
        // The selected day's events.
        let events = self.calendar.selected_events();
        let (y, m, d) = self.calendar.selected;
        // Each event: its summary, described by when it is and how long.
        let events = if events.is_empty() { vec![Node::label(tr("No Events"))] } else { events.into_iter().map(|(summary, when)| Node::new(Role::ListItem, summary).describe(when)).collect() };
        out.push(Node::new(Role::List, trf!("Events, {}", day_name(y, m, d))).id("events").children(events));
        let list = Node::new(Role::List, tr("Notifications")).id("list").children(list);
        let list = if entries.is_empty() { list.child(Node::label(tr("No Notifications"))) } else { list };
        out.insert(0, list);
        out
    }

    // ----- Dock --------------------------------------------------------------

    /// The dock's items: each a button whose description says what is
    /// painted on it (windows, badge, progress, attention); the focused
    /// window's item is selected; the keyboard's item has the focus.
    fn dock_nodes(&self, items: &[(Rect, &crate::dock::Item, crate::dock::ItemState)], keyboard: Option<usize>) -> Vec<Node> {
        use crate::dock::Item;
        let names = self.atspi.app_names.as_ref().map(|(n, _)| n);
        let buttons = items.iter().enumerate().map(|(n, (r, item, state))| {
            let r = crate::a11y::Rect::from(*r);
            let node = match item {
                Item::App { id, .. } => {
                    let key = id.strip_suffix(".desktop").unwrap_or(id).to_lowercase();
                    let name = names.and_then(|n| n.get(&key)).cloned().unwrap_or_else(|| id.clone());
                    Node::button(name, r).id(id.clone())
                }
                Item::Mount(i) => Node::button(self.dock.mounts.get(*i).map_or_else(String::new, |m| m.name.clone()), r).id(format!("mount:{i}")),
                Item::Ext(i) => {
                    let name = self.ext.dock.get(*i).map_or_else(String::new, |d| if d.label.is_empty() { d.name.clone() } else { d.label.clone() });
                    Node::button(name, r).id(format!("ext:{i}"))
                }
                Item::Trash => Node::button(tr("Trash"), r).id("trash"),
                Item::ShowApps => Node::button(tr("Show Apps"), r).id("show-apps").toggled(self.launcher.is_some()),
            };
            let node = node.describe(crate::dock::describe_item(state)).focusable().focused(keyboard == Some(n));
            if matches!(item, Item::ShowApps) { node } else { node.selected(state.focused) }
        });
        vec![Node::new(Role::Toolbar, tr("Dock")).id("dock").children(buttons)]
    }
}

/// The question dialog (end session, display change, …) as painted: its
/// title, message (or list of busy apps), check box and buttons.
pub(super) fn dialog(title: &str, message: &str, list: Option<(&str, &[(String, String)])>, install: Option<bool>, hits: &[(Rect, OverlayHit)], (cancel, confirm): (&str, &str), focus: super::DialogStop) -> Node {
    use super::DialogStop;
    use crate::end_dialog::Choice;
    let at = |hit: OverlayHit| hits.iter().find(|(_, h)| *h == hit).map(|(r, _)| crate::a11y::Rect::from(*r)).unwrap_or_default();
    let mut out = Node::new(Role::Dialog, title).id("dialog").modal();
    match list {
        Some((header, rows)) => {
            out = out.child(Node::label(header));
            let rows = rows.iter().map(|(app, reason)| Node::new(Role::ListItem, app).describe(reason.as_str()));
            out = out.child(Node::new(Role::List, "").id("list").children(rows));
        }
        None if !message.is_empty() => out = out.child(Node::label(message).id("message")),
        None => {}
    }
    if let Some(on) = install {
        out = out.child(Node::new(Role::CheckBox, tr("Install pending software updates")).id("install").at(at(OverlayHit::EndInstall)).click().toggled(on).focused(focus == DialogStop::Install));
    }
    // Enter answers with the button that has the focus.
    let on = |choice| focus == DialogStop::Button(choice);
    out.child(Node::button(cancel, at(OverlayHit::Cancel)).id("cancel").focused(on(Choice::Canceled))).child(Node::button(confirm, at(OverlayHit::Confirm)).id("confirm").focused(on(Choice::Confirmed)))
}

/// "Share your screen?" as painted: one row per monitor (picked or not),
/// Cancel, and Share when several may be picked.
pub(super) fn share_dialog(message: &str, remember: bool, sources: &[(crate::screencast_chooser::Shared, String, String)], multiple: bool, picked: &[usize], hits: &[(Rect, OverlayHit)]) -> Node {
    let mut out = Node::new(Role::Dialog, tr("Share your screen?")).id("share").modal().child(Node::label(message).id("message"));
    if remember {
        out = out.child(Node::label(tr("It will be able to share the same screen again without asking.")));
    }
    let rows = hits.iter().filter_map(|(r, hit)| {
        let OverlayHit::Monitor(i) = hit else { return None };
        let (_, name, connector) = sources.get(*i)?;
        let row = Node::new(Role::ListBoxOption, name).id(format!("monitor:{i}")).describe(connector.as_str()).at((*r).into()).click();
        Some(if multiple { row.selected(picked.contains(i)) } else { row.focused(*i == 0) })
    });
    out = out.child(Node::new(Role::ListBox, "").id("monitors").children(rows));
    for (r, hit) in hits {
        match hit {
            OverlayHit::Cancel => out = out.child(Node::button(tr("Cancel"), (*r).into()).id("cancel")),
            OverlayHit::Confirm => out = out.child(Node::button(tr("Share"), (*r).into()).id("share")),
            _ => {}
        }
    }
    out
}

/// What a field holds after a client set its text to `value`, computed
/// from what was published; `range` is the characters the client said it
/// replaces (selected just before). A plain field takes `value`. A
/// password field was published masked, so the same change is made to
/// `current`: what stands where the range was is inserted between the
/// characters before and after it; without a range (or one that does not
/// fit) `value` replaces it all. Control characters are dropped.
pub(super) fn edited(current: &str, password: bool, value: &str, range: Option<(usize, usize)>) -> String {
    let clean = |s: &str| s.chars().filter(|c| !c.is_control()).collect::<String>();
    let len = current.chars().count();
    let new: Vec<char> = value.chars().collect();
    match range {
        Some((start, end)) if password && start <= end && end <= len && new.len() >= start + (len - end) => {
            let inserted: String = new[start..new.len() - (len - end)].iter().collect();
            let before: String = current.chars().take(start).collect();
            let after: String = current.chars().skip(end).collect();
            format!("{before}{}{after}", clean(&inserted))
        }
        _ => clean(value),
    }
}

/// The status area's icons in words.
fn status_words(s: &crate::status::Status) -> String {
    let mut words: Vec<String> = Vec::new();
    match (&s.wifi, &s.network) {
        (Some((true, Some(ssid))), _) => words.push(format!("Wi-Fi {ssid}")),
        (_, Some(icon)) if icon.contains("offline") || icon.contains("disconnected") || icon.contains("no-route") => words.push(tr("No network").into()),
        (_, Some(icon)) if icon.contains("wired") => words.push(tr("Wired network").into()),
        (_, Some(icon)) if icon.contains("vpn") => words.push("VPN".into()),
        (_, Some(_)) => words.push(tr("Network").into()),
        _ => {}
    }
    if let Some(icon) = &s.volume {
        words.push(if icon.contains("muted") { tr("Volume muted").into() } else { tr("Volume").into() });
    }
    if let Some((icon, percent)) = &s.battery {
        let charging = icon.contains("charging") && !icon.contains("discharging");
        let percent = helix_i18n::format_percent(percent.round());
        words.push(if charging { trf!("Battery {percent}, charging", percent = percent) } else { trf!("Battery {percent}", percent = percent) });
    }
    for (on, what) in [(s.location, tr("Location in use")), (s.microphone, tr("Microphone in use")), (s.camera, tr("Camera in use"))] {
        if on {
            words.push(what.into());
        }
    }
    words.join(", ")
}

pub(super) fn playback_name(command: crate::shell_keys::Playback, playing: bool) -> &'static str {
    use crate::shell_keys::Playback;
    match command {
        Playback::PlayPause if playing => tr("Pause"),
        Playback::PlayPause => tr("Play"),
        Playback::Pause => tr("Pause"),
        Playback::Stop => tr("Stop"),
        Playback::Next => tr("Next Track"),
        Playback::Previous => tr("Previous Track"),
        Playback::Rewind => tr("Rewind"),
        Playback::Forward => tr("Fast Forward"),
        Playback::Repeat => tr("Repeat"),
        Playback::Shuffle => tr("Shuffle"),
    }
}

/// A calendar day in words, e.g. "14 November 2026".
fn day_name(y: i32, m: i32, d: i32) -> String {
    let Ok(day) = gio::glib::DateTime::from_utc(y, m, d, 12, 0, 0.0) else { return String::new() };
    // Translators: a calendar day read out by screen readers (GLib
    // DateTime format): "14 November 2026".
    helix_i18n::format_date(&day, &helix_i18n::trc("calendar day for screen readers", "%e %B %Y"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hits() -> Vec<(Rect, OverlayHit)> {
        vec![(Rect { x: 10, y: 100, w: 80, h: 36 }, OverlayHit::Cancel), (Rect { x: 100, y: 100, w: 80, h: 36 }, OverlayHit::Confirm), (Rect { x: 10, y: 60, w: 200, h: 26 }, OverlayHit::EndInstall)]
    }

    #[test]
    fn end_session_dialog() {
        let node = dialog("Power Off", "The session will end automatically in 60 seconds.", None, Some(true), &hits(), ("Cancel", "Power Off"), super::super::DialogStop::Button(Choice::Confirmed));
        assert_eq!((node.role, node.name.as_str(), node.modal), (Role::Dialog, "Power Off", true));
        let message = node.find(&|n| n.role == Role::Label).expect("message");
        assert_eq!(message.text.as_deref(), Some("The session will end automatically in 60 seconds."));
        let install = node.find(&|n| n.role == Role::CheckBox).expect("check box");
        assert_eq!((install.toggled, install.bounds), (Some(true), Some(crate::a11y::Rect::new(10, 60, 200, 26))));
        let confirm = node.find(&|n| n.focused).expect("focus");
        assert_eq!((confirm.role, confirm.name.as_str()), (Role::Button, "Power Off"));
        let cancel = node.find(&|n| n.name == "Cancel").expect("cancel");
        assert_eq!(cancel.bounds, Some(crate::a11y::Rect::new(10, 100, 80, 36)));
        assert!(cancel.clickable);
        // Busy apps instead of the message.
        let busy = [("Editor".to_owned(), "Unsaved work".to_owned())];
        let node = dialog("Log Out", "ignored", Some(("Some applications are busy or have unsaved work:", &busy)), None, &hits(), ("Cancel", "Log Out Anyway"), super::super::DialogStop::Button(Choice::Confirmed));
        let row = node.find(&|n| n.role == Role::ListItem).expect("row");
        assert_eq!((row.name.as_str(), row.description.as_str()), ("Editor", "Unsaved work"));
        assert!(node.find(&|n| n.text.as_deref() == Some("ignored")).is_none());
        // A dialog whose default is Cancel reports Cancel focused.
        let node = dialog("“Editor” is not responding.", "", None, None, &hits(), ("Wait", "Force Quit"), super::super::DialogStop::Button(Choice::Canceled));
        assert_eq!(node.find(&|n| n.focused).map(|n| n.name.as_str()), Some("Wait"));
        assert_eq!(node.find(&|n| n.name == "Force Quit").map(|n| n.focused), Some(false));
        // The check box with the focus is the one marked focused.
        let node = dialog("Power Off", "", None, Some(false), &hits(), ("Cancel", "Power Off"), super::super::DialogStop::Install);
        assert_eq!(node.find(&|n| n.focused).map(|n| n.role), Some(Role::CheckBox));
        assert_eq!(node.find(&|n| n.name == "Power Off" && n.role == Role::Button).map(|n| n.focused), Some(false));
    }

    #[test]
    fn share_dialog_rows() {
        use crate::screencast_chooser::Shared;
        let sources: Vec<(Shared, String, String)> = vec![(Shared::Monitor("DP-1".into()), "Built-in Display".into(), "eDP-1".into()), (Shared::Monitor("DP-2".into()), "Dell".into(), "DP-2".into())];
        let hits = vec![(Rect { x: 0, y: 0, w: 100, h: 40 }, OverlayHit::Monitor(0)), (Rect { x: 0, y: 48, w: 100, h: 40 }, OverlayHit::Monitor(1)), (Rect { x: 0, y: 100, w: 100, h: 36 }, OverlayHit::Cancel), (Rect { x: 110, y: 100, w: 100, h: 36 }, OverlayHit::Confirm)];
        let node = share_dialog("App wants to see your screen.", false, &sources, true, &[1], &hits);
        let rows: Vec<&Node> = node.children.iter().find(|n| n.role == Role::ListBox).expect("list").children.iter().collect();
        assert_eq!(rows.iter().map(|r| (r.name.as_str(), r.selected)).collect::<Vec<_>>(), [("Built-in Display", Some(false)), ("Dell", Some(true))]);
        assert!(node.find(&|n| n.name == "Share" && n.role == Role::Button).is_some());
    }

    #[test]
    fn status_in_words() {
        let mut s = crate::status::Status { wifi: Some((true, Some("Home".into()))), network: Some("network-wireless-signal-good-symbolic".into()), volume: Some("audio-volume-muted-symbolic".into()), battery: Some(("battery-level-80-charging-symbolic".into(), 80.4)), microphone: true, ..Default::default() };
        assert_eq!(status_words(&s), "Wi-Fi Home, Volume muted, Battery 80%, charging, Microphone in use");
        s.wifi = None;
        s.network = Some("network-wired-symbolic".into());
        s.battery = Some(("battery-level-50-discharging-symbolic".into(), 50.0));
        s.microphone = false;
        assert_eq!(status_words(&s), "Wired network, Volume muted, Battery 50%");
        assert_eq!(status_words(&Default::default()), "");
    }

    #[test]
    fn editing_fields() {
        // A plain field takes the new text.
        assert_eq!(edited("ls", false, "ls -l", Some((2, 2))), "ls -l");
        // A password field: the change between the masked texts is made
        // to the secret (insert "x" at 1; delete 1..3; replace all).
        assert_eq!(edited("abc", true, "•x••", Some((1, 1))), "axbc");
        assert_eq!(edited("abcd", true, "••", Some((1, 3))), "ad");
        assert_eq!(edited("abc", true, "new", None), "new");
        assert_eq!(edited("abc", true, "z", Some((0, 1))), "z", "a range that does not fit: it all");
        assert_eq!(edited("", false, "a\nb", None), "ab", "no control characters");
    }

    #[test]
    fn words_for_buttons() {
        use crate::shell_keys::Playback;
        assert_eq!(playback_name(Playback::PlayPause, true), "Pause");
        assert_eq!(playback_name(Playback::PlayPause, false), "Play");
        assert_eq!(day_name(2026, 11, 14), "14 November 2026");
    }
}
