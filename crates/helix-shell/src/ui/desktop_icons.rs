//! Desktop icons (helix-desktop-spec `contracts/gsettings/background.md`,
//! "Desktop icons"): a see-through surface just above the primary
//! monitor's background, with the desktop folder's files.

use std::collections::HashMap;
use std::path::PathBuf;

use super::*;
use crate::desktop_icons::{self as logic, DesktopItem};

/// Under windows, above the background; named so the compositor does not
/// fade it like other shell surfaces.
pub(super) const NAMESPACE: &str = "helix-background-icons";
const LABEL_PX: f32 = 13.0;
const SELECTED: u32 = 0x553584E4;

pub(super) struct IconsView {
    layer: LayerSurface,
    output: wl_output::WlOutput,
    width: u32,
    height: u32,
    configured: bool,
}

#[derive(Default)]
pub(super) struct DesktopIconsState {
    /// The items, when shown.
    pub items: Option<Vec<DesktopItem>>,
    view: Option<IconsView>,
    selected: Option<PathBuf>,
    /// The last press: on which item, when (ms).
    last_press: Option<(PathBuf, u32)>,
    icons: HashMap<PathBuf, Option<helix_draw::Image>>,
}

impl Ui {
    /// The primary monitor's output.
    fn primary_output(&self) -> Option<wl_output::WlOutput> {
        self.output_state.outputs().find(|o| self.output_state.info(o).and_then(|i| i.logical_position).is_some_and(|p| p == (self.primary_area.0, self.primary_area.1)))
    }

    /// Show, move or take down the surface as the items and monitors say.
    pub(super) fn desktop_icons_sync(&mut self) {
        let output = self.desktop_icons.items.as_ref().and(self.primary_output());
        if self.desktop_icons.view.as_ref().map(|v| &v.output) == output.as_ref() {
            self.draw_desktop_icons();
            return;
        }
        self.desktop_icons.view = None;
        let Some(output) = output else { return };
        let surface = self.compositor.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(&self.qh, surface, Layer::Bottom, Some(NAMESPACE.to_owned()), Some(&output));
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        // Kept clear of the panel (and a fixed dock).
        layer.set_exclusive_zone(0);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_size(0, 0);
        layer.commit();
        self.desktop_icons.view = Some(IconsView { layer, output, width: 0, height: 0, configured: false });
    }

    /// New items (`None`: no icons).
    pub(super) fn desktop_icons_set(&mut self, items: Option<Vec<DesktopItem>>) {
        if let Some(items) = items.as_ref() {
            self.desktop_icons.icons.retain(|p, _| items.iter().any(|i| &i.path == p));
            if self.desktop_icons.selected.as_ref().is_some_and(|s| !items.iter().any(|i| &i.path == s)) {
                self.desktop_icons.selected = None;
            }
        }
        self.desktop_icons.items = items;
        self.desktop_icons_sync();
    }

    pub(super) fn desktop_icons_configured(&mut self, layer: &LayerSurface, size: (u32, u32)) -> bool {
        let Some(view) = self.desktop_icons.view.as_mut().filter(|v| &v.layer == layer) else { return false };
        view.width = size.0;
        view.height = size.1;
        view.configured = true;
        self.draw_desktop_icons();
        true
    }

    pub(super) fn desktop_icons_closed(&mut self, layer: &LayerSurface) -> bool {
        if self.desktop_icons.view.as_ref().is_some_and(|v| &v.layer == layer) {
            self.desktop_icons.view = None;
            return true;
        }
        false
    }

    fn desktop_icon(&mut self, item: &DesktopItem, px: u32) -> Option<helix_draw::Image> {
        self.desktop_icons.icons.entry(item.path.clone()).or_insert_with(|| crate::icons::load(&item.icon, px)).clone()
    }

    fn draw_desktop_icons(&mut self) {
        let Some(view) = self.desktop_icons.view.as_ref().filter(|v| v.configured && v.width > 0 && v.height > 0) else { return };
        let (w, h) = (view.width as usize, view.height as usize);
        let scale = self.scales.of(view.layer.wl_surface());
        let items = self.desktop_icons.items.clone().unwrap_or_default();
        let cells = logic::cells(items.len(), w as i32, h as i32);
        let px = (f64::from(logic::ICON) * scale).round() as u32;
        let images: Vec<Option<helix_draw::Image>> = items.iter().take(cells.len()).map(|i| self.desktop_icon(i, px)).collect();
        let Some(view) = self.desktop_icons.view.as_ref() else { return };
        let Ok((buffer, data)) = self.pool.create_buffer(physical(w, scale) as i32, physical(h, scale) as i32, (physical(w, scale) * 4) as i32, wl_shm::Format::Argb8888) else { return };
        let mut canvas = Canvas::scaled(data, w, h, scale);
        canvas.fill(0x00000000);
        let text = self.text.as_ref();
        for ((item, &(x, y)), image) in items.iter().zip(&cells).zip(&images) {
            if self.desktop_icons.selected.as_ref() == Some(&item.path) {
                canvas.round_rect(f64::from(x + 2), f64::from(y + 2), f64::from(logic::CELL_W - 4), f64::from(logic::CELL_H - 4), 8.0, SELECTED);
            }
            let ix = x + (logic::CELL_W - logic::ICON) / 2;
            if let Some(image) = image {
                canvas.image(f64::from(ix), f64::from(y + 6), f64::from(logic::ICON), f64::from(logic::ICON), image);
            }
            let max_w = (logic::CELL_W - 8) as f32;
            let lines = text.map_or_else(|| vec![item.name.clone()], |t| t.wrap(&item.name, LABEL_PX, max_w, 2));
            for (n, line) in lines.iter().enumerate() {
                let tw = text.map_or(0.0, |t| t.width(line, LABEL_PX)).min(max_w);
                let lx = x as f32 + (logic::CELL_W as f32 - tw) / 2.0;
                let base = (y + 6 + logic::ICON + 16) as f32 + n as f32 * (LABEL_PX + 3.0);
                // A shadow keeps the name readable on any picture.
                label(text, &mut canvas, line, lx + 1.0, base + 1.0, LABEL_PX, 0xC0000000, max_w);
                label(text, &mut canvas, line, lx, base, LABEL_PX, 0xFFFFFFFF, max_w);
            }
        }
        let surface = view.layer.wl_surface();
        self.scales.prepare(&self.qh, surface, w as i32, h as i32);
        if buffer.attach_to(surface).is_ok() {
            surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
            view.layer.commit();
        }
    }

    /// Pointer input on the surface; true if it was for it. A press
    /// selects the item under it (or nothing); a second press on it within
    /// the double-click time opens it.
    pub(super) fn desktop_icons_pointer(&mut self, event: &PointerEvent) -> bool {
        let Some(view) = self.desktop_icons.view.as_ref().filter(|v| v.layer.wl_surface() == &event.surface) else { return false };
        let PointerEventKind::Press { button: BTN_LEFT, time, .. } = event.kind else { return true };
        let items = self.desktop_icons.items.clone().unwrap_or_default();
        let cells = logic::cells(items.len(), view.width as i32, view.height as i32);
        let (x, y) = event.position;
        let hit = cells.iter().position(|&(cx, cy)| Rect { x: cx, y: cy, w: logic::CELL_W, h: logic::CELL_H }.contains(x, y)).map(|i| items[i].clone());
        let double = match (&hit, &self.desktop_icons.last_press) {
            (Some(item), Some((path, at))) => &item.path == path && time.wrapping_sub(*at) <= logic::DOUBLE_CLICK_MS,
            _ => false,
        };
        self.desktop_icons.last_press = hit.as_ref().map(|i| (i.path.clone(), time));
        self.desktop_icons.selected = hit.as_ref().map(|i| i.path.clone());
        self.draw_desktop_icons();
        if let (true, Some(item)) = (double, hit) {
            self.desktop_icons.last_press = None;
            logic::open(&item);
        }
        true
    }
}
