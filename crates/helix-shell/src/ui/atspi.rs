//! The shell UI on the accessibility bus (helix-desktop-spec
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

use super::lock_extras::LockApp;
use super::notify::ListHit;
use super::*;
use crate::a11y::publish::{Event, Publisher};
use crate::a11y::{Node, Role, Window};

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

/// What the app grid last showed: the page's cells, the open folder's
/// cells, the selected item, the page dots, the page.
pub(super) type GridShown<'a> = (&'a [(Rect, String)], &'a [(Rect, String)], Option<usize>, &'a [(Rect, usize)], usize);
/// A dock shown: its surface, monitor area, size, and items' places.
pub(super) type DockShown<'a> = (&'a WlSurface, (i32, i32, i32, i32), (u32, u32), Vec<(Rect, &'a crate::dock::Item)>);
/// The notification list shown: its surface, size, and places.
pub(super) type ListShown<'a> = (&'a WlSurface, (u32, u32), &'a [(Rect, ListHit)]);

#[derive(Default)]
pub(super) struct AtSpi {
    /// `None` until started, and with `HELIX_NO_ATSPI` set.
    publisher: Option<Publisher<WlSurface>>,
    /// What paint code recorded for its surface.
    recorded: Vec<(WlSurface, Vec<Node>)>,
    /// App names for the dock (reading them is slow), and when.
    app_names: Option<(HashMap<String, String>, Instant)>,
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
        if publisher.any_active() && !self.dock_a11y().is_empty() {
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
            match crate::a11y::tree::click_point(&target, request.action, request.data.as_ref()) {
                Some(point) => self.atspi_click(surface, point),
                None => tracing::debug!(?request, "accessibility action not supported"),
            }
        } else if activated {
            // Paint again, recording the nodes.
            self.redraw_all();
        }
    }

    /// A click at `point` on `surface`, as from the pointer.
    fn atspi_click(&mut self, surface: WlSurface, position: (f64, f64)) {
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
        out
    }

    /// The lock surface published: the primary monitor's.
    fn lock_primary(&self) -> Option<&LockView> {
        let primary = (self.primary_area.0, self.primary_area.1);
        self.lock.surfaces.iter().find(|v| v.origin == primary && v.width > 0).or_else(|| self.lock.surfaces.iter().find(|v| v.width > 0))
    }

    /// The window that has the keyboard.
    fn atspi_active(&self) -> &'static str {
        if self.lock.session.is_some() {
            LOCK
        } else if self.dialog.is_some() {
            DIALOG
        } else if self.switcher.is_some() {
            SWITCHER
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
        }
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
                ("Lock Screen".to_owned(), bounds, self.recorded(view.surface.wl_surface()))
            }
            PANEL => ("Top Bar".to_owned(), self.overlay_bounds(self.panel_width, PANEL_HEIGHT), self.panel_nodes()),
            DIALOG => {
                let o = self.dialog.as_ref()?;
                let nodes = self.recorded(o.layer.wl_surface());
                let title = nodes.first().map_or_else(|| "Dialog".to_owned(), |n| n.name.clone());
                (title, self.overlay_bounds(o.width, o.height), nodes)
            }
            QUICK => {
                let o = self.power_menu.as_ref()?;
                ("System Menu".to_owned(), self.overlay_bounds(o.width, o.height), self.recorded(o.layer.wl_surface()))
            }
            SWITCHER => {
                let o = self.switcher.as_ref()?;
                let (title, list) = self.switcher_nodes(o);
                (title, self.overlay_bounds(o.width, o.height), list)
            }
            LAUNCHER => {
                let view = self.launcher.as_ref()?;
                ("Overview".to_owned(), self.overlay_bounds(view.width, view.height), self.launcher_nodes(view))
            }
            BANNER => {
                let view = self.banner.as_ref()?;
                let (x, y, w, _) = self.primary_area;
                let bounds = crate::a11y::Rect::new(x + (w - view.width as i32) / 2, y + 8, view.width as i32, view.height as i32);
                ("Notification".to_owned(), bounds, vec![self.banner_node(view)])
            }
            NOTIFICATIONS => {
                let (_, (w, h), hits) = self.notification_list_a11y()?;
                ("Notifications".to_owned(), self.overlay_bounds(w, h), self.notification_list_nodes(hits))
            }
            dock => {
                let index: usize = dock.strip_prefix(DOCK)?.parse().ok()?;
                let docks = self.dock_a11y();
                let (_, area, (w, h), items) = docks.get(index)?;
                let (w, h) = (*w as i32, *h as i32);
                let (ax, ay, aw, ah) = *area;
                let (x, y) = match self.dock.settings.edge {
                    crate::dock::Edge::Bottom => (ax + (aw - w) / 2, ay + ah - h),
                    crate::dock::Edge::Top => (ax + (aw - w) / 2, ay),
                    crate::dock::Edge::Left => (ax, ay + (ah - h) / 2),
                    crate::dock::Edge::Right => (ax + aw - w, ay + (ah - h) / 2),
                };
                ("Dock".to_owned(), crate::a11y::Rect::new(x, y, w, h), self.dock_nodes(items))
            }
        };
        Some(Window { title, bounds, active, children })
    }

    // ----- Lock screen -------------------------------------------------------

    /// The lock screen as painted on `view`: the clock, the user, the
    /// password field and its message, the media player and notification
    /// counts, and the buttons; `asked` is the power question shown, with
    /// the apps that are busy.
    pub(super) fn lock_a11y(&self, view: &LockView, clock: &str, field: Rect, asked: Option<(&str, Vec<String>)>, counts: &[LockApp]) -> Vec<Node> {
        let mut out = vec![Node::label(clock).id("clock"), Node::label(&self.lock.user).id("user")];
        let password = Node::text_field(Role::PasswordInput, "Password", Node::mask(&self.lock.password), field.into()).id("password");
        out.push(password.focused(self.lock.confirming.is_none()).disabled(self.lock.checking));
        if let Some(message) = &self.lock.message {
            out.push(Node::label(message).id("message").live());
        }
        if let Some(player) = &self.lock_extras.media {
            let title = if player.title.is_empty() { &player.identity } else { &player.title };
            out.push(Node::label(title).id("media-title"));
            if !player.artist.is_empty() {
                out.push(Node::label(&player.artist).id("media-artist"));
            }
        }
        for &(r, command) in &view.hits {
            out.push(Node::button(playback_name(command, self.lock_extras.media.as_ref().is_some_and(|p| p.playing)), r.into()).id(format!("{command:?}")));
        }
        for app in counts {
            out.push(Node::label(format!("{}: {} notification{}", app.name, app.count, if app.count == 1 { "" } else { "s" })));
        }
        if let Some((question, busy)) = asked {
            if !busy.is_empty() {
                out.push(Node::label("Some apps are busy:"));
                out.extend(busy.into_iter().map(Node::label));
            }
            out.push(Node::label(question).id("question").live());
        }
        for &(r, hit) in &view.power_hits {
            let name = match hit {
                LockPowerHit::Ask(PowerAction::Restart) => "Restart…",
                LockPowerHit::Ask(_) => "Power Off…",
                LockPowerHit::Cancel => "Cancel",
                LockPowerHit::Confirm if self.lock.confirming == Some(PowerAction::Restart) => "Restart",
                LockPowerHit::Confirm => "Power Off",
            };
            out.push(Node::button(name, r.into()).id(format!("{hit:?}")).focused(hit == LockPowerHit::Confirm));
        }
        if let Some(r) = view.switch_user {
            out.push(Node::button("Switch User…", r.into()).id("switch-user"));
        }
        out
    }

    // ----- Panel -------------------------------------------------------------

    /// The panel's items where the last paint put them.
    fn panel_nodes(&self) -> Vec<Node> {
        use crate::panel_layout::{Item, Slot};
        let tray = self.tray_shown();
        let controls = panel_controls(&self.panel_placed, tray.len());
        let focus = self.panel_focus.and_then(|i| controls.get(i).copied());
        let focused = |slot: Slot, x: i32| focus.is_some_and(|(s, fx, _)| s == slot && fx == x);
        let h = PANEL_HEIGHT as i32;
        let mut out = Vec::new();
        for &(slot, x, w) in &self.panel_placed {
            if w <= 0 {
                continue;
            }
            let r = crate::a11y::Rect::new(x, 0, w, h);
            let node = match slot {
                Slot::Item(Item::Apps) => Node::button(APPS_LABEL, r).toggled(self.launcher.is_some()),
                Slot::Item(Item::Workspaces) => Node::label(format!("Workspace {} of {}", self.windows.active + 1, self.windows.count)).at(r),
                Slot::Item(Item::Clock) => {
                    let clock = self.shown_minute.clone().unwrap_or_else(clock_text);
                    let what = if self.unseen_dot() { "Notifications and calendar, unread notifications" } else { "Notifications and calendar" };
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
                Slot::Item(Item::Accessibility) => Node::button("Accessibility", r),
                Slot::Item(Item::Keyboard) => Node::button(self.source_label.clone().unwrap_or_default(), r).describe("Input source"),
                Slot::Item(Item::Status) => Node::button("System", r).describe(status_words(&self.status)).expanded(self.power_menu.is_some()),
                Slot::Item(Item::Power) => Node::button(POWER_LABEL, r).expanded(self.power_menu.is_some()),
                Slot::Ext(i) => {
                    let Some(item) = self.ext.panel.get(i) else { continue };
                    let name = [&item.accessible_name, &item.label, &item.tooltip, &item.name].into_iter().find(|s| !s.is_empty()).cloned().unwrap_or_default();
                    Node::button(name, r).id(item.full_id.clone())
                }
                Slot::Privacy => {
                    let mut ix = x;
                    if self.sharing {
                        out.push(Node::label("Sharing").id("sharing").at(crate::a11y::Rect::new(ix, 0, SHARING_W, h)));
                        ix += SHARING_W;
                    }
                    if self.recording {
                        out.push(Node::button("Recording", crate::a11y::Rect::new(ix, 0, RECORDING_W, h)).id("recording").describe("Stop recording"));
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

    // ----- Window switcher ---------------------------------------------------

    fn switcher_nodes(&self, o: &Overlay<SwitcherState>) -> (String, Vec<Node>) {
        let switcher = &o.state.switcher;
        let title = match &o.state.kind {
            SwitchKind::InputSources(_) => "Input Sources",
            SwitchKind::Windows { apps: None } if self.switcher_mode == Some(SwitchMode::Apps) => "Applications",
            SwitchKind::Windows { .. } => "Windows",
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
        let field = Rect { h: 48, ..view.content };
        let results = !model.query.is_empty();
        let (cells, folder_cells, selected, dots, page) = view.grid.a11y();
        let grid_focus = !results && selected.is_some();
        let search = Node::text_field(Role::SearchInput, "Search", &model.query, field.into()).id("search");
        let mut out = vec![search.focused(!results && !grid_focus)];
        let name = |id: &str| model.apps.iter().find(|a| a.id == id).map(|a| a.name.clone()).or_else(|| model.folders.iter().find(|f| f.id == id).map(|f| f.name.clone())).unwrap_or_default();
        if results {
            let rows = view.rows.iter().map(|&(r, result)| {
                let hit = model.results[result];
                let (label, _) = model.row_label(hit);
                let label = match hit {
                    launcher::Hit::More(_) => format!("Search for “{}” in {label}", model.query.trim()),
                    _ => label.to_owned(),
                };
                let tag = match hit {
                    launcher::Hit::Window(_) => "Open window".to_owned(),
                    launcher::Hit::Folder(_) => "Folder".to_owned(),
                    launcher::Hit::App(i) if model.is_favorite(i) => "Favorite".to_owned(),
                    _ => model.search_hit(hit).map(|h| if h.description.is_empty() { h.app.clone() } else { format!("{}, {}", h.description, h.app) }).unwrap_or_default(),
                };
                let on = result == model.selected;
                Node::new(Role::ListBoxOption, label).id(format!("{hit:?}")).describe(tag).at(r.into()).click().selected(on).focused(on)
            });
            let list = Node::new(Role::ListBox, "Search Results").id("results").children(rows);
            out.push(if view.rows.is_empty() { list.child(Node::label("No matching apps")) } else { list });
        } else if let Some(folder) = model.open_folder.and_then(|i| model.folders.get(i)) {
            let current = selected.and_then(|k| folder.apps.get(k));
            let apps = folder_cells.iter().map(|(r, id)| Node::button(name(id), (*r).into()).id(id.clone()).selected(current == Some(id)).focused(current == Some(id)));
            out.push(Node::new(Role::Group, &folder.name).id(format!("folder:{}", folder.id)).children(apps));
        } else {
            let apps = cells.iter().enumerate().map(|(i, (r, id))| Node::button(name(id), (*r).into()).id(id.clone()).selected(selected == Some(i)).focused(selected == Some(i)));
            out.push(Node::new(Role::Group, "Applications").id("grid").children(apps));
            if dots.len() > 1 {
                let pages = dots.iter().map(|(r, p)| Node::button(format!("Page {}", p + 1), (*r).into()).selected(*p == page));
                out.push(Node::new(Role::Group, "Pages").id("pages").children(pages));
            }
        }
        out
    }

    // ----- Notifications -----------------------------------------------------

    fn banner_node(&self, view: &BannerView) -> Node {
        let d = &view.data;
        let body = if d.markup { crate::markup::plain(&d.body) } else { d.body.clone() };
        let body_r = view.hits.iter().find(|(_, h)| *h == BannerHit::Body).map(|(r, _)| (*r).into()).unwrap_or_default();
        let mut note = Node::new(Role::Alert, &d.summary).id(format!("note:{}", d.id)).describe(body).at(body_r).click().live().focused(view.focused);
        if !d.app_name.is_empty() {
            note = note.child(Node::label(&d.app_name).id("app"));
        }
        for (r, hit) in &view.hits {
            let r = crate::a11y::Rect::from(*r);
            if *hit == BannerHit::Body {
                continue;
            }
            note = note.child(match hit {
                BannerHit::Body | BannerHit::Close => Node::button("Close", r).id("close"),
                BannerHit::Button(key) => Node::button(d.buttons.iter().find(|(k, _)| k == key).map_or(key.as_str(), |(_, l)| l.as_str()), r).id(format!("button:{key}")),
                BannerHit::Link(uri) => Node::new(Role::Link, uri).at(r).click(),
            });
        }
        note
    }

    fn notification_list_nodes(&self, hits: &[(Rect, ListHit)]) -> Vec<Node> {
        let entries = &self.notify.list.entries;
        let summary = |id: u32| entries.iter().find(|e| e.id == id);
        let mut out = Vec::new();
        let mut list = Vec::new();
        // A notification's close button and links come before it.
        let mut pending = Vec::new();
        for (r, hit) in hits {
            let r = crate::a11y::Rect::from(*r);
            match hit {
                ListHit::Entry(id) => {
                    let Some(e) = summary(*id) else { continue };
                    let body = if e.markup { crate::markup::plain(&e.body) } else { e.body.clone() };
                    let name = if e.app_name.is_empty() { e.summary.clone() } else { format!("{}: {}", e.app_name, e.summary) };
                    list.push(Node::new(Role::ListItem, name).id(format!("note:{id}")).describe(body).at(r).click().children(pending.drain(..)));
                }
                ListHit::Dismiss(_) => pending.push(Node::button("Close", r).id("close")),
                ListHit::Link(uri) => pending.push(Node::new(Role::Link, uri).at(r).click()),
                ListHit::GroupClear(_) => pending.push(Node::button("Clear", r).id("clear")),
                ListHit::GroupToggle(key) => {
                    let count = entries.iter().filter(|e| notify::group_key(e) == *key).count();
                    let app = entries.iter().find(|e| notify::group_key(e) == *key).map_or_else(String::new, |e| e.app_name.clone());
                    list.push(Node::button(format!("{app}, {count} notifications"), r).id(format!("group:{key}")).children(pending.drain(..)));
                }
                ListHit::Month(delta) => out.push(Node::button(if *delta < 0 { "Previous Month" } else { "Next Month" }, r).id(format!("month:{delta}"))),
                ListHit::Day(y, m, d) => out.push(Node::button(day_name(*y, *m, *d), r).id(format!("day:{y}-{m}-{d}"))),
                ListHit::Media(command) => out.push(Node::button(playback_name(*command, self.lock_extras.media.as_ref().is_some_and(|p| p.playing)), r).id(format!("{command:?}"))),
                ListHit::Clear => out.push(Node::button("Clear", r).id("clear-all")),
                ListHit::DoNotDisturb => out.push(Node::new(Role::Switch, "Do Not Disturb").id("dnd").at(r).click().toggled(self.notify.do_not_disturb)),
                ListHit::Inside | ListHit::Outside => {}
            }
        }
        let list = Node::new(Role::List, "Notifications").id("list").children(list);
        let list = if entries.is_empty() { list.child(Node::label("No Notifications")) } else { list };
        out.insert(0, list);
        out
    }

    // ----- Dock --------------------------------------------------------------

    fn dock_nodes(&self, items: &[(Rect, &crate::dock::Item)]) -> Vec<Node> {
        use crate::dock::Item;
        let names = self.atspi.app_names.as_ref().map(|(n, _)| n);
        let running = |id: &str| {
            let id = id.strip_suffix(".desktop").unwrap_or(id).to_lowercase();
            self.windows.mru().iter().any(|e| e.app_id.to_lowercase() == id)
        };
        let buttons = items.iter().map(|(r, item)| {
            let r = crate::a11y::Rect::from(*r);
            match item {
                Item::App { id, .. } => {
                    let key = id.strip_suffix(".desktop").unwrap_or(id).to_lowercase();
                    let name = names.and_then(|n| n.get(&key)).cloned().unwrap_or_else(|| id.clone());
                    Node::button(name, r).id(id.clone()).describe(if running(id) { "Running" } else { "" })
                }
                Item::Mount(i) => Node::button(self.dock.mounts.get(*i).map_or_else(String::new, |m| m.name.clone()), r).id(format!("mount:{i}")),
                Item::Ext(i) => {
                    let name = self.ext.dock.get(*i).map_or_else(String::new, |d| if d.label.is_empty() { d.name.clone() } else { d.label.clone() });
                    Node::button(name, r).id(format!("ext:{i}"))
                }
                Item::Trash => Node::button("Trash", r).id("trash"),
                Item::ShowApps => Node::button("Show Apps", r).id("show-apps").toggled(self.launcher.is_some()),
            }
        });
        vec![Node::new(Role::Toolbar, "Dock").id("dock").children(buttons)]
    }
}

/// The question dialog (end session, display change, …) as painted: its
/// title, message (or list of busy apps), check box and buttons.
pub(super) fn dialog(title: &str, message: &str, list: Option<(&str, &[(String, String)])>, install: Option<bool>, hits: &[(Rect, OverlayHit)], (cancel, confirm): (&str, &str)) -> Node {
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
        out = out.child(Node::new(Role::CheckBox, "Install pending software updates").id("install").at(at(OverlayHit::EndInstall)).click().toggled(on));
    }
    // Enter answers with the action: it has the focus.
    out.child(Node::button(cancel, at(OverlayHit::Cancel)).id("cancel")).child(Node::button(confirm, at(OverlayHit::Confirm)).id("confirm").focused(true))
}

/// "Share your screen?" as painted: one row per monitor (picked or not),
/// Cancel, and Share when several may be picked.
pub(super) fn share_dialog(message: &str, remember: bool, sources: &[(crate::screencast_chooser::Shared, String, String)], multiple: bool, picked: &[usize], hits: &[(Rect, OverlayHit)]) -> Node {
    let mut out = Node::new(Role::Dialog, "Share your screen?").id("share").modal().child(Node::label(message).id("message"));
    if remember {
        out = out.child(Node::label("It will be able to share the same screen again without asking."));
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
            OverlayHit::Cancel => out = out.child(Node::button("Cancel", (*r).into()).id("cancel")),
            OverlayHit::Confirm => out = out.child(Node::button("Share", (*r).into()).id("share")),
            _ => {}
        }
    }
    out
}

/// The status area's icons in words.
fn status_words(s: &crate::status::Status) -> String {
    let mut words: Vec<String> = Vec::new();
    match (&s.wifi, &s.network) {
        (Some((true, Some(ssid))), _) => words.push(format!("Wi-Fi {ssid}")),
        (_, Some(icon)) if icon.contains("offline") || icon.contains("disconnected") || icon.contains("no-route") => words.push("No network".into()),
        (_, Some(icon)) if icon.contains("wired") => words.push("Wired network".into()),
        (_, Some(icon)) if icon.contains("vpn") => words.push("VPN".into()),
        (_, Some(_)) => words.push("Network".into()),
        _ => {}
    }
    if let Some(icon) = &s.volume {
        words.push(if icon.contains("muted") { "Volume muted".into() } else { "Volume".into() });
    }
    if let Some((icon, percent)) = &s.battery {
        let charging = icon.contains("charging") && !icon.contains("discharging");
        words.push(format!("Battery {}%{}", percent.round() as i64, if charging { ", charging" } else { "" }));
    }
    for (on, what) in [(s.location, "Location in use"), (s.microphone, "Microphone in use"), (s.camera, "Camera in use")] {
        if on {
            words.push(what.into());
        }
    }
    words.join(", ")
}

fn playback_name(command: crate::shell_keys::Playback, playing: bool) -> &'static str {
    use crate::shell_keys::Playback;
    match command {
        Playback::PlayPause if playing => "Pause",
        Playback::PlayPause => "Play",
        Playback::Pause => "Pause",
        Playback::Stop => "Stop",
        Playback::Next => "Next Track",
        Playback::Previous => "Previous Track",
        Playback::Rewind => "Rewind",
        Playback::Forward => "Fast Forward",
        Playback::Repeat => "Repeat",
        Playback::Shuffle => "Shuffle",
    }
}

/// A calendar day in words, e.g. "14 November 2026".
fn day_name(y: i32, m: i32, d: i32) -> String {
    const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
    let month = usize::try_from(m - 1).ok().and_then(|i| MONTHS.get(i)).copied().unwrap_or("");
    format!("{d} {month} {y}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hits() -> Vec<(Rect, OverlayHit)> {
        vec![(Rect { x: 10, y: 100, w: 80, h: 36 }, OverlayHit::Cancel), (Rect { x: 100, y: 100, w: 80, h: 36 }, OverlayHit::Confirm), (Rect { x: 10, y: 60, w: 200, h: 26 }, OverlayHit::EndInstall)]
    }

    #[test]
    fn end_session_dialog() {
        let node = dialog("Power Off", "The session will end automatically in 60 seconds.", None, Some(true), &hits(), ("Cancel", "Power Off"));
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
        let node = dialog("Log Out", "ignored", Some(("Some applications are busy or have unsaved work:", &busy)), None, &hits(), ("Cancel", "Log Out Anyway"));
        let row = node.find(&|n| n.role == Role::ListItem).expect("row");
        assert_eq!((row.name.as_str(), row.description.as_str()), ("Editor", "Unsaved work"));
        assert!(node.find(&|n| n.text.as_deref() == Some("ignored")).is_none());
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
    fn words_for_buttons() {
        use crate::shell_keys::Playback;
        assert_eq!(playback_name(Playback::PlayPause, true), "Pause");
        assert_eq!(playback_name(Playback::PlayPause, false), "Play");
        assert_eq!(day_name(2026, 11, 14), "14 November 2026");
    }
}
