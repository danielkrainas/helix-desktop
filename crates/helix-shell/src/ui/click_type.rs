//! The dwell click type window (helix-desktop-spec
//! `contracts/gsettings/a11y-mouse.md`, "Click type window"): four
//! buttons along the right edge choosing what the next dwell does.

use super::*;

const BUTTON_W: i32 = 96;
const BUTTON_H: i32 = 44;
const GAP: i32 = 6;
pub(super) const NAMESPACE: &str = "helix-click-type";

/// The types, as the compositor names them, and their labels.
pub(super) const TYPES: [(&str, &str); 4] = [("single", "Single"), ("double", "Double"), ("drag", "Drag"), ("secondary", "Secondary")];

pub(super) struct ClickTypeWindow {
    layer: LayerSurface,
    width: u32,
    height: u32,
    configured: bool,
    hits: Vec<(Rect, usize)>,
}

/// What the window keeps between showings.
#[derive(Default)]
pub(super) struct ClickTypeState {
    pub view: Option<ClickTypeWindow>,
    /// Whether the settings ask for it.
    pub wanted: bool,
    /// The current type (`single`…).
    pub current: String,
}

fn size() -> (u32, u32) {
    (BUTTON_W as u32 + 2 * GAP as u32, (TYPES.len() as i32 * (BUTTON_H + GAP) + GAP) as u32)
}

impl Ui {
    /// Show or hide the window as the settings say (`wanted`).
    pub(super) fn click_type_sync(&mut self) {
        match (self.click_type.wanted, self.click_type.view.is_some()) {
            (true, false) => {
                let surface = self.compositor.create_surface(&self.qh);
                let layer = self.layer_shell.create_layer_surface(&self.qh, surface, Layer::Overlay, Some(NAMESPACE.to_owned()), None);
                layer.set_anchor(Anchor::RIGHT);
                layer.set_margin(0, 8, 0, 0);
                let (w, h) = size();
                layer.set_size(w, h);
                layer.set_keyboard_interactivity(KeyboardInteractivity::None);
                layer.commit();
                self.click_type.view = Some(ClickTypeWindow { layer, width: 0, height: 0, configured: false, hits: Vec::new() });
            }
            (false, true) => self.click_type.view = None,
            _ => {}
        }
    }

    pub(super) fn click_type_configured(&mut self, layer: &LayerSurface, size: (u32, u32)) -> bool {
        let Some(view) = self.click_type.view.as_mut().filter(|v| &v.layer == layer) else { return false };
        let (w, h) = self::size();
        view.width = if size.0 > 0 { size.0 } else { w };
        view.height = if size.1 > 0 { size.1 } else { h };
        view.configured = true;
        self.draw_click_type();
        true
    }

    pub(super) fn click_type_closed(&mut self, layer: &LayerSurface) -> bool {
        if self.click_type.view.as_ref().is_some_and(|v| &v.layer == layer) {
            self.click_type.view = None;
            return true;
        }
        false
    }

    pub(super) fn draw_click_type(&mut self) {
        let Some(mut view) = self.click_type.view.take() else { return };
        if view.configured {
            let (w, h) = (view.width as usize, view.height as usize);
            let scale = self.scales.of(view.layer.wl_surface());
            if let Ok((buffer, data)) = self.pool.create_buffer(physical(w, scale) as i32, physical(h, scale) as i32, (physical(w, scale) * 4) as i32, wl_shm::Format::Argb8888) {
                let mut canvas = Canvas::scaled(data, w, h, scale);
                canvas.fill(0x00000000);
                canvas.rect(0, 0, w as i32, h as i32, BANNER_BORDER);
                canvas.rect(1, 1, w as i32 - 2, h as i32 - 2, BANNER_BG);
                let text = self.text.as_ref();
                view.hits.clear();
                for (i, (name, label_text)) in TYPES.iter().enumerate() {
                    let r = Rect { x: GAP, y: GAP + i as i32 * (BUTTON_H + GAP), w: BUTTON_W, h: BUTTON_H };
                    let current = self.click_type.current == *name;
                    canvas.rect(r.x, r.y, r.w, r.h, if current { ACCENT } else { FIELD_BG });
                    let tw = text.map_or(0.0, |t| t.width(label_text, 14.0));
                    label(text, &mut canvas, label_text, r.x as f32 + (r.w as f32 - tw) / 2.0, baseline(text, r.y, r.h, 14.0), 14.0, FG, r.w as f32);
                    view.hits.push((r, i));
                }
                let surface = view.layer.wl_surface();
                self.scales.prepare(&self.qh, surface, w as i32, h as i32);
                if buffer.attach_to(surface).is_ok() {
                    view.layer.commit();
                }
            }
        }
        self.click_type.view = Some(view);
    }

    /// Pointer input on the window; true if it was for it.
    pub(super) fn click_type_pointer(&mut self, event: &PointerEvent) -> bool {
        let Some(view) = self.click_type.view.as_ref().filter(|v| v.layer.wl_surface() == &event.surface) else { return false };
        let PointerEventKind::Press { button: BTN_LEFT, .. } = event.kind else { return true };
        let (x, y) = event.position;
        let Some(&(_, i)) = view.hits.iter().find(|(r, _)| r.contains(x, y)) else { return true };
        let name = TYPES[i].0.to_owned();
        tracing::debug!(name, "click type chosen");
        self.click_type.current = name.clone();
        self.draw_click_type();
        let _ = self.requests.send(UiRequest::SetClickType(name));
        true
    }
}
