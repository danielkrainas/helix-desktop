//! The workspace switch popup (helix-desktop-spec
//! `contracts/gsettings/workspaces.md`, "Indicator"): shown for a moment
//! when a key switches workspaces.

use super::*;

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
        .unwrap_or_else(|| format!("Workspace {}", n + 1))
}

fn read_names() -> Vec<String> {
    use gio::prelude::*;
    const SCHEMA: &str = "org.gnome.desktop.wm.preferences";
    if !helix_settings::has_schema(SCHEMA) {
        return Vec::new();
    }
    gio::Settings::new(SCHEMA).strv("workspace-names").iter().map(|s| s.to_string()).collect()
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
                self.pool.create_buffer(physical(w, scale) as i32, physical(h, scale) as i32, (physical(w, scale) * 4) as i32, wl_shm::Format::Argb8888)
            {
                let mut canvas = Canvas::scaled(data, w, h, scale);
                canvas.fill(BANNER_BORDER);
                canvas.rect(1, 1, w as i32 - 2, h as i32 - 2, BOX_BG);
                let n = self.windows.count.min(36) as i32;
                let row = n * BOX_W + (n - 1) * GAP;
                let x0 = (w as i32 - row) / 2;
                for i in 0..n {
                    let x = x0 + i * (BOX_W + GAP);
                    let color = if i as u32 == popup.active { ACCENT } else { FIELD_BG };
                    canvas.rect(x, PAD, BOX_W, BOX_H, color);
                }
                let name = workspace_name(&names, popup.active);
                let text = self.text.as_ref();
                let nw = text.map_or(0.0, |t| t.width(&name, 15.0)).min((w as i32 - 2 * PAD) as f32);
                let base = baseline(text, PAD + BOX_H + 4, NAME_H, 15.0);
                label(text, &mut canvas, &name, (w as f32 - nw) / 2.0, base, 15.0, FG, (w as i32 - 2 * PAD) as f32);
                let surface = popup.layer.wl_surface();
                self.scales.prepare(&self.qh, surface, w as i32, h as i32);
                if buffer.attach_to(surface).is_ok() {
                    popup.layer.commit();
                }
            }
        }
        self.workspace_popup = Some(popup);
    }
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
