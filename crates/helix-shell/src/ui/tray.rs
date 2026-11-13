//! The panel's status icons (`helix:tray`; helix-desktop-spec
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

    /// The icons to draw, at `scale`: (image, needs attention).
    pub(super) fn tray_images(&mut self, scale: f64) -> Vec<(Option<Image>, bool)> {
        let px = (f64::from(status::ICON) * scale).round() as u32;
        let shown: Vec<(crate::tray::Icon, bool)> = self.tray_shown().iter().map(|i| (i.icon_now().clone(), i.needs_attention())).collect();
        shown
            .into_iter()
            .map(|(icon, attention)| {
                let key = format!("tray:{}:{}:{}@{px}", icon.name, icon.theme_path, icon.pixmaps.iter().map(|p| p.2.len()).sum::<usize>());
                let rgb = [(FG >> 16) as u8, (FG >> 8) as u8, FG as u8];
                let image = if icon.pixmaps.is_empty() {
                    self.icon_cache.entry(key).or_insert_with(|| tray_icon(&icon, px, rgb)).clone()
                } else {
                    // Pixmaps can change without their name changing.
                    tray_icon(&icon, px, rgb)
                };
                (image, attention)
            })
            .collect()
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
        let _ = self.requests.send(UiRequest::Tray(Request { key, action, x, y: PANEL_HEIGHT as i32 }));
    }

    /// Pointer input on the panel's status icons; true if it was for them.
    pub(super) fn tray_pointer(&mut self, event: &PointerEvent) -> bool {
        if &event.surface != self.panel.wl_surface() || self.lock.session.is_some() {
            return false;
        }
        let Some(&(_, tray_x, tray_w)) = self.panel_placed.iter().find(|p| p.0 == crate::panel_layout::Slot::Item(crate::panel_layout::Item::Tray)) else { return false };
        let x = event.position.0;
        if x < f64::from(tray_x - TRAY_GAP / 2) || x >= f64::from(tray_x + tray_w + TRAY_GAP / 2) {
            return false;
        }
        let Some((key, ix)) = self.tray_at(x, tray_x) else { return false };
        match event.kind {
            PointerEventKind::Press { button, .. } => {
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
        }
    }

    /// Show status icon `key`'s menu below it.
    pub(super) fn show_tray_menu(&mut self, key: String, x: i32, entries: Vec<MenuEntry>) {
        let mut ids = Vec::new();
        let items = menu_items(&entries, &mut ids);
        self.open_menu(0, x, PANEL_HEIGHT as i32, items, Vec::new());
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

/// A status icon's picture at `px`.
fn tray_icon(icon: &crate::tray::Icon, px: u32, rgb: [u8; 3]) -> Option<Image> {
    if !icon.name.is_empty() {
        let from_dir = (!icon.theme_path.is_empty()).then(|| find_in_dir(std::path::Path::new(&icon.theme_path), &icon.name, 4)).flatten();
        let found = match from_dir {
            Some(path) => crate::icons::decode(&std::fs::read(path).ok()?, px),
            None if icon.name.ends_with("-symbolic") => crate::icons::symbolic(&icon.name, px, rgb),
            None => crate::icons::load(&crate::icons::IconSource::Names(vec![icon.name.clone()]), px),
        };
        if found.is_some() {
            return found;
        }
    }
    let (w, h, rgba) = crate::tray::best_pixmap(&icon.pixmaps, px)?;
    Some(Image { width: *w as usize, height: *h as usize, rgba: rgba.clone() })
}

/// `name.png` or `name.svg` in `dir` or (up to `depth` levels) below it.
fn find_in_dir(dir: &std::path::Path, name: &str, depth: usize) -> Option<std::path::PathBuf> {
    for ext in ["png", "svg"] {
        let path = dir.join(format!("{name}.{ext}"));
        if path.is_file() {
            return Some(path);
        }
    }
    if depth == 0 {
        return None;
    }
    let mut dirs: Vec<_> = std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    dirs.sort();
    dirs.iter().find_map(|d| find_in_dir(d, name, depth - 1))
}
