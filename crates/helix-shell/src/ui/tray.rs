//! The panel's status icons (`helix:tray`; helixdesktop/spec
//! `contracts/dbus/org.kde.StatusNotifierItem.md`): drawing them, clicks
//! and scrolls, and their menus.

use super::window_menu::{Choice, Item as MenuItem};
use super::*;
use crate::tray::{Action, MenuEntry, Request};
use helix_draw::Image;

const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

/// Space between status icons.
pub(super) const TRAY_GAP: i32 = 8;

impl Ui {
    /// The status icons shown, in order.
    pub(super) fn tray_shown(&self) -> Vec<&crate::tray::Item> {
        self.tray.iter().filter(|i| i.shown()).collect()
    }

    pub(super) fn tray_width(&self) -> i32 {
        let n = self.tray_shown().len() as i32;
        if n == 0 {
            0
        } else {
            n * status::ICON + (n - 1) * TRAY_GAP
        }
    }

    /// The icons to draw, at `scale`: (image, overlay, needs attention).
    pub(super) fn tray_images(&mut self, scale: f64) -> Vec<(Option<Image>, Option<Image>, bool)> {
        let px = (f64::from(status::ICON) * scale).round() as u32;
        let shown: Vec<(crate::tray::Icon, crate::tray::Icon, bool)> = self.tray_shown().iter().map(|i| (i.icon_now().clone(), i.overlay_icon.clone(), i.needs_attention())).collect();
        shown
            .into_iter()
            .map(|(icon, overlay, attention)| {
                let image = self.tray_image(&icon, px);
                let overlay = if overlay.is_empty() { None } else { self.tray_image(&overlay, px / 2) };
                (image, overlay, attention)
            })
            .collect()
    }

    fn tray_image(&mut self, icon: &crate::tray::Icon, px: u32) -> Option<Image> {
        let rgb = [(FG >> 16) as u8, (FG >> 8) as u8, FG as u8];
        // The named icon (decoded on a worker), else the pixmap (which can
        // change without the name changing: not cached).
        let named = (!icon.name.is_empty()).then(|| {
            let key = format!("tray:{}:{}@{px}", icon.name, icon.theme_path);
            let (name, theme_path) = (icon.name.clone(), icon.theme_path.clone());
            self.icon_cache.get(key, move || tray_named(&name, &theme_path, px, rgb))
        });
        // A pixmap at least twice the size, shrunk.
        named.flatten().or_else(|| {
            let (w, h, rgba) = crate::tray::best_pixmap(&icon.pixmaps, px.saturating_mul(2))?;
            Some(crate::icons::shrink(Image { width: *w as usize, height: *h as usize, rgba: rgba.clone() }, px))
        })
    }

    /// The status icon at panel `x` within the tray drawn at `tray_x`:
    /// its key and left edge.
    pub(super) fn tray_at(&self, x: f64, tray_x: i32) -> Option<(String, i32)> {
        self.tray_shown().iter().enumerate().find_map(|(i, item)| {
            let ix = tray_x + i as i32 * (status::ICON + TRAY_GAP);
            (x >= f64::from(ix - TRAY_GAP / 2) && x < f64::from(ix + status::ICON + TRAY_GAP / 2)).then(|| (item.key.clone(), ix))
        })
    }

    pub(super) fn tray_request(&mut self, key: String, action: Action, x: i32) {
        crate::click_feedback();
        let _ = self.requests.send(UiRequest::Tray(Request { key, action, x, y: panel_height() as i32 }));
    }

    /// Pointer input on the panel's status icons; true if it was for them.
    pub(super) fn tray_pointer(&mut self, event: &PointerEvent) -> bool {
        if &event.surface != self.panel.wl_surface() || self.lock.session.is_some() {
            return false;
        }
        if let PointerEventKind::Leave { .. } = event.kind {
            self.panel_hover(None, 0);
            return false;
        }
        let under = self
            .panel_placed
            .iter()
            .find(|p| p.0 == crate::panel_layout::Slot::Item(crate::panel_layout::Item::Tray))
            .and_then(|&(_, tray_x, tray_w)| {
                let x = event.position.0;
                (x >= f64::from(tray_x - TRAY_GAP / 2) && x < f64::from(tray_x + tray_w + TRAY_GAP / 2)).then(|| self.tray_at(x, tray_x)).flatten()
            });
        let Some((key, ix)) = under else {
            self.panel_hover(None, 0);
            return false;
        };
        match event.kind {
            PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                self.panel_hover(Some(key), ix);
                true
            }
            PointerEventKind::Press { button, .. } => {
                // A click takes the tooltip away until the pointer moves on.
                self.panel_tooltip = None;
                self.panel_hover.serial += 1;
                let action = match button {
                    BTN_RIGHT => Action::Secondary,
                    BTN_MIDDLE => Action::Middle,
                    _ => Action::Primary,
                };
                self.tray_request(key, action, ix);
                true
            }
            PointerEventKind::Axis { vertical, horizontal, .. } => {
                let (steps, horizontal) = if vertical.discrete != 0 {
                    (vertical.discrete, false)
                } else if horizontal.discrete != 0 {
                    (horizontal.discrete, true)
                } else if vertical.absolute != 0.0 {
                    (vertical.absolute.signum() as i32, false)
                } else if horizontal.absolute != 0.0 {
                    (horizontal.absolute.signum() as i32, true)
                } else {
                    return true;
                };
                self.tray_request(key, Action::Scroll(steps, horizontal), ix);
                true
            }
            _ => true,
        }
    }

    /// The status icons changed.
    pub(super) fn tray_changed(&mut self, items: Vec<crate::tray::Item>) {
        if items != self.tray {
            self.tray = items;
            self.draw_panel();
            // The tooltip shown follows its item (`NewToolTip`), and goes
            // with it.
            if self.panel_hover.key.as_ref().is_some_and(|k| !self.tray.iter().any(|i| &i.key == k)) {
                self.panel_hover(None, 0);
            } else if self.panel_tooltip.is_some() {
                self.show_panel_tooltip();
            }
        }
    }

    /// Show status icon `key`'s menu below it.
    pub(super) fn show_tray_menu(&mut self, key: String, x: i32, entries: Vec<MenuEntry>) {
        let mut ids = Vec::new();
        let items = menu_items(&entries, &mut ids);
        self.open_menu(0, x, panel_height() as i32, items, Vec::new());
        self.tray_menu = Some((key, ids));
    }

    /// Entry `index` of the open status icon menu was chosen.
    pub(super) fn tray_menu_choice(&mut self, index: usize) {
        let Some((key, ids)) = self.tray_menu.take() else { return };
        if let Some(&id) = ids.get(index) {
            let x = self.panel_placed.iter().find(|p| p.0 == crate::panel_layout::Slot::Item(crate::panel_layout::Item::Tray)).map_or(0, |p| p.1);
            self.tray_request(key, Action::MenuClicked(id), x);
        }
    }
}

/// Menu entries as the panel's menu items; `ids` collects the entries'
/// ids, which `Choice::Tray` indexes.
fn menu_items(entries: &[MenuEntry], ids: &mut Vec<i32>) -> Vec<Option<MenuItem>> {
    entries
        .iter()
        .map(|entry| match entry {
            MenuEntry::Separator => None,
            MenuEntry::Item { id, label, enabled, check, radio_on, children } => {
                let sub = menu_items(children, ids);
                let choice = if !children.is_empty() {
                    Choice::Submenu
                } else if !enabled {
                    Choice::Disabled
                } else {
                    ids.push(*id);
                    Choice::Tray(ids.len() - 1)
                };
                Some(MenuItem { label: label.clone(), choice, checked: *check, active: *radio_on, sub })
            }
        })
        .collect()
}

/// A status icon's named picture at `px`: its symbolic form, else a
/// larger image shrunk (see [`crate::icons::status_icon`]), from the
/// item's own theme directory or the icon theme.
fn tray_named(name: &str, theme_path: &str, px: u32, rgb: [u8; 3]) -> Option<Image> {
    let dir = (!theme_path.is_empty()).then(|| std::path::Path::new(theme_path));
    crate::icons::status_icon(name, dir, px, rgb)
}
