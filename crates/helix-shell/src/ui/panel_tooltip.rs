//! Tooltips for panel items (helix-desktop-spec
//! `contracts/dbus/org.kde.StatusNotifierItem.md`, "Tooltip"): a box
//! below the panel, under the item, shown after the pointer rests on it.
//! Pointer input passes through it.

use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};

use super::*;

/// How long the pointer rests on an item before its tooltip shows.
const DELAY: Duration = Duration::from_millis(600);
const PAD: i32 = 10;
const LINE_H: i32 = 20;
const MAX_W: i32 = 420;

pub(super) struct PanelTooltip {
    layer: LayerSurface,
    width: u32,
    height: u32,
    configured: bool,
    lines: Vec<String>,
}

/// What the pointer rests on: an item's key, where it is drawn, and
/// when that started.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct Hover {
    pub key: Option<String>,
    pub x: i32,
    pub serial: u64,
}

impl Ui {
    /// The pointer now rests on item `key` (drawn from `x`), or on none.
    pub(super) fn panel_hover(&mut self, key: Option<String>, x: i32) {
        if self.panel_hover.key == key {
            return;
        }
        self.panel_hover.serial += 1;
        self.panel_hover.key = key.clone();
        self.panel_hover.x = x;
        self.panel_tooltip = None;
        if key.is_none() {
            return;
        }
        let serial = self.panel_hover.serial;
        if let Some(handle) = &self.loop_handle {
            let _ = handle.insert_source(Timer::from_duration(DELAY), move |_, _, ui: &mut Ui| {
                if ui.panel_hover.serial == serial {
                    ui.show_panel_tooltip();
                }
                TimeoutAction::Drop
            });
        }
    }

    /// The lines for what the pointer rests on now.
    fn hover_lines(&self) -> Vec<String> {
        let Some(key) = &self.panel_hover.key else { return Vec::new() };
        self.tray.iter().find(|i| &i.key == key).map(|i| i.tooltip_lines()).or_else(|| self.ext_tooltip_lines(key)).unwrap_or_default()
    }

    fn tooltip_size(&self, lines: &[String]) -> (u32, u32) {
        let (text, bold) = (self.text.as_ref(), self.bold.as_ref());
        // The first line is bold; a little slack so nothing is cut short.
        let widest = lines
            .iter()
            .enumerate()
            .map(|(i, l)| if i == 0 { bold.or(text) } else { text }.map_or(0.0, |t| t.width(l, 14.0)).ceil() as i32 + 4)
            .max()
            .unwrap_or(0);
        ((widest + 2 * PAD).min(MAX_W) as u32, (lines.len() as i32 * LINE_H + 2 * PAD) as u32)
    }

    pub(super) fn show_panel_tooltip(&mut self) {
        let lines = self.hover_lines();
        if lines.is_empty() {
            self.panel_tooltip = None;
            return;
        }
        let (w, h) = self.tooltip_size(&lines);
        if let Some(tip) = self.panel_tooltip.as_mut() {
            if tip.lines == lines {
                return;
            }
            tip.lines = lines;
            if (tip.width, tip.height) != (w, h) {
                tip.layer.set_size(w, h);
                tip.layer.commit();
            } else {
                self.draw_panel_tooltip();
            }
            return;
        }
        let surface = self.compositor.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(&self.qh, surface, Layer::Overlay, Some("helix-panel-tooltip"), None);
        layer.set_anchor(Anchor::TOP | Anchor::LEFT);
        // Under the item, kept on the panel's monitor.
        let left = (self.panel_hover.x - w as i32 / 2 + status::ICON / 2).clamp(4, (self.panel_width as i32 - w as i32 - 4).max(4));
        layer.set_margin(4, 0, 0, left);
        layer.set_size(w, h);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        if let Ok(region) = smithay_client_toolkit::compositor::Region::new(&self.compositor) {
            layer.wl_surface().set_input_region(Some(region.wl_region()));
        }
        layer.commit();
        self.panel_tooltip = Some(PanelTooltip { layer, width: 0, height: 0, configured: false, lines });
    }

    pub(super) fn panel_tooltip_configured(&mut self, layer: &LayerSurface, size: (u32, u32)) -> bool {
        let Some(tip) = self.panel_tooltip.as_mut().filter(|t| &t.layer == layer) else { return false };
        tip.width = size.0;
        tip.height = size.1;
        tip.configured = true;
        self.draw_panel_tooltip();
        true
    }

    pub(super) fn panel_tooltip_closed(&mut self, layer: &LayerSurface) -> bool {
        if self.panel_tooltip.as_ref().is_some_and(|t| &t.layer == layer) {
            self.panel_tooltip = None;
            return true;
        }
        false
    }

    fn draw_panel_tooltip(&mut self) {
        let Some(tip) = self.panel_tooltip.take() else { return };
        if tip.configured && tip.width > 0 {
            let (w, h) = (tip.width as usize, tip.height as usize);
            let scale = self.scales.of(tip.layer.wl_surface());
            if let Ok((buffer, data)) =
                self.pool.create_buffer(physical(w, scale) as i32, physical(h, scale) as i32, (physical(w, scale) * 4) as i32, wl_shm::Format::Argb8888)
            {
                let mut canvas = Canvas::scaled(data, w, h, scale);
                canvas.fill(BANNER_BORDER);
                canvas.rect(1, 1, w as i32 - 2, h as i32 - 2, BANNER_BG);
                let (text, bold) = (self.text.as_ref(), self.bold.as_ref());
                for (i, line) in tip.lines.iter().enumerate() {
                    let font = if i == 0 { bold.or(text) } else { text };
                    let color = if i == 0 { FG } else { DIM_FG };
                    let base = baseline(font, PAD + i as i32 * LINE_H, LINE_H, 14.0);
                    label(font, &mut canvas, line, PAD as f32, base, 14.0, color, (w as i32 - 2 * PAD) as f32);
                }
                let surface = tip.layer.wl_surface();
                self.scales.prepare(&self.qh, surface, w as i32, h as i32);
                if buffer.attach_to(surface).is_ok() {
                    tip.layer.commit();
                }
            }
        }
        self.panel_tooltip = Some(tip);
    }
}
