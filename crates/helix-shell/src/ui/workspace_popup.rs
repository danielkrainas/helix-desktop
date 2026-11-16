//! The workspace switch popup (helix-desktop-spec
//! `contracts/gsettings/workspaces.md`, "Indicator"): shown for a moment
//! when a key switches workspaces.

use super::*;
use helix_i18n::trf;

/// How long it stays after the last switch.
const SHOWN: Duration = Duration::from_millis(750);
const BOX_W: i32 = 64;
const BOX_H: i32 = 40;
const GAP: i32 = 8;
const PAD: i32 = 16;
const NAME_H: i32 = 30;

/// `workspace-names`: entry `n` (0-based), else "Workspace n+1".
pub(super) fn workspace_name(names: &[String], n: u32) -> String {
    names
        .get(n as usize)
        .filter(|name| !name.trim().is_empty())
        .cloned()
        .unwrap_or_else(|| trf!("Workspace {}", n + 1))
}

pub(super) fn read_names() -> Vec<String> {
    const SCHEMA: &str = "org.gnome.desktop.wm.preferences";
    if !helix_settings::has_schema(SCHEMA) {
        return Vec::new();
    }
    helix_settings::Settings::new(SCHEMA).strv("workspace-names").iter().map(|s| s.to_string()).collect()
}

pub(super) struct WorkspacePopup {
    layer: LayerSurface,
    width: u32,
    height: u32,
    configured: bool,
    /// The workspace switched to (shown before the compositor confirms).
    active: u32,
    until: Instant,
}

impl Ui {
    /// A key switched to workspace `target`: show (or update) the popup.
    pub(super) fn show_workspace_popup(&mut self, target: u32) {
        if self.windows.count < 2 {
            return;
        }
        let until = Instant::now() + SHOWN;
        if let Some(popup) = self.workspace_popup.as_mut() {
            popup.active = target;
            popup.until = until;
            self.draw_workspace_popup();
        } else {
            let (w, h) = self.workspace_popup_size();
            let surface = self.compositor.create_surface(&self.qh);
            let layer = self.layer_shell.create_layer_surface(&self.qh, surface, Layer::Overlay, Some("helix-workspace-switch"), None);
            layer.set_size(w, h);
            layer.set_keyboard_interactivity(KeyboardInteractivity::None);
            // Clicks go through it.
            if let Ok(region) = smithay_client_toolkit::compositor::Region::new(&self.compositor) {
                layer.wl_surface().set_input_region(Some(region.wl_region()));
            }
            layer.commit();
            self.workspace_popup = Some(WorkspacePopup { layer, width: 0, height: 0, configured: false, active: target, until });
        }
        if let Some(handle) = &self.loop_handle {
            let _ = handle.insert_source(Timer::from_duration(SHOWN), |_, _, ui| {
                if ui.workspace_popup.as_ref().is_some_and(|p| Instant::now() >= p.until) {
                    ui.workspace_popup = None;
                }
                TimeoutAction::Drop
            });
        }
    }

    fn workspace_popup_size(&self) -> (u32, u32) {
        let n = self.windows.count.min(36) as i32;
        let w = (2 * PAD + n * BOX_W + (n - 1) * GAP).max(220);
        (w as u32, (2 * PAD + BOX_H + NAME_H) as u32)
    }

    /// The compositor reported the workspaces.
    pub(super) fn workspaces_changed(&mut self) {
        let size = self.workspace_popup_size();
        let active = self.windows.active;
        if let Some(popup) = self.workspace_popup.as_mut() {
            popup.active = active;
            if (popup.width, popup.height) != size && popup.configured {
                popup.layer.set_size(size.0, size.1);
                popup.layer.commit();
            } else {
                self.draw_workspace_popup();
            }
        }
    }

    pub(super) fn workspace_popup_configured(&mut self, layer: &LayerSurface, size: (u32, u32)) -> bool {
        let Some(popup) = self.workspace_popup.as_mut().filter(|p| &p.layer == layer) else { return false };
        popup.width = size.0;
        popup.height = size.1;
        popup.configured = true;
        self.draw_workspace_popup();
        true
    }

    pub(super) fn workspace_popup_closed(&mut self, layer: &LayerSurface) -> bool {
        if self.workspace_popup.as_ref().is_some_and(|p| &p.layer == layer) {
            self.workspace_popup = None;
            return true;
        }
        false
    }

    pub(super) fn draw_workspace_popup(&mut self) {
        let Some(popup) = self.workspace_popup.take() else { return };
        if popup.configured && popup.width > 0 {
            let (w, h) = (popup.width as usize, popup.height as usize);
            let scale = self.scales.of(popup.layer.wl_surface());
            let names = read_names();
            if let Ok((buffer, data)) =
                self.buffers.create_buffer(popup.layer.wl_surface(), Ui::draw_workspace_popup, physical(w, scale) as i32, physical(h, scale) as i32, (physical(w, scale) * 4) as i32, wl_shm::Format::Argb8888)
            {
                let mut canvas = Canvas::scaled(data, w, h, scale);
                canvas.fill(0);
                theme::popover_round(&mut canvas, 1.0, 1.0, w as f64 - 2.0, h as f64 - 2.0, theme::RADIUS_POPOVER);
                let n = self.windows.count.min(36) as i32;
                let row = n * BOX_W + (n - 1) * GAP;
                let x0 = (w as i32 - row) / 2;
                for i in 0..n {
                    let x = x0 + i * (BOX_W + GAP);
                    theme::control(&mut canvas, x, PAD, BOX_W, BOX_H, theme::Look { on: i as u32 == popup.active, ..Default::default() });
                }
                let name = workspace_name(&names, popup.active);
                let text = self.text.as_ref();
                let nw = text.map_or(0.0, |t| t.width(&name, size::BODY)).min((w as i32 - 2 * PAD) as f32);
                let base = baseline(text, PAD + BOX_H + 4, NAME_H, size::BODY);
                label(text, &mut canvas, &name, (w as f32 - nw) / 2.0, base, size::BODY, FG, (w as i32 - 2 * PAD) as f32);
                let surface = popup.layer.wl_surface();
                self.scales.prepare(&self.qh, surface, w as i32, h as i32);
                if buffer.attach_to(surface).is_ok() {
                    popup.layer.commit();
                }
            }
        }
        self.workspace_popup = Some(popup);
    }

    /// The popup for the accessibility bus: its size and what it shows.
    pub(super) fn workspace_popup_a11y(&self) -> Option<(&LayerSurface, (u32, u32), Vec<crate::a11y::Node>)> {
        let popup = self.workspace_popup.as_ref().filter(|p| p.configured && p.width > 0)?;
        let (w, _) = (popup.width as i32, popup.height);
        let nodes = popup_nodes(&read_names(), self.windows.count.min(36), popup.active, w);
        Some((&popup.layer, (popup.width, popup.height), nodes))
    }
}

/// The workspace popup on the accessibility bus (helix-desktop-spec
/// `contracts/dbus/org.a11y.atspi.md`): the workspace switched to as a
/// live status (announced), and the boxes as a list of the workspaces,
/// the active one selected; `w` is the surface's width.
pub(super) fn popup_nodes(names: &[String], count: u32, active: u32, w: i32) -> Vec<crate::a11y::Node> {
    use crate::a11y::{Node, Role};
    let n = count as i32;
    let x0 = (w - (n * BOX_W + (n - 1) * GAP)) / 2;
    let boxes = (0..count).map(|i| {
        let r = crate::a11y::Rect::new(x0 + i as i32 * (BOX_W + GAP), PAD, BOX_W, BOX_H);
        Node::new(Role::ListItem, workspace_name(names, i)).id(format!("workspace:{i}")).at(r).selected(i == active)
    });
    let current = workspace_name(names, active);
    vec![Node::new(Role::Status, current).id("current").at(crate::a11y::Rect::new(PAD, PAD + BOX_H + 4, w - 2 * PAD, NAME_H)).live(), Node::new(Role::List, helix_i18n::tr("Workspaces")).id("workspaces").children(boxes)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        let names = vec!["Mail".to_owned(), "".to_owned(), "Code".to_owned()];
        assert_eq!(workspace_name(&names, 0), "Mail");
        assert_eq!(workspace_name(&names, 1), "Workspace 2", "empty entry");
        assert_eq!(workspace_name(&names, 3), "Workspace 4", "beyond the list");
    }
}
