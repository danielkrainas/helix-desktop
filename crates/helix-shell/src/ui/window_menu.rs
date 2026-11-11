//! The window menu (helix-desktop-spec `contracts/wayland/README.md`,
//! "Window menu and window keys"): drawn by the shell where the compositor
//! says, its choices carried out by the compositor.

use helix_protocols::client::helix_window_list_v1::MenuState;

use super::*;
use crate::windows::{WindowAction, ALL_WORKSPACES};

const MENU_W: i32 = 240;
const ITEM_H: i32 = 32;
const SEPARATOR_H: i32 = 9;

/// What a menu entry does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Choice {
    Action(WindowAction),
    OnAllWorkspaces(bool),
    Workspace(u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Item {
    pub label: &'static str,
    pub choice: Choice,
    /// A check box, and whether it is checked.
    pub checked: Option<bool>,
}

/// The entries for a window in `state` on `workspace` of `count`; `None`
/// is a separator.
pub(super) fn items(state: MenuState, workspace: Option<u32>, count: u32) -> Vec<Option<Item>> {
    let item = |label, choice| Some(Item { label, choice, checked: None });
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
    out.push(Some(Item { label: "Always on Top", choice: Choice::Action(WindowAction::ToggleAbove), checked: Some(above) }));
    let all = state.contains(MenuState::OnAllWorkspaces);
    out.push(Some(Item { label: "Always on Visible Workspace", choice: Choice::OnAllWorkspaces(!all), checked: Some(all) }));
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
}

impl Ui {
    /// The compositor asks for the menu of window `id` at (x, y).
    pub(super) fn show_window_menu(&mut self, id: u32, x: i32, y: i32, state: MenuState) {
        self.close_launcher(true);
        self.power_menu = None;
        let workspace = self.windows.workspace_of(id);
        let items = items(state, workspace, self.windows.count);
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
            label(text, &mut canvas, item.label, x as f32, base, 14.0, FG, (r.x + r.w - x - 8) as f32);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(items: &[Option<Item>]) -> Vec<&'static str> {
        items.iter().map(|i| i.as_ref().map_or("-", |i| i.label)).collect()
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
