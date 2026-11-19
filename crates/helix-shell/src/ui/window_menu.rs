//! The window menu (helixdesktop/spec `contracts/wayland/README.md`,
//! "Window menu and window keys"): drawn by the shell where the compositor
//! says, its choices carried out by the compositor.

use helix_protocols::client::helix_window_list_v1::MenuState;

use super::*;
use crate::windows::{WindowAction, ALL_WORKSPACES};
use helix_i18n::tr;

/// A menu's width and an entry's height: they grow with the text scale.
pub(super) fn menu_w() -> i32 {
    helix_draw::theme::grow(240)
}

pub(super) fn item_h() -> i32 {
    helix_draw::theme::grow(32)
}
const SEPARATOR_H: i32 = 9;
/// Window previews: a tile's width, its picture's height, the title below.
pub(super) const TILE_W: i32 = 220;
pub(super) const PICTURE_H: i32 = 130;
pub(super) const TILE_TITLE_H: i32 = 28;
const TILE_GAP: i32 = 8;
/// At most this many tiles in a row.
const TILES_PER_ROW: usize = 4;

/// What a menu entry does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Choice {
    Action(WindowAction),
    OnAllWorkspaces(bool),
    Workspace(u32),
    /// Switch to an input source (the panel's source menu), by index.
    InputSource(usize),
    /// A dock menu entry, by index into its targets.
    Dock(usize),
    /// An accessibility switch (`a11y_menu::SWITCHES` index).
    A11y(usize),
    /// An IBus engine property (index into the menu's properties).
    ImProperty(usize),
    /// A status icon menu entry (index into the open menu's ids).
    Tray(usize),
    /// An entry that does nothing (shown dimmed).
    Disabled,
    /// The panel's "Customize Panel…" and "Extensions…".
    CustomizePanel,
    Extensions,
    /// An extension menu's entry (index into the open menu's ids).
    ExtMenu(usize),
    /// The background menu's entries: open Settings at a panel.
    Settings(super::background_menu::SettingsPanel),
    /// A desktop icon menu's entry.
    DesktopIcon(super::desktop_icons::IconAction),
    /// Show the item's submenu.
    Submenu,
    /// Back to the menu a submenu came from.
    Back,
    /// The agent indicator's *Stop* (index into the menu's agents).
    StopAgent(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Item {
    pub label: String,
    pub choice: Choice,
    /// A check box, and whether it is checked.
    pub checked: Option<bool>,
    /// The current one of a set (drawn with a dot).
    pub active: bool,
    /// A submenu's entries (with `Choice::Submenu`).
    pub sub: Vec<Option<Item>>,
}

/// The entries for a window in `state` on `workspace` of `count`; `None`
/// is a separator.
pub(super) fn items(state: MenuState, workspace: Option<u32>, count: u32) -> Vec<Option<Item>> {
    let item = |label: &str, choice| Some(Item { label: label.to_owned(), choice, checked: None, active: false, sub: Vec::new() });
    let maximized = state.contains(MenuState::Maximized);
    let fixed = maximized || state.contains(MenuState::Fullscreen);
    let mut out = vec![item(tr("Minimize"), Choice::Action(WindowAction::Minimize))];
    out.push(if maximized {
        item(tr("Restore"), Choice::Action(WindowAction::Unmaximize))
    } else {
        item(tr("Maximize"), Choice::Action(WindowAction::Maximize))
    });
    if !fixed {
        out.push(item(tr("Move"), Choice::Action(WindowAction::BeginMove)));
        out.push(item(tr("Resize"), Choice::Action(WindowAction::BeginResize)));
    }
    out.push(None);
    let above = state.contains(MenuState::Above);
    out.push(Some(Item { label: tr("Always on Top").into(), choice: Choice::Action(WindowAction::ToggleAbove), checked: Some(above), active: false, sub: Vec::new() }));
    let all = state.contains(MenuState::OnAllWorkspaces);
    out.push(Some(Item { label: tr("Always on Visible Workspace").into(), choice: Choice::OnAllWorkspaces(!all), checked: Some(all), active: false, sub: Vec::new() }));
    if let Some(ws) = workspace.filter(|w| !all && *w != ALL_WORKSPACES) {
        if ws > 0 {
            out.push(item(tr("Move to Workspace Left"), Choice::Workspace(ws - 1)));
        }
        if ws + 1 < count {
            out.push(item(tr("Move to Workspace Right"), Choice::Workspace(ws + 1)));
        }
    }
    let monitors = [
        (MenuState::MonitorLeft, tr("Move to Monitor Left"), WindowAction::MoveToMonitorLeft),
        (MenuState::MonitorRight, tr("Move to Monitor Right"), WindowAction::MoveToMonitorRight),
        (MenuState::MonitorUp, tr("Move to Monitor Up"), WindowAction::MoveToMonitorUp),
        (MenuState::MonitorDown, tr("Move to Monitor Down"), WindowAction::MoveToMonitorDown),
    ];
    if monitors.iter().any(|(bit, _, _)| state.contains(*bit)) {
        out.push(None);
        for (bit, label, action) in monitors {
            if state.contains(bit) {
                out.push(item(label, Choice::Action(action)));
            }
        }
    }
    out.push(None);
    out.push(item(tr("Close"), Choice::Action(WindowAction::Close)));
    out
}

/// The menu entries for engine properties (`None`: a separator): visible
/// ones; a menu property is a submenu of its entries (one without any is
/// left out); insensitive ones are left out; separators only between
/// entries. `leaf` makes the entry of a property that is not a menu.
pub(super) fn property_items(props: &[crate::ibus::Prop], leaf: &mut dyn FnMut(&crate::ibus::Prop) -> Item) -> Vec<Option<Item>> {
    use crate::ibus::PropKind;
    let mut out: Vec<Option<Item>> = Vec::new();
    for p in props.iter().filter(|p| p.visible) {
        match p.kind {
            PropKind::Separator => out.push(None),
            PropKind::Menu if p.sensitive => {
                let sub = property_items(&p.sub, leaf);
                if !sub.is_empty() {
                    out.push(Some(Item { label: p.label.clone(), choice: Choice::Submenu, checked: None, active: false, sub }));
                }
            }
            _ if p.sensitive => out.push(Some(leaf(p))),
            _ => {}
        }
    }
    // No separator first, last or twice.
    let mut clean: Vec<Option<Item>> = Vec::new();
    for e in out {
        if e.is_none() && clean.last().is_none_or(|l| l.is_none()) {
            continue;
        }
        clean.push(e);
    }
    if clean.last().is_some_and(|l| l.is_none()) {
        clean.pop();
    }
    clean
}

/// The open window menu.
pub(super) struct WindowMenu {
    layer: LayerSurface,
    width: u32,
    height: u32,
    configured: bool,
    /// The window it is for.
    id: u32,
    /// Where it opens, relative to its output.
    at: (i32, i32),
    /// The output's place in the layout.
    origin: (i32, i32),
    items: Vec<Option<Item>>,
    /// Keyboard selection (index into `items`).
    selected: Option<usize>,
    hits: Vec<(Rect, usize)>,
    /// Where the box is, for clicks outside it.
    bounds: Rect,
    /// The input sources an input source menu is for.
    sources: Vec<crate::input_sources::Source>,
    /// The panel item that opened it (its pill is lit while it is open).
    pub(super) panel_item: Option<crate::panel_layout::Item>,
    /// Window previews instead of rows: the windows; the items are, for
    /// each, "activate" then "close".
    pub(super) previews: Vec<u32>,
    /// The keyboard highlight timed out (`keyboard-focus-visible-timeout`).
    focus_hidden: bool,
    /// The selection was made with the keyboard (it gets a focus ring).
    by_keyboard: bool,
    /// The menus a submenu was opened from, and their selection.
    parents: Vec<(Vec<Option<Item>>, Option<usize>)>,
}

/// The size of a previews box for `count` windows.
pub(super) fn previews_size(count: usize) -> (i32, i32) {
    let per_row = count.clamp(1, TILES_PER_ROW);
    let rows = count.div_ceil(TILES_PER_ROW).max(1);
    (per_row as i32 * (TILE_W + TILE_GAP) + TILE_GAP, rows as i32 * (PICTURE_H + TILE_TITLE_H + TILE_GAP) + TILE_GAP)
}

impl Ui {
    /// The compositor asks for the menu of window `id` at (x, y).
    pub(super) fn show_window_menu(&mut self, id: u32, x: i32, y: i32, state: MenuState) {
        // Alt+Space: the first entry starts selected.
        self.menu_by_key = state.contains(MenuState::Keyboard);
        let workspace = self.windows.workspace_of(id);
        let items = items(state, workspace, self.windows.count);
        // The menu is laid out left to right and drawn mirrored in a
        // right-to-left interface: mirror the screen point it opens at.
        let x = if helix_draw::rtl() { self.mirrored_screen_x(x, y) } else { x };
        self.open_menu(id, x, y, items, Vec::new());
    }

    /// `x` mirrored on the monitor that (x, y) is on.
    pub(super) fn mirrored_screen_x(&self, x: i32, y: i32) -> i32 {
        self.output_state
            .outputs()
            .filter_map(|o| self.output_state.info(&o))
            .find_map(|info| {
                let (Some((ox, oy)), Some((w, h))) = (info.logical_position, info.logical_size) else { return None };
                (x >= ox && y >= oy && x < ox + w && y < oy + h).then_some(ox + w - (x - ox))
            })
            .unwrap_or(x)
    }

    /// The panel's input source menu, below its indicator at `x`
    /// (helixdesktop/spec `contracts/gsettings/input-settings.md`,
    /// "Input source indicator").
    pub(super) fn show_sources_menu(&mut self, x: i32) {
        let sources = crate::input_sources::current();
        let mut items: Vec<Option<Item>> = sources
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let label = if s.0 == "xkb" { crate::input_sources::description(&s.1).unwrap_or_else(|| s.1.clone()) } else { s.1.clone() };
                Some(Item { label, choice: Choice::InputSource(i), checked: None, active: i == 0, sub: Vec::new() })
            })
            .collect();
        // The IBus engine's properties, below its source.
        self.ime_menu.clear();
        if sources.first().is_some_and(|s| s.0 == "ibus") {
            let ime_menu = &mut self.ime_menu;
            let mut leaf = |p: &crate::ibus::Prop| {
                let index = ime_menu.len();
                ime_menu.push((p.key.clone(), p.kind, p.state));
                let checked = (p.kind == crate::ibus::PropKind::Toggle).then_some(p.state == 1);
                let active = p.kind == crate::ibus::PropKind::Radio && p.state == 1;
                Item { label: p.label.clone(), choice: Choice::ImProperty(index), checked, active, sub: Vec::new() }
            };
            let props = property_items(&self.ime_props, &mut leaf);
            if !props.is_empty() {
                items.push(None);
            }
            items.extend(props);
        }
        self.open_menu(0, x, panel_height() as i32, items, sources);
        self.lit_by_panel(crate::panel_layout::Item::Keyboard);
    }

    /// The panel's accessibility menu, below its indicator at `x`.
    pub(super) fn show_a11y_menu(&mut self, x: i32) {
        let items = crate::a11y_menu::SWITCHES
            .iter()
            .zip(&self.a11y.switches)
            .enumerate()
            .filter_map(|(i, (s, state))| Some(Some(Item { label: tr(s.label).to_owned(), choice: Choice::A11y(i), checked: Some((*state)?), active: false, sub: Vec::new() })))
            .collect();
        self.open_menu(0, x, panel_height() as i32, items, Vec::new());
        self.lit_by_panel(crate::panel_layout::Item::Accessibility);
    }

    /// The agent indicator's menu, below it at `x` (helixdesktop/spec
    /// `contracts/agent-access/README.md`, "Indicator"): *Stop* for each
    /// active agent, and the Agent Access settings.
    pub(super) fn show_agents_menu(&mut self, x: i32) {
        let item = |label: String, choice| Some(Item { label, choice, checked: None, active: false, sub: Vec::new() });
        let mut items = vec![item(tr("Agents are using your desktop").to_owned(), Choice::Disabled)];
        let mut ids = Vec::new();
        for (i, agent) in self.agents_active.iter().enumerate() {
            let level = super::agent_prompt::level_name(agent.level);
            items.push(item(helix_i18n::trf!("Stop {agent} ({level})", agent = agent.label(), level = level), Choice::StopAgent(i)));
            ids.push(agent.agent.clone());
        }
        items.push(None);
        items.push(item(tr("Agent Access Settings").to_owned(), Choice::Settings(super::background_menu::SettingsPanel::AgentAccess)));
        self.open_menu(0, x, panel_height() as i32, items, Vec::new());
        self.agent_menu = ids;
    }

    /// The menu just opened belongs to the panel's `item`: its pill
    /// shows the open state until the menu closes.
    fn lit_by_panel(&mut self, item: crate::panel_layout::Item) {
        if let Some(menu) = self.window_menu.as_mut() {
            menu.panel_item = Some(item);
        }
        self.draw_panel();
    }

    /// Close the menu (lighting down the panel pill that opened it).
    pub(super) fn close_window_menu(&mut self) {
        if self.window_menu.take().is_some_and(|m| m.panel_item.is_some()) {
            self.draw_panel();
        }
    }

    /// Open a menu at (x, y); when it was asked for with a key
    /// (`menu_by_key`, set just before) its first enabled entry is
    /// selected, else none until the pointer or a key moves.
    pub(super) fn open_menu(&mut self, id: u32, x: i32, y: i32, items: Vec<Option<Item>>, sources: Vec<crate::input_sources::Source>) {
        let was_lit = self.window_menu.as_ref().is_some_and(|m| m.panel_item.is_some());
        let selected = if std::mem::take(&mut self.menu_by_key) { first_enabled(&items) } else { None };
        // A menu of the app grid's opens over the launcher.
        if !std::mem::take(&mut self.dock.menu_over_launcher) {
            self.close_launcher(true);
            self.dock.menu_from_grid = false;
        }
        self.tray_menu = None;
        self.ext_menu = None;
        self.power_menu = None;
        // The monitor the point is on.
        let output = self.output_state.outputs().find(|o| {
            self.output_state.info(o).is_some_and(|info| {
                let (Some((ox, oy)), Some((w, h))) = (info.logical_position, info.logical_size) else { return false };
                x >= ox && y >= oy && x < ox + w && y < oy + h
            })
        });
        let origin = output
            .as_ref()
            .and_then(|o| self.output_state.info(o))
            .and_then(|i| i.logical_position)
            .unwrap_or((0, 0));
        let surface = self.compositor.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(&self.qh, surface, Layer::Overlay, Some("helix-window-menu"), output.as_ref());
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        layer.set_size(0, 0);
        layer.commit();
        self.window_menu = Some(WindowMenu {
            layer,
            width: 0,
            height: 0,
            configured: false,
            id,
            at: (x - origin.0, y - origin.1),
            origin,
            items,
            selected,
            hits: Vec::new(),
            bounds: Rect { x: 0, y: 0, w: 0, h: 0 },
            sources,
            panel_item: None,
            previews: Vec::new(),
            focus_hidden: false,
            by_keyboard: selected.is_some(),
            parents: Vec::new(),
        });
        if was_lit {
            self.draw_panel();
        }
    }

    pub(super) fn window_menu_configured(&mut self, layer: &LayerSurface, size: (u32, u32)) -> bool {
        let Some(menu) = self.window_menu.as_mut().filter(|m| &m.layer == layer) else { return false };
        menu.width = size.0;
        menu.height = size.1;
        menu.configured = true;
        self.draw_window_menu();
        true
    }

    pub(super) fn window_menu_closed(&mut self, layer: &LayerSurface) -> bool {
        if self.window_menu.as_ref().is_some_and(|m| &m.layer == layer) {
            self.close_window_menu();
            return true;
        }
        false
    }

    pub(super) fn draw_window_menu(&mut self) {
        let Some(mut menu) = self.window_menu.take() else { return };
        if menu.configured && menu.width > 0 && menu.height > 0 {
            self.paint_window_menu(&mut menu);
        }
        self.window_menu = Some(menu);
    }

    fn paint_window_menu(&mut self, menu: &mut WindowMenu) {
        let (w, h) = (menu.width as usize, menu.height as usize);
        let scale = self.scales.of(menu.layer.wl_surface());
        let Ok((buffer, data)) =
            self.buffers.create_buffer(menu.layer.wl_surface(), Ui::draw_window_menu, physical(w, scale) as i32, physical(h, scale) as i32, (physical(w, scale) * 4) as i32, wl_shm::Format::Argb8888)
        else {
            return;
        };
        let mut canvas = Canvas::scaled(data, w, h, scale).mirrored(super::mirror::on(menu.layer.wl_surface()));
        canvas.fill(0x00000000);
        if !menu.previews.is_empty() {
            paint_previews(menu, &mut canvas, w as i32, h as i32, self.text.as_ref(), &self.overview_shots);
            let surface = menu.layer.wl_surface();
            self.scales.prepare(&self.qh, surface, w as i32, h as i32);
            if buffer.attach_to(surface).is_ok() {
                menu.layer.commit();
            }
            return;
        }
        let selected = menu.selected.filter(|_| !menu.focus_hidden);
        let (bounds, hits) = paint_menu(&mut canvas, self.text.as_ref(), (w as i32, h as i32), menu.at, &menu.items, selected, menu.by_keyboard, !menu.sources.is_empty());
        if menu.hits.is_empty() {
            // Where its entries are (for tests): "label@x,y,w,h" on the
            // surface, which covers its monitor.
            let entries: Vec<String> = hits.iter().filter_map(|(r, i)| Some(format!("{}@{},{},{},{}", menu.items.get(*i)?.as_ref()?.label.replace(' ', "_"), r.x, r.y, r.w, r.h))).collect();
            tracing::debug!(origin = ?menu.origin, entries = %entries.join(" "), "menu shown");
        }
        menu.bounds = bounds;
        menu.hits = hits;
        let surface = menu.layer.wl_surface();
        self.scales.prepare(&self.qh, surface, w as i32, h as i32);
        if buffer.attach_to(surface).is_ok() {
            menu.layer.commit();
        }
    }

    /// Show the submenu of entry `index` in place of the menu (a "Back"
    /// entry first), or (`None`) go back to the menu it came from.
    fn window_submenu(&mut self, index: Option<usize>) {
        let Some(menu) = self.window_menu.as_mut() else { return };
        match index {
            Some(index) => {
                let Some(Some(item)) = menu.items.get(index).cloned() else { return };
                let mut items = vec![Some(Item { label: tr("Back").into(), choice: Choice::Back, checked: None, active: false, sub: Vec::new() }), None];
                items.extend(item.sub);
                let parent = std::mem::replace(&mut menu.items, items);
                menu.parents.push((parent, Some(index)));
                menu.selected = Some(0);
            }
            None => {
                let Some((items, selected)) = menu.parents.pop() else { return };
                menu.items = items;
                menu.selected = selected;
            }
        }
        menu.hits.clear();
        self.draw_window_menu();
    }

    /// Carry out entry `index` of the open menu, and close it.
    fn choose_window_menu(&mut self, index: usize) {
        match self.window_menu.as_ref().and_then(|m| m.items.get(index)).and_then(|i| i.as_ref()).map(|i| i.choice) {
            Some(Choice::Submenu) => return self.window_submenu(Some(index)),
            Some(Choice::Back) => return self.window_submenu(None),
            Some(Choice::Disabled) => return,
            _ => {}
        }
        let Some(menu) = self.window_menu.take() else { return };
        if menu.panel_item.is_some() {
            self.draw_panel();
        }
        let Some(Some(item)) = menu.items.get(index) else { return };
        if let Choice::ImProperty(i) = item.choice {
            if let Some((key, kind, state)) = self.ime_menu.get(i).cloned() {
                let state = match kind {
                    crate::ibus::PropKind::Toggle => u32::from(state != 1),
                    crate::ibus::PropKind::Radio => 1,
                    _ => 0,
                };
                let _ = self.requests.send(UiRequest::Ime(crate::ibus::ImRequest::PropertyActivate { key, state }));
            }
            return;
        }
        if item.choice == Choice::CustomizePanel {
            self.open_panel_editor();
            return;
        }
        if item.choice == Choice::Extensions {
            // Settings' Extensions panel when it is installed.
            if !super::extensions_dialog::open_in_settings() {
                self.open_extensions_dialog();
            }
            return;
        }
        if let Choice::ExtMenu(i) = item.choice {
            self.ext_menu_choice(i);
            return;
        }
        if let Choice::Settings(panel) = item.choice {
            self.agent_menu.clear();
            self.background_menu_choice(panel);
            return;
        }
        if let Choice::StopAgent(i) = item.choice {
            if let Some(agent) = std::mem::take(&mut self.agent_menu).into_iter().nth(i) {
                let _ = self.requests.send(UiRequest::StopAgent(agent));
            }
            return;
        }
        if let Choice::DesktopIcon(action) = item.choice {
            self.desktop_icon_menu_choice(action);
            return;
        }
        if let Choice::Tray(i) = item.choice {
            self.tray_menu_choice(i);
            return;
        }
        if let Choice::A11y(i) = item.choice {
            crate::a11y_menu::set(i, !item.checked.unwrap_or(false));
            return;
        }
        if let Choice::Dock(i) = item.choice {
            self.dock_menu_choice(i);
            return;
        }
        if let Choice::InputSource(i) = item.choice {
            if let Some(source) = menu.sources.get(i) {
                crate::input_sources::activate(source);
                self.ime_sync();
                self.refresh_source_label();
            }
            return;
        }
        let Some(list) = self.window_list.as_ref() else { return };
        match item.choice {
            Choice::Action(action) if list.version() >= 4 => list.window_action(menu.id, action),
            Choice::OnAllWorkspaces(on) if list.version() >= 3 => list.set_on_all_workspaces(menu.id, u32::from(on)),
            Choice::Workspace(ws) => list.move_window(menu.id, ws),
            _ => {}
        }
    }

    /// Pointer input on the menu; true if it was for it.
    pub(super) fn window_menu_pointer(&mut self, event: &PointerEvent) -> bool {
        let Some(menu) = self.window_menu.as_mut().filter(|m| m.layer.wl_surface() == &event.surface) else { return false };
        let (x, y) = event.position;
        let under = menu.hits.iter().find(|(r, _)| r.contains(x, y)).map(|(_, i)| *i);
        match event.kind {
            PointerEventKind::Motion { .. } => {
                // Disabled entries do not light up.
                let under = under.filter(|i| menu.items.get(*i).and_then(|i| i.as_ref()).is_some_and(enabled));
                if menu.selected != under || (under.is_some() && menu.focus_hidden) {
                    menu.selected = under;
                    menu.by_keyboard = false;
                    menu.focus_hidden = false;
                    self.draw_window_menu();
                }
            }
            PointerEventKind::Press { .. } => match under {
                Some(index) => self.choose_window_menu(index),
                None if menu.bounds.contains(x, y) => {}
                None => self.close_window_menu(),
            },
            _ => {}
        }
        true
    }

    /// With `keyboard-focus-visible-timeout`, take the keyboard highlight
    /// away after that many seconds without keyboard moves (the
    /// selection stays: Return still chooses it).
    fn hide_focus_later(&mut self) {
        self.focus_serial += 1;
        let (Some(handle), true) = (self.loop_handle.as_ref(), self.focus_timeout > 0) else { return };
        let serial = self.focus_serial;
        let after = std::time::Duration::from_secs(self.focus_timeout as u64);
        let _ = handle.insert_source(smithay_client_toolkit::reexports::calloop::timer::Timer::from_duration(after), move |_, _, ui: &mut Ui| {
            if ui.focus_serial == serial {
                if let Some(menu) = ui.window_menu.as_mut() {
                    menu.focus_hidden = true;
                }
                ui.draw_window_menu();
            }
            smithay_client_toolkit::reexports::calloop::timer::TimeoutAction::Drop
        });
        if let Some(menu) = self.window_menu.as_mut() {
            menu.focus_hidden = false;
        }
    }

    /// Keys while the menu is open.
    pub(super) fn window_menu_key(&mut self, event: &KeyEvent) {
        let Some(menu) = self.window_menu.as_mut() else { return };
        let typed = event.utf8.as_deref().and_then(|s| s.chars().next()).filter(|c| c.is_alphanumeric());
        match menu_key(&menu.items, menu.selected, event.keysym, typed, !menu.parents.is_empty()) {
            MenuKey::None => {}
            MenuKey::Close => self.close_window_menu(),
            MenuKey::Back => self.window_submenu(None),
            MenuKey::Open(index) => self.window_submenu(Some(index)),
            MenuKey::Choose(index) => self.choose_window_menu(index),
            MenuKey::Select(index) => {
                menu.selected = Some(index);
                menu.by_keyboard = true;
                self.draw_window_menu();
                self.hide_focus_later();
            }
        }
    }
}

/// Paint a menu of `items` at `at` (kept on the `w`×`h` surface) with
/// `selected` lit; `sources`: an input source menu (dots for the current
/// one). Returns its box and each entry's place.
#[allow(clippy::too_many_arguments)]
pub(super) fn paint_menu(canvas: &mut Canvas, text: Option<&Text>, (w, h): (i32, i32), at: (i32, i32), items: &[Option<Item>], selected: Option<usize>, ring: bool, sources: bool) -> (Rect, Vec<(Rect, usize)>) {
    let item_h = item_h();
    let radio_marks = sources || items.iter().flatten().any(|i| i.active);
    // Wide enough for the longest label (translations run longer than
    // English), up to twice the usual width; longer labels are shortened.
    let needed = items
        .iter()
        .flatten()
        .map(|item| {
            let extra = 12 + if item.checked.is_some() { 24 } else { 0 } + if radio_marks { 16 } else { 0 } + if item.choice == Choice::Back { 14 } else { 0 } + 24 + 8;
            text.map_or(0, |t| t.width(&item.label, size::LABEL).ceil() as i32) + extra
        })
        .max()
        .unwrap_or(0);
    let menu_w = needed.clamp(menu_w(), menu_w() * 2).min((w - 16).max(menu_w()));
    let mh: i32 = 8 + items.iter().map(|i| if i.is_some() { item_h } else { SEPARATOR_H }).sum::<i32>();
    // Keep it on the output.
    let mx = at.0.clamp(0, (w - menu_w).max(0));
    let my = at.1.clamp(0, (h - mh).max(0));
    theme::popover(canvas, mx, my, menu_w, mh);
    let bounds = Rect { x: mx, y: my, w: menu_w, h: mh };
    let mut hits = Vec::new();
    let mut y = my + 4;
    // Radio items show which one is on (`radio_marks`).
    for (i, item) in items.iter().enumerate() {
        let Some(item) = item else {
            theme::separator(canvas, mx + 8, y + SEPARATOR_H / 2, menu_w - 16);
            y += SEPARATOR_H;
            continue;
        };
        let r = Rect { x: mx + 4, y, w: menu_w - 8, h: item_h };
        if selected == Some(i) {
            // Selected from the keyboard: the focus ring too.
            theme::row_selected(canvas, r.x, r.y, r.w, r.h, false, ring);
        }
        let base = baseline(text, r.y, r.h, size::LABEL);
        let mut x = r.x + 12;
        if let Some(checked) = item.checked {
            theme::check_box(canvas, f64::from(x), f64::from(r.y + (r.h - 16) / 2), 16.0, checked, theme::Look::default());
            x += 24;
        }
        if radio_marks {
            if item.active {
                canvas.circle(f64::from(x) + 4.0, f64::from(r.y + r.h / 2), 3.5, accent());
            }
            x += 16;
        }
        // A submenu: an arrow at the end; "Back": one at the start.
        let cy = f64::from(r.y + r.h / 2);
        match item.choice {
            Choice::Submenu => theme::chevron(canvas, f64::from(r.x + r.w - 14), cy, 10.0, theme::Towards::Right, DIM_FG),
            Choice::Back => {
                theme::chevron(canvas, f64::from(x) + 2.0, cy, 10.0, theme::Towards::Left, DIM_FG);
                x += 14;
            }
            _ => {}
        }
        let color = if item.choice == Choice::Disabled { disabled_text() } else { FG };
        label(text, canvas, &item.label, x as f32, base, size::LABEL, color, (r.x + r.w - x - 24) as f32);
        hits.push((r, i));
        y += item_h;
    }
    (bounds, hits)
}

/// A disabled entry's label: dimmed, and in high contrast (where all text
/// is white) white at 60 % (still 7:1 on black).
pub(super) fn disabled_text() -> u32 {
    theme::disabled_text()
}

/// Whether an entry can be chosen.
pub(super) fn enabled(item: &Item) -> bool {
    item.choice != Choice::Disabled
}

/// The first entry that can be chosen.
pub(super) fn first_enabled(items: &[Option<Item>]) -> Option<usize> {
    items.iter().position(|i| i.as_ref().is_some_and(enabled))
}

/// What a key does in a menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MenuKey {
    None,
    Close,
    /// Select an entry.
    Select(usize),
    /// Choose an entry (Return).
    Choose(usize),
    /// Show an entry's submenu; back from a submenu.
    Open(usize),
    Back,
}

/// What `keysym` (typing `typed`) does with `selected` of `items`;
/// `in_submenu`: a submenu is shown.
pub(super) fn menu_key(items: &[Option<Item>], selected: Option<usize>, keysym: Keysym, typed: Option<char>, in_submenu: bool) -> MenuKey {
    let choosable: Vec<usize> = items.iter().enumerate().filter(|(_, i)| i.as_ref().is_some_and(enabled)).map(|(n, _)| n).collect();
    let pos = selected.and_then(|s| choosable.iter().position(|n| *n == s));
    let item = selected.and_then(|s| items.get(s)).and_then(|i| i.as_ref());
    let n = choosable.len();
    match keysym {
        Keysym::Escape => MenuKey::Close,
        Keysym::Left | Keysym::KP_Left if in_submenu => MenuKey::Back,
        Keysym::Right | Keysym::KP_Right => match (selected, item) {
            (Some(index), Some(i)) if i.choice == Choice::Submenu => MenuKey::Open(index),
            _ => MenuKey::None,
        },
        _ if n == 0 => MenuKey::None,
        Keysym::Down | Keysym::KP_Down => MenuKey::Select(choosable[pos.map_or(0, |p| (p + 1) % n)]),
        Keysym::Up | Keysym::KP_Up => MenuKey::Select(choosable[pos.map_or(n - 1, |p| (p + n - 1) % n)]),
        Keysym::Home | Keysym::KP_Home | Keysym::Page_Up => MenuKey::Select(choosable[0]),
        Keysym::End | Keysym::KP_End | Keysym::Page_Down => MenuKey::Select(choosable[n - 1]),
        Keysym::Return | Keysym::KP_Enter | Keysym::space => match (selected, item) {
            (Some(index), Some(i)) if enabled(i) => MenuKey::Choose(index),
            _ => MenuKey::None,
        },
        _ => {
            // Type-ahead: the next entry (after the selected one, wrapping)
            // starting with the letter typed.
            let Some(c) = typed else { return MenuKey::None };
            let starts = |index: &usize| items[*index].as_ref().and_then(|i| i.label.chars().next()).is_some_and(|f| f.to_lowercase().eq(c.to_lowercase()));
            let from = pos.map_or(0, |p| p + 1);
            (0..n).map(|k| choosable[(from + k) % n]).find(starts).map_or(MenuKey::None, MenuKey::Select)
        }
    }
}

impl WindowMenu {
    /// The surface, its output-relative size, and whether it is a box of
    /// window previews (`ui/atspi.rs`).
    pub(super) fn a11y_surface(&self) -> Option<super::atspi::SurfaceAt<'_>> {
        (self.configured && self.width > 0).then_some((&self.layer, (self.origin.0, self.origin.1, self.width, self.height)))
    }

    /// The menu on the accessibility bus.
    pub(super) fn a11y(&self, title: &str) -> crate::a11y::Node {
        menu_node(title, &self.items, &self.hits, self.selected, !self.previews.is_empty(), self.parents.len())
    }

    /// The entries, for naming the menu.
    pub(super) fn items(&self) -> &[Option<Item>] {
        &self.items
    }
}

/// A shell menu on the accessibility bus (helixdesktop/spec
/// `contracts/dbus/org.a11y.atspi.md`, "Menus"): a `menu` named `title`
/// of its entries where they were painted (`hits`), separators between.
/// Check box entries are check menu items, radio entries (input sources,
/// engine modes) radio menu items, checked as set; an entry with a
/// submenu is expandable; disabled entries are insensitive. The selected
/// entry (keyboard or pointer) is selected and has the focus; with none
/// the menu has it. Window previews are a `menu` of each window's entry
/// (named by its title) and its `Close` (described by the title).
pub(super) fn menu_node(title: &str, items: &[Option<Item>], hits: &[(Rect, usize)], selected: Option<usize>, previews: bool, depth: usize) -> crate::a11y::Node {
    use crate::a11y::{Node, Role};
    let radio = items.iter().flatten().any(|i| i.active);
    let mut entries = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let Some(item) = item else {
            if !previews {
                entries.push(Node::new(Role::Splitter, "").id(format!("separator:{i}")));
            }
            continue;
        };
        let Some(r) = hits.iter().find(|(_, n)| *n == i).map(|(r, _)| crate::a11y::Rect::from(*r)) else { continue };
        let on = selected == Some(i);
        let node = match (item.checked, item.choice) {
            (Some(checked), _) => Node::new(Role::MenuItemCheckBox, &item.label).toggled(checked),
            (None, Choice::Submenu) => Node::new(Role::MenuItem, &item.label).expanded(false),
            _ if radio => Node::new(Role::MenuItemRadio, &item.label).toggled(item.active),
            _ => Node::new(Role::MenuItem, &item.label),
        };
        let node = if previews && i % 2 == 1 {
            // A window's close button: which window.
            let window = items.get(i - 1).and_then(|w| w.as_ref()).map_or("", |w| w.label.as_str());
            node.describe(window)
        } else {
            node
        };
        entries.push(node.id(format!("{depth}:{i}")).at(r).click().disabled(!enabled(item)).focusable().selected(on).focused(on));
    }
    // Separators only between entries shown.
    while entries.last().is_some_and(|n| n.role == Role::Splitter) {
        entries.pop();
    }
    Node::new(Role::Menu, title).id("menu").focused(selected.is_none()).children(entries)
}

/// Window previews: tiles with each window's picture (its outline
/// until the picture comes), title and a close button.
fn paint_previews(menu: &mut WindowMenu, canvas: &mut Canvas, w: i32, h: i32, text: Option<&Text>, shots: &super::overview::Shots) {
    let (mw, mh) = previews_size(menu.previews.len());
    let mx = menu.at.0.clamp(0, (w - mw).max(0));
    let my = menu.at.1.clamp(0, (h - mh).max(0));
    theme::popover(canvas, mx, my, mw, mh);
    menu.bounds = Rect { x: mx, y: my, w: mw, h: mh };
    menu.hits.clear();
    for (i, window) in menu.previews.iter().enumerate() {
        let (col, row) = ((i % TILES_PER_ROW) as i32, (i / TILES_PER_ROW) as i32);
        let tile = Rect { x: mx + TILE_GAP + col * (TILE_W + TILE_GAP), y: my + TILE_GAP + row * (PICTURE_H + TILE_TITLE_H + TILE_GAP), w: TILE_W, h: PICTURE_H + TILE_TITLE_H };
        if menu.selected == Some(2 * i) && !menu.focus_hidden {
            theme::row_highlight(canvas, tile.x - 2, tile.y - 2, tile.w + 4, tile.h + 4);
            if menu.by_keyboard {
                theme::focus_ring(canvas, tile.x, tile.y, tile.w, tile.h);
            }
        }
        // The picture, fitted into the tile keeping its proportions.
        let pic = Rect { x: tile.x + 4, y: tile.y + 4, w: tile.w - 8, h: PICTURE_H - 8 };
        match shots.picture(*window) {
            Some(image) if image.width > 0 && image.height > 0 => {
                let s = (f64::from(pic.w) / (image.width as f64)).min(f64::from(pic.h) / (image.height as f64)).min(1.0);
                let (iw, ih) = ((image.width as f64) * s, (image.height as f64) * s);
                canvas.image_rounded(f64::from(pic.x) + (f64::from(pic.w) - iw) / 2.0, f64::from(pic.y) + (f64::from(pic.h) - ih) / 2.0, iw, ih, theme::RADIUS_ROW, image, 1.0);
            }
            _ => theme::control(canvas, pic.x, pic.y, pic.w, pic.h, theme::Look::default()),
        }
        let title = menu.items.get(2 * i).and_then(|i| i.as_ref()).map(|i| i.label.clone()).unwrap_or_default();
        label(text, canvas, &title, (tile.x + 6) as f32, baseline(text, tile.y + PICTURE_H, TILE_TITLE_H, size::CAPTION), size::CAPTION, FG, (tile.w - 12) as f32);
        // The close button, at the picture's top right corner.
        let close = Rect { x: tile.x + tile.w - 26, y: tile.y + 2, w: 24, h: 24 };
        let hot = menu.selected == Some(2 * i + 1) && !menu.focus_hidden;
        let (cx, cy) = (f64::from(close.x) + 12.0, f64::from(close.y) + 12.0);
        theme::round_control(canvas, cx, cy, 20.0, theme::Look { hovered: hot, ..Default::default() }, hot && menu.by_keyboard);
        theme::close_symbol(canvas, cx, cy, 9.0, FG);
        // The close button first: it lies on the tile.
        menu.hits.push((close, 2 * i + 1));
        menu.hits.push((tile, 2 * i));
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn labels(items: &[Option<Item>]) -> Vec<&str> {
        items.iter().map(|i| i.as_ref().map_or("-", |i| i.label.as_str())).collect()
    }

    fn p(key: &str, kind: crate::ibus::PropKind, visible: bool, sub: Vec<crate::ibus::Prop>) -> crate::ibus::Prop {
        crate::ibus::Prop { key: key.into(), kind, label: key.into(), state: 0, sensitive: true, visible, symbol: String::new(), sub }
    }

    fn item(label: &str, choice: Choice) -> Option<Item> {
        Some(Item { label: label.into(), choice, checked: None, active: false, sub: Vec::new() })
    }

    #[test]
    fn keyboard() {
        let items = vec![
            item("Not available", Choice::Disabled),
            item("Minimize", Choice::Action(WindowAction::Minimize)),
            None,
            item("Move to Monitor", Choice::Submenu),
            item("Disabled too", Choice::Disabled),
            item("Close", Choice::Action(WindowAction::Close)),
        ];
        assert_eq!(first_enabled(&items), Some(1), "keyboard-opened menus start on the first enabled entry");
        let key = |selected, k| menu_key(&items, selected, k, None, false);
        assert_eq!(key(None, Keysym::Down), MenuKey::Select(1));
        assert_eq!(key(None, Keysym::Up), MenuKey::Select(5));
        assert_eq!(key(Some(3), Keysym::Down), MenuKey::Select(5), "disabled entries are skipped");
        assert_eq!(key(Some(5), Keysym::Down), MenuKey::Select(1), "wrapping, past the disabled first entry");
        assert_eq!(key(Some(1), Keysym::Up), MenuKey::Select(5));
        assert_eq!(key(Some(3), Keysym::Home), MenuKey::Select(1));
        assert_eq!(key(Some(1), Keysym::End), MenuKey::Select(5));
        // Right only opens submenus; Left only goes back.
        assert_eq!(key(Some(3), Keysym::Right), MenuKey::Open(3));
        assert_eq!(key(Some(1), Keysym::Right), MenuKey::None);
        assert_eq!(key(Some(1), Keysym::Left), MenuKey::None);
        assert_eq!(menu_key(&items, Some(1), Keysym::Left, None, true), MenuKey::Back);
        assert_eq!(key(Some(5), Keysym::Return), MenuKey::Choose(5));
        assert_eq!(key(Some(0), Keysym::Return), MenuKey::None, "a disabled entry does nothing");
        assert_eq!(key(None, Keysym::Return), MenuKey::None);
        // Type-ahead.
        let typed = |selected, c| menu_key(&items, selected, Keysym::c, Some(c), false);
        assert_eq!(typed(None, 'c'), MenuKey::Select(5));
        assert_eq!(typed(Some(1), 'M'), MenuKey::Select(3), "the next one starting with M");
        assert_eq!(typed(Some(3), 'm'), MenuKey::Select(1), "wrapping");
        assert_eq!(typed(Some(1), 'd'), MenuKey::None, "disabled ones are not reached");
        assert_eq!(key(Some(1), Keysym::Escape), MenuKey::Close);
    }

    #[test]
    fn property_menu() {
        use crate::ibus::PropKind::*;
        let props = vec![
            p("sep", Separator, true, vec![]),
            p("InputMode", Menu, true, vec![p("Hira", Radio, true, vec![]), p("Kata", Radio, true, vec![]), p("hidden", Radio, false, vec![]), p("More", Menu, true, vec![p("Half", Radio, true, vec![])])]),
            p("Wide", Toggle, true, vec![]),
            p("Gone", Normal, false, vec![]),
            p("Empty", Menu, true, vec![p("x", Radio, false, vec![])]),
            p("sep2", Separator, true, vec![]),
        ];
        let mut leaf = |p: &crate::ibus::Prop| Item { label: p.key.clone(), choice: Choice::Back, checked: None, active: false, sub: Vec::new() };
        let items = property_items(&props, &mut leaf);
        assert_eq!(labels(&items), ["InputMode", "Wide"]);
        let menu = items[0].as_ref().unwrap();
        assert_eq!(menu.choice, Choice::Submenu);
        assert_eq!(labels(&menu.sub), ["Hira", "Kata", "More"]);
        assert_eq!(labels(&menu.sub[2].as_ref().unwrap().sub), ["Half"], "menus in menus");
    }

    #[test]
    fn entries() {
        let plain = items(MenuState::empty(), Some(0), 1);
        assert_eq!(
            labels(&plain),
            ["Minimize", "Maximize", "Move", "Resize", "-", "Always on Top", "Always on Visible Workspace", "-", "Close"]
        );
        let busy = items(MenuState::Maximized | MenuState::Above | MenuState::MonitorRight, Some(1), 3);
        assert_eq!(
            labels(&busy),
            [
                "Minimize",
                "Restore",
                "-",
                "Always on Top",
                "Always on Visible Workspace",
                "Move to Workspace Left",
                "Move to Workspace Right",
                "-",
                "Move to Monitor Right",
                "-",
                "Close"
            ]
        );
        assert_eq!(busy[3].as_ref().unwrap().checked, Some(true));
        assert_eq!(busy[5].as_ref().unwrap().choice, Choice::Workspace(0));
        let sticky = items(MenuState::OnAllWorkspaces, Some(ALL_WORKSPACES), 3);
        assert!(!labels(&sticky).contains(&"Move to Workspace Right"));
        assert_eq!(sticky[6].as_ref().unwrap().choice, Choice::OnAllWorkspaces(false));
    }
}
