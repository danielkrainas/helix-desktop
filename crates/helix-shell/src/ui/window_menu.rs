//! The window menu (helix-desktop-spec `contracts/wayland/README.md`,
//! "Window menu and window keys"): drawn by the shell where the compositor
//! says, its choices carried out by the compositor.

use helix_protocols::client::helix_window_list_v1::MenuState;

use super::*;
use crate::windows::{WindowAction, ALL_WORKSPACES};

const MENU_W: i32 = 240;
const ITEM_H: i32 = 32;
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Item {
    pub label: String,
    pub choice: Choice,
    /// A check box, and whether it is checked.
    pub checked: Option<bool>,
    /// The current one of a set (drawn with a dot).
    pub active: bool,
}

/// The entries for a window in `state` on `workspace` of `count`; `None`
/// is a separator.
pub(super) fn items(state: MenuState, workspace: Option<u32>, count: u32) -> Vec<Option<Item>> {
    let item = |label: &str, choice| Some(Item { label: label.to_owned(), choice, checked: None, active: false });
    let maximized = state.contains(MenuState::Maximized);
    let fixed = maximized || state.contains(MenuState::Fullscreen);
    let mut out = vec![item("Minimize", Choice::Action(WindowAction::Minimize))];
    out.push(if maximized {
        item("Restore", Choice::Action(WindowAction::Unmaximize))
    } else {
        item("Maximize", Choice::Action(WindowAction::Maximize))
    });
    if !fixed {
        out.push(item("Move", Choice::Action(WindowAction::BeginMove)));
        out.push(item("Resize", Choice::Action(WindowAction::BeginResize)));
    }
    out.push(None);
    let above = state.contains(MenuState::Above);
    out.push(Some(Item { label: "Always on Top".into(), choice: Choice::Action(WindowAction::ToggleAbove), checked: Some(above), active: false }));
    let all = state.contains(MenuState::OnAllWorkspaces);
    out.push(Some(Item { label: "Always on Visible Workspace".into(), choice: Choice::OnAllWorkspaces(!all), checked: Some(all), active: false }));
    if let Some(ws) = workspace.filter(|w| !all && *w != ALL_WORKSPACES) {
        if ws > 0 {
            out.push(item("Move to Workspace Left", Choice::Workspace(ws - 1)));
        }
        if ws + 1 < count {
            out.push(item("Move to Workspace Right", Choice::Workspace(ws + 1)));
        }
    }
    let monitors = [
        (MenuState::MonitorLeft, "Move to Monitor Left", WindowAction::MoveToMonitorLeft),
        (MenuState::MonitorRight, "Move to Monitor Right", WindowAction::MoveToMonitorRight),
        (MenuState::MonitorUp, "Move to Monitor Up", WindowAction::MoveToMonitorUp),
        (MenuState::MonitorDown, "Move to Monitor Down", WindowAction::MoveToMonitorDown),
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
    out.push(item("Close", Choice::Action(WindowAction::Close)));
    out
}

/// The menu entries for engine properties (`None`: a separator): visible
/// and sensitive ones, a menu's entries in its place, separators only
/// between entries.
pub(super) fn property_entries(props: &[crate::ibus::Prop]) -> Vec<Option<&crate::ibus::Prop>> {
    use crate::ibus::PropKind;
    let mut out: Vec<Option<&crate::ibus::Prop>> = Vec::new();
    for p in props.iter().filter(|p| p.visible) {
        match p.kind {
            PropKind::Separator => out.push(None),
            PropKind::Menu => {
                out.push(None);
                out.extend(p.sub.iter().filter(|s| s.visible && s.sensitive && s.kind != PropKind::Separator && s.kind != PropKind::Menu).map(Some));
                out.push(None);
            }
            _ if p.sensitive => out.push(Some(p)),
            _ => {}
        }
    }
    // No separator first, last or twice.
    let mut clean: Vec<Option<&crate::ibus::Prop>> = Vec::new();
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
    items: Vec<Option<Item>>,
    /// Keyboard selection (index into `items`).
    selected: Option<usize>,
    hits: Vec<(Rect, usize)>,
    /// Where the box is, for clicks outside it.
    bounds: Rect,
    /// The input sources an input source menu is for.
    sources: Vec<crate::input_sources::Source>,
    /// Window previews instead of rows: the windows; the items are, for
    /// each, "activate" then "close".
    pub(super) previews: Vec<u32>,
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
        let workspace = self.windows.workspace_of(id);
        let items = items(state, workspace, self.windows.count);
        self.open_menu(id, x, y, items, Vec::new());
    }

    /// The panel's input source menu, below its indicator at `x`
    /// (helix-desktop-spec `contracts/gsettings/input-settings.md`,
    /// "Input source indicator").
    pub(super) fn show_sources_menu(&mut self, x: i32) {
        let sources = crate::input_sources::current();
        let mut items: Vec<Option<Item>> = sources
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let label = if s.0 == "xkb" { crate::input_sources::description(&s.1).unwrap_or_else(|| s.1.clone()) } else { s.1.clone() };
                Some(Item { label, choice: Choice::InputSource(i), checked: None, active: i == 0 })
            })
            .collect();
        // The IBus engine's properties, below its source.
        self.ime_menu.clear();
        if sources.first().is_some_and(|s| s.0 == "ibus") {
            let props = property_entries(&self.ime_props);
            if !props.is_empty() {
                items.push(None);
            }
            for entry in props {
                match entry {
                    None => items.push(None),
                    Some(p) => {
                        let index = self.ime_menu.len();
                        self.ime_menu.push((p.key.clone(), p.kind, p.state));
                        let checked = (p.kind == crate::ibus::PropKind::Toggle).then_some(p.state == 1);
                        let active = p.kind == crate::ibus::PropKind::Radio && p.state == 1;
                        items.push(Some(Item { label: p.label.clone(), choice: Choice::ImProperty(index), checked, active }));
                    }
                }
            }
        }
        self.open_menu(0, x, PANEL_HEIGHT as i32, items, sources);
    }

    /// The panel's accessibility menu, below its indicator at `x`.
    pub(super) fn show_a11y_menu(&mut self, x: i32) {
        let items = crate::a11y_menu::SWITCHES
            .iter()
            .zip(&self.a11y.switches)
            .enumerate()
            .filter_map(|(i, (s, state))| Some(Some(Item { label: s.label.to_owned(), choice: Choice::A11y(i), checked: Some((*state)?), active: false })))
            .collect();
        self.open_menu(0, x, PANEL_HEIGHT as i32, items, Vec::new());
    }

    pub(super) fn open_menu(&mut self, id: u32, x: i32, y: i32, items: Vec<Option<Item>>, sources: Vec<crate::input_sources::Source>) {
        self.close_launcher(true);
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
            items,
            selected: None,
            hits: Vec::new(),
            bounds: Rect { x: 0, y: 0, w: 0, h: 0 },
            sources,
            previews: Vec::new(),
        });
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
            self.window_menu = None;
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
            self.pool.create_buffer(physical(w, scale) as i32, physical(h, scale) as i32, (physical(w, scale) * 4) as i32, wl_shm::Format::Argb8888)
        else {
            return;
        };
        let mut canvas = Canvas::scaled(data, w, h, scale);
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
        let mh: i32 = 8 + menu.items.iter().map(|i| if i.is_some() { ITEM_H } else { SEPARATOR_H }).sum::<i32>();
        // Keep it on the output.
        let mx = menu.at.0.clamp(0, (w as i32 - MENU_W).max(0));
        let my = menu.at.1.clamp(0, (h as i32 - mh).max(0));
        canvas.rect(mx - 1, my - 1, MENU_W + 2, mh + 2, BANNER_BORDER);
        canvas.rect(mx, my, MENU_W, mh, BANNER_BG);
        menu.bounds = Rect { x: mx, y: my, w: MENU_W, h: mh };
        menu.hits.clear();
        let text = self.text.as_ref();
        let mut y = my + 4;
        for (i, item) in menu.items.iter().enumerate() {
            let Some(item) = item else {
                canvas.rect(mx + 8, y + SEPARATOR_H / 2, MENU_W - 16, 1, BANNER_BORDER);
                y += SEPARATOR_H;
                continue;
            };
            let r = Rect { x: mx + 4, y, w: MENU_W - 8, h: ITEM_H };
            if menu.selected == Some(i) {
                canvas.rect(r.x, r.y, r.w, r.h, HOVER_BG);
            }
            let base = baseline(text, r.y, r.h, 14.0);
            let mut x = r.x + 12;
            if let Some(checked) = item.checked {
                let (bx, by) = (x, r.y + (r.h - 14) / 2);
                if checked {
                    canvas.rect(bx, by, 14, 14, ACCENT);
                    canvas.line((f64::from(bx) + 3.0, f64::from(by) + 7.5), (f64::from(bx) + 6.0, f64::from(by) + 10.5), 1.8, FG);
                    canvas.line((f64::from(bx) + 6.0, f64::from(by) + 10.5), (f64::from(bx) + 11.0, f64::from(by) + 3.5), 1.8, FG);
                } else {
                    for (rx, ry, rw, rh) in [(bx, by, 14, 1), (bx, by + 13, 14, 1), (bx, by, 1, 14), (bx + 13, by, 1, 14)] {
                        canvas.rect(rx, ry, rw, rh, DIM_FG);
                    }
                }
                x += 22;
            }
            if !menu.sources.is_empty() {
                if item.active {
                    canvas.circle(f64::from(x) + 4.0, f64::from(r.y + r.h / 2), 3.5, ACCENT);
                }
                x += 16;
            }
            label(text, &mut canvas, &item.label, x as f32, base, 14.0, FG, (r.x + r.w - x - 8) as f32);
            menu.hits.push((r, i));
            y += ITEM_H;
        }
        let surface = menu.layer.wl_surface();
        self.scales.prepare(&self.qh, surface, w as i32, h as i32);
        if buffer.attach_to(surface).is_ok() {
            menu.layer.commit();
        }
    }

    /// Carry out entry `index` of the open menu, and close it.
    fn choose_window_menu(&mut self, index: usize) {
        let Some(menu) = self.window_menu.take() else { return };
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
                if menu.selected != under {
                    menu.selected = under;
                    self.draw_window_menu();
                }
            }
            PointerEventKind::Press { .. } => match under {
                Some(index) => self.choose_window_menu(index),
                None if menu.bounds.contains(x, y) => {}
                None => self.window_menu = None,
            },
            _ => {}
        }
        true
    }

    /// Keys while the menu is open: arrows, Return, Escape.
    pub(super) fn window_menu_key(&mut self, event: &KeyEvent) {
        let Some(menu) = self.window_menu.as_mut() else { return };
        let selectable: Vec<usize> = menu.items.iter().enumerate().filter(|(_, i)| i.is_some()).map(|(n, _)| n).collect();
        let pos = menu.selected.and_then(|s| selectable.iter().position(|n| *n == s));
        match event.keysym {
            Keysym::Escape => self.window_menu = None,
            Keysym::Down => {
                menu.selected = Some(selectable[pos.map_or(0, |p| (p + 1) % selectable.len())]);
                self.draw_window_menu();
            }
            Keysym::Up => {
                menu.selected = Some(selectable[pos.map_or(selectable.len() - 1, |p| (p + selectable.len() - 1) % selectable.len())]);
                self.draw_window_menu();
            }
            Keysym::Return | Keysym::KP_Enter | Keysym::space => {
                if let Some(index) = menu.selected {
                    self.choose_window_menu(index);
                }
            }
            _ => {}
        }
    }
}

/// Window previews: tiles with each window's picture (its outline
/// until the picture comes), title and a close button.
fn paint_previews(menu: &mut WindowMenu, canvas: &mut Canvas, w: i32, h: i32, text: Option<&Text>, shots: &super::overview::Shots) {
    let (mw, mh) = previews_size(menu.previews.len());
    let mx = menu.at.0.clamp(0, (w - mw).max(0));
    let my = menu.at.1.clamp(0, (h - mh).max(0));
    canvas.rect(mx - 1, my - 1, mw + 2, mh + 2, BANNER_BORDER);
    canvas.rect(mx, my, mw, mh, BANNER_BG);
    menu.bounds = Rect { x: mx, y: my, w: mw, h: mh };
    menu.hits.clear();
    for (i, window) in menu.previews.iter().enumerate() {
        let (col, row) = ((i % TILES_PER_ROW) as i32, (i / TILES_PER_ROW) as i32);
        let tile = Rect { x: mx + TILE_GAP + col * (TILE_W + TILE_GAP), y: my + TILE_GAP + row * (PICTURE_H + TILE_TITLE_H + TILE_GAP), w: TILE_W, h: PICTURE_H + TILE_TITLE_H };
        if menu.selected == Some(2 * i) {
            canvas.rect(tile.x - 2, tile.y - 2, tile.w + 4, tile.h + 4, HOVER_BG);
        }
        // The picture, fitted into the tile keeping its proportions.
        let pic = Rect { x: tile.x + 4, y: tile.y + 4, w: tile.w - 8, h: PICTURE_H - 8 };
        match shots.picture(*window) {
            Some(image) if image.width > 0 && image.height > 0 => {
                let s = (f64::from(pic.w) / (image.width as f64)).min(f64::from(pic.h) / (image.height as f64)).min(1.0);
                let (iw, ih) = ((image.width as f64) * s, (image.height as f64) * s);
                canvas.image(f64::from(pic.x) + (f64::from(pic.w) - iw) / 2.0, f64::from(pic.y) + (f64::from(pic.h) - ih) / 2.0, iw, ih, image);
            }
            _ => canvas.rect(pic.x, pic.y, pic.w, pic.h, 0xFF55555F),
        }
        let title = menu.items.get(2 * i).and_then(|i| i.as_ref()).map(|i| i.label.clone()).unwrap_or_default();
        label(text, canvas, &title, (tile.x + 6) as f32, baseline(text, tile.y + PICTURE_H, TILE_TITLE_H, 13.0), 13.0, FG, (tile.w - 12) as f32);
        // The close button, at the picture's top right corner.
        let close = Rect { x: tile.x + tile.w - 26, y: tile.y + 2, w: 24, h: 24 };
        let hot = menu.selected == Some(2 * i + 1);
        canvas.circle(f64::from(close.x) + 12.0, f64::from(close.y) + 12.0, 10.0, if hot { 0xFFE01B24 } else { 0xE0303036 });
        canvas.line((f64::from(close.x) + 8.0, f64::from(close.y) + 8.0), (f64::from(close.x) + 16.0, f64::from(close.y) + 16.0), 1.8, FG);
        canvas.line((f64::from(close.x) + 16.0, f64::from(close.y) + 8.0), (f64::from(close.x) + 8.0, f64::from(close.y) + 16.0), 1.8, FG);
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

    #[test]
    fn property_menu() {
        use crate::ibus::PropKind::*;
        let props = vec![
            p("sep", Separator, true, vec![]),
            p("InputMode", Menu, true, vec![p("Hira", Radio, true, vec![]), p("Kata", Radio, true, vec![]), p("hidden", Radio, false, vec![])]),
            p("Wide", Toggle, true, vec![]),
            p("Gone", Normal, false, vec![]),
            p("sep2", Separator, true, vec![]),
        ];
        let keys: Vec<&str> = property_entries(&props).iter().map(|e| e.map_or("-", |p| p.key.as_str())).collect();
        assert_eq!(keys, ["Hira", "Kata", "-", "Wide"]);
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
