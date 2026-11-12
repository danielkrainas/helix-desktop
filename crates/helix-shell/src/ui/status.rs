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
    fn symbolic_icon(&mut self, name: &str, scale: f64) -> Option<Image> {
        let px = (f64::from(ICON) * scale).round() as u32;
        let key = format!("sym:{name}@{px}");
        let rgb = [(FG >> 16) as u8, (FG >> 8) as u8, FG as u8];
        self.icon_cache.entry(key).or_insert_with(|| crate::icons::symbolic(name, px, rgb)).clone()
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
        out
    }
}
