//! The panel's system status icons (helix-desktop-spec
//! `contracts/wayland/README.md`, "System status").

use helix_draw::Image;

use super::{Ui, FG};

/// Status icons are this large (logical pixels).
pub(super) const ICON: i32 = 16;

/// What the status area shows, right to left.
pub(super) enum StatusItem {
    Icon(Image),
    Text(String),
}

impl Ui {
    /// A symbolic icon in the panel's text color, cached.
    pub(super) fn symbolic_icon(&mut self, name: &str, scale: f64) -> Option<Image> {
        let px = (f64::from(ICON) * scale).round() as u32;
        let key = format!("sym:{name}@{px}");
        let rgb = [(FG >> 16) as u8, (FG >> 8) as u8, FG as u8];
        let name = name.to_owned();
        self.icon_cache.get(key, move || crate::icons::symbolic(&name, px, rgb))
    }

    /// A symbolic icon `logical` px large in the panel's text color,
    /// cached.
    pub(super) fn symbolic_icon_px(&mut self, name: &str, logical: f64, scale: f64) -> Option<Image> {
        let px = (logical * scale).round() as u32;
        let key = format!("sym:{name}@{px}");
        let rgb = [(FG >> 16) as u8, (FG >> 8) as u8, FG as u8];
        let name = name.to_owned();
        self.icon_cache.get(key, move || crate::icons::symbolic(&name, px, rgb))
    }

    /// A symbolic icon `logical` px large in `color`, cached.
    pub(super) fn symbolic_icon_color(&mut self, name: &str, logical: f64, scale: f64, color: u32) -> Option<Image> {
        let px = (logical * scale).round() as u32;
        let key = format!("sym:{name}@{px}#{color:06x}");
        let rgb = [(color >> 16) as u8, (color >> 8) as u8, color as u8];
        let name = name.to_owned();
        self.icon_cache.get(key, move || crate::icons::symbolic(&name, px, rgb))
    }

    /// The icon of the app with desktop file id `id`, `logical` px large,
    /// cached.
    pub(super) fn app_icon_px(&mut self, id: &str, logical: f64, scale: f64) -> Option<Image> {
        if id.is_empty() {
            return None;
        }
        let px = (logical * scale).round() as u32;
        let key = format!("app:{id}@{px}");
        let id = id.to_owned();
        self.icon_cache.get(key, move || crate::icons::app_icon(&id, px))
    }

    /// The status area's items, right to left.
    pub(super) fn status_items(&mut self, scale: f64) -> Vec<StatusItem> {
        let status = self.status.clone();
        let mut out = Vec::new();
        if let Some((icon, percent)) = &status.battery {
            if status.show_percentage {
                out.push(StatusItem::Text(format!("{}%", percent.round() as i64)));
            }
            out.extend(self.symbolic_icon(icon, scale).map(StatusItem::Icon));
        }
        if let Some(icon) = &status.volume {
            out.extend(self.symbolic_icon(icon, scale).map(StatusItem::Icon));
        }
        if let Some(icon) = &status.network {
            out.extend(self.symbolic_icon(icon, scale).map(StatusItem::Icon));
        }
        for (on, icon) in [
            (status.location, "location-services-active-symbolic"),
            (status.microphone, "audio-input-microphone-symbolic"),
            (status.camera, "camera-web-symbolic"),
        ] {
            if on {
                out.extend(self.symbolic_icon(icon, scale).map(StatusItem::Icon));
            }
        }
        out
    }
}
