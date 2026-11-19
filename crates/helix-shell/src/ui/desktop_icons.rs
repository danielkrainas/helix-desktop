//! Desktop icons (helixdesktop/spec `contracts/gsettings/background.md`,
//! "Desktop icons"): a see-through surface just above the primary
//! monitor's background, with the desktop folder's files, and their menu.

use std::path::PathBuf;

use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};

use super::window_menu::{Choice, Item};
use super::*;
use crate::desktop_icons::{self as logic, DesktopItem, Launching};
use helix_i18n::tr;

const BTN_RIGHT: u32 = 0x111;

/// What a desktop icon menu entry does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum IconAction {
    Open,
    AllowLaunching,
    DisallowLaunching,
}

/// The menu of a file whose launching can be changed as `launching`
/// says (`None`: it cannot), as Desktop Icons NG's: "Open", then "Allow
/// Launching" or "Don't Allow Launching".
pub(super) fn menu_items(launching: Option<Launching>) -> Vec<Option<Item>> {
    let item = |label: &str, action| Some(Item { label: label.to_owned(), choice: Choice::DesktopIcon(action), checked: None, active: false, sub: Vec::new() });
    let mut out = vec![item(tr("Open"), IconAction::Open)];
    match launching {
        Some(Launching::Allow) => out.extend([None, item(tr("Allow Launching"), IconAction::AllowLaunching)]),
        Some(Launching::Disallow) => out.extend([None, item(tr("Don't Allow Launching"), IconAction::DisallowLaunching)]),
        None => {}
    }
    out
}

/// A primary-button or touch press on an icon that may become a long
/// press.
struct Hold {
    serial: u64,
    path: PathBuf,
    at: (f64, f64),
}

/// Under windows, above the background; named so the compositor does not
/// fade it like other shell surfaces.
pub(super) const NAMESPACE: &str = "helix-background-icons";
const LABEL_PX: f32 = helix_draw::theme::size::CAPTION;

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
    /// A press held on an item, timed for a long press.
    hold: Option<Hold>,
    hold_serial: u64,
    /// The item whose menu is (or was last) open.
    menu_for: Option<PathBuf>,
    /// The surface has the keyboard.
    keyboard: bool,
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
        // A click on an icon gives it the keyboard (for the menu key).
        layer.set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
        layer.set_size(0, 0);
        layer.commit();
        self.desktop_icons.view = Some(IconsView { layer, output, width: 0, height: 0, configured: false });
    }

    /// New items (`None`: no icons).
    pub(super) fn desktop_icons_set(&mut self, items: Option<Vec<DesktopItem>>) {
        if let Some(items) = items.as_ref() {
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
            self.desktop_icons.keyboard = false;
            return true;
        }
        false
    }

    fn desktop_icon(&mut self, item: &DesktopItem, px: u32) -> Option<helix_draw::Image> {
        super::source_icon(&mut self.icon_cache, item.icon.clone(), px)
    }

    pub(super) fn draw_desktop_icons(&mut self) {
        let _busy = crate::watchdog::scope("draw desktop icons");
        let Some(view) = self.desktop_icons.view.as_ref().filter(|v| v.configured && v.width > 0 && v.height > 0) else { return };
        let (w, h) = (view.width as usize, view.height as usize);
        let scale = self.scales.of(view.layer.wl_surface());
        let items = self.desktop_icons.items.clone().unwrap_or_default();
        let cells = logic::cells(items.len(), w as i32, h as i32);
        let px = (f64::from(logic::ICON) * scale).round() as u32;
        let images: Vec<Option<helix_draw::Image>> = items.iter().take(cells.len()).map(|i| self.desktop_icon(i, px)).collect();
        let Some(view) = self.desktop_icons.view.as_ref() else { return };
        let Ok((buffer, data)) = self.buffers.create_buffer(view.layer.wl_surface(), Ui::draw_desktop_icons, physical(w, scale) as i32, physical(h, scale) as i32, (physical(w, scale) * 4) as i32, wl_shm::Format::Argb8888) else { return };
        let mut canvas = Canvas::scaled(data, w, h, scale);
        canvas.fill(0x00000000);
        let text = self.text.as_ref();
        for ((item, &(x, y)), image) in items.iter().zip(&cells).zip(&images) {
            if self.desktop_icons.selected.as_ref() == Some(&item.path) {
                canvas.round_rect(f64::from(x + 2), f64::from(y + 2), f64::from(logic::CELL_W - 4), f64::from(logic::CELL_H - 4), 8.0, helix_draw::accent::current().selection);
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
        // Only the icons take input: presses beside them reach the
        // background (and its menu) underneath.
        if let Ok(region) = smithay_client_toolkit::compositor::Region::new(&self.compositor) {
            for &(x, y) in cells.iter().take(items.len()) {
                region.add(x, y, logic::CELL_W, logic::CELL_H);
            }
            surface.set_input_region(Some(region.wl_region()));
        }
        self.scales.prepare(&self.qh, surface, w as i32, h as i32);
        if buffer.attach_to(surface).is_ok() {
            surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
            view.layer.commit();
        }
    }

    /// The icons for the accessibility bus: the surface, its monitor's
    /// place and size, and what it shows.
    pub(super) fn desktop_icons_a11y(&self) -> Option<(&LayerSurface, (u32, u32), Vec<crate::a11y::Node>)> {
        let view = self.desktop_icons.view.as_ref().filter(|v| v.configured && v.width > 0 && v.height > 0)?;
        let items = self.desktop_icons.items.as_deref().unwrap_or_default();
        let cells = logic::cells(items.len(), view.width as i32, view.height as i32);
        Some((&view.layer, (view.width, view.height), icon_nodes(items, &cells, self.desktop_icons.selected.as_deref())))
    }

    /// Take the selection away (a press on the background beside the
    /// icons).
    pub(super) fn desktop_icons_unselect(&mut self) {
        if self.desktop_icons.selected.take().is_some() {
            self.desktop_icons.last_press = None;
            self.draw_desktop_icons();
        }
    }

    /// Pointer input on the surface; true if it was for it. A press
    /// selects the item under it (or nothing); a second press on it within
    /// the double-click time opens it. A right press on an item, or a
    /// long press (primary button or touch), opens its menu.
    pub(super) fn desktop_icons_pointer(&mut self, event: &PointerEvent) -> bool {
        let Some(view) = self.desktop_icons.view.as_ref().filter(|v| v.layer.wl_surface() == &event.surface) else { return false };
        let (x, y) = event.position;
        let hit = || {
            let items = self.desktop_icons.items.as_deref().unwrap_or_default();
            let cells = logic::cells(items.len(), view.width as i32, view.height as i32);
            cells.iter().position(|&(cx, cy)| Rect { x: cx, y: cy, w: logic::CELL_W, h: logic::CELL_H }.contains(x, y)).map(|i| items[i].clone())
        };
        match event.kind {
            PointerEventKind::Press { button: BTN_LEFT, time, .. } => {
                let hit = hit();
                let double = match (&hit, &self.desktop_icons.last_press) {
                    (Some(item), Some((path, at))) => &item.path == path && time.wrapping_sub(*at) <= logic::DOUBLE_CLICK_MS,
                    _ => false,
                };
                self.desktop_icons.hold = None;
                self.desktop_icons.last_press = hit.as_ref().map(|i| (i.path.clone(), time));
                self.desktop_icons.selected = hit.as_ref().map(|i| i.path.clone());
                self.draw_desktop_icons();
                match (double, hit) {
                    (true, Some(item)) => {
                        self.desktop_icons.last_press = None;
                        logic::open(&item);
                    }
                    (false, Some(item)) => self.hold_desktop_icon(item.path, (x, y)),
                    _ => {}
                }
            }
            PointerEventKind::Press { button: BTN_RIGHT, .. } => {
                let hit = hit();
                self.desktop_icons.hold = None;
                self.desktop_icons.last_press = None;
                self.desktop_icons.selected = hit.as_ref().map(|i| i.path.clone());
                self.draw_desktop_icons();
                if let Some(item) = hit {
                    self.open_desktop_icon_menu(&item, (x, y));
                }
            }
            PointerEventKind::Motion { .. } => {
                if self.desktop_icons.hold.as_ref().is_some_and(|h| super::background_menu::moved_too_far(h.at, (x, y))) {
                    self.desktop_icons.hold = None;
                }
            }
            PointerEventKind::Release { .. } | PointerEventKind::Leave { .. } => self.desktop_icons.hold = None,
            _ => {}
        }
        true
    }

    /// A touch was cancelled: it is no long press.
    pub(super) fn desktop_icons_cancel_hold(&mut self) {
        self.desktop_icons.hold = None;
    }

    /// Start timing a press on the item at `path` for a long press.
    fn hold_desktop_icon(&mut self, path: PathBuf, at: (f64, f64)) {
        self.desktop_icons.hold_serial += 1;
        let serial = self.desktop_icons.hold_serial;
        self.desktop_icons.hold = Some(Hold { serial, path, at });
        let Some(handle) = self.loop_handle.as_ref() else { return };
        let _ = handle.insert_source(Timer::from_duration(super::background_menu::LONG_PRESS), move |_, _, ui: &mut Ui| {
            if let Some(hold) = ui.desktop_icons.hold.take_if(|h| h.serial == serial) {
                if let Some(item) = ui.desktop_icons.items.as_ref().and_then(|items| items.iter().find(|i| i.path == hold.path)).cloned() {
                    tracing::debug!(path = %item.path.display(), "desktop icon long press");
                    // The release that follows is no second click.
                    ui.desktop_icons.last_press = None;
                    ui.open_desktop_icon_menu(&item, hold.at);
                }
            }
            TimeoutAction::Drop
        });
    }

    /// Open `item`'s menu at surface point `at`.
    fn open_desktop_icon_menu(&mut self, item: &DesktopItem, at: (f64, f64)) {
        if self.lock.session.is_some() {
            return;
        }
        let Some(view) = self.desktop_icons.view.as_ref() else { return };
        let origin = self.output_state.info(&view.output).and_then(|i| i.logical_position).unwrap_or((0, 0));
        let launching = logic::folder().and_then(|dir| logic::launching(&item.path, &dir, logic::uid()));
        let (x, y) = (origin.0 + at.0 as i32, origin.1 + at.1 as i32);
        // Laid out left to right, drawn mirrored right to left.
        let x = if helix_draw::rtl() { self.mirrored_screen_x(x, y) } else { x };
        tracing::debug!(path = %item.path.display(), ?launching, x, y, "desktop icon menu");
        self.desktop_icons.menu_for = Some(item.path.clone());
        self.open_menu(0, x, y, menu_items(launching), Vec::new());
    }

    /// The name of the item whose menu is open (for the accessibility
    /// bus).
    pub(super) fn desktop_icon_menu_name(&self) -> Option<String> {
        let path = self.desktop_icons.menu_for.as_ref()?;
        self.desktop_icons.items.as_ref()?.iter().find(|i| &i.path == path).map(|i| i.name.clone())
    }

    /// An entry of an icon's menu was chosen.
    pub(super) fn desktop_icon_menu_choice(&mut self, action: IconAction) {
        let Some(path) = self.desktop_icons.menu_for.take() else { return };
        let Some(item) = self.desktop_icons.items.as_ref().and_then(|items| items.iter().find(|i| i.path == path)).cloned() else { return };
        let allow = match action {
            IconAction::Open => return logic::open(&item),
            IconAction::AllowLaunching => true,
            IconAction::DisallowLaunching => false,
        };
        // Looked at again: the file may have changed since the menu opened.
        let Some(dir) = logic::folder().filter(|dir| logic::launching(&path, dir, logic::uid()).is_some()) else {
            tracing::warn!(path = %path.display(), "launching can no longer be changed");
            return;
        };
        match logic::set_launching(&path, allow) {
            Ok(()) => tracing::info!(path = %path.display(), allow, "desktop launcher trust changed"),
            Err(err) => {
                tracing::warn!(%err, path = %path.display(), "cannot change whether it launches");
                return;
            }
        }
        // The icon shows its launcher's name and icon (or its own) at once.
        if self.desktop_icons.items.is_some() {
            self.desktop_icons_set(Some(logic::list(&dir)));
        }
    }

    /// The keyboard came to (or left) `surface`.
    pub(super) fn desktop_icons_keyboard(&mut self, surface: &wl_surface::WlSurface, entered: bool) {
        if self.desktop_icons.view.as_ref().is_some_and(|v| v.layer.wl_surface() == surface) {
            self.desktop_icons.keyboard = entered;
        }
    }

    /// Whether the surface has the keyboard.
    pub(super) fn desktop_icons_have_keyboard(&self) -> bool {
        self.desktop_icons.keyboard && self.desktop_icons.view.is_some()
    }

    /// A key while the surface has the keyboard: Menu or Shift+F10 opens
    /// the selected item's menu (its first entry selected), Return opens
    /// the item, Escape takes the selection away.
    pub(super) fn desktop_icons_key(&mut self, event: &KeyEvent) {
        let Some(view) = self.desktop_icons.view.as_ref() else { return };
        let items = self.desktop_icons.items.as_deref().unwrap_or_default();
        let cells = logic::cells(items.len(), view.width as i32, view.height as i32);
        let selected = self.desktop_icons.selected.as_ref().and_then(|s| items.iter().position(|i| &i.path == s)).filter(|&i| i < cells.len());
        let Some(index) = selected else { return };
        let item = items[index].clone();
        match event.keysym {
            Keysym::Menu => self.desktop_icon_key_menu(&item, cells[index]),
            Keysym::F10 if self.mods.shift => self.desktop_icon_key_menu(&item, cells[index]),
            Keysym::Return | Keysym::KP_Enter => logic::open(&item),
            Keysym::Escape => self.desktop_icons_unselect(),
            _ => {}
        }
    }

    /// The selected item's menu, at the item's middle, its first entry
    /// selected.
    fn desktop_icon_key_menu(&mut self, item: &DesktopItem, (x, y): (i32, i32)) {
        self.menu_by_key = true;
        self.open_desktop_icon_menu(item, (f64::from(x + logic::CELL_W / 2), f64::from(y + logic::CELL_H / 2)));
        self.menu_by_key = false;
    }
}

/// The desktop icons on the accessibility bus (helixdesktop/spec
/// `contracts/dbus/org.a11y.atspi.md`): a `Desktop` list box of the
/// items in their cells, named by their names, described "Folder" or
/// "Application" when they are one; the selected one selected.
pub(super) fn icon_nodes(items: &[DesktopItem], cells: &[(i32, i32)], selected: Option<&std::path::Path>) -> Vec<crate::a11y::Node> {
    use crate::a11y::{Node, Role};
    use helix_i18n::tr;
    let icons = items.iter().zip(cells).map(|(item, &(x, y))| {
        let what = if item.folder { tr("Folder") } else if item.launcher { tr("Application") } else { "" };
        let r = crate::a11y::Rect::new(x, y, logic::CELL_W, logic::CELL_H);
        Node::new(Role::ListBoxOption, &item.name).id(item.path.to_string_lossy().into_owned()).describe(what).at(r).click().selected(selected == Some(item.path.as_path()))
    });
    vec![Node::new(Role::ListBox, tr("Desktop")).id("icons").children(icons)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_entries() {
        let entries = |launching| menu_items(launching).into_iter().map(|i| i.map(|i| (i.label, i.choice))).collect::<Vec<_>>();
        let open = Some(("Open".to_owned(), Choice::DesktopIcon(IconAction::Open)));
        assert_eq!(entries(None), vec![open.clone()]);
        assert_eq!(entries(Some(Launching::Allow)), [open.clone(), None, Some(("Allow Launching".to_owned(), Choice::DesktopIcon(IconAction::AllowLaunching)))]);
        assert_eq!(entries(Some(Launching::Disallow)), [open, None, Some(("Don't Allow Launching".to_owned(), Choice::DesktopIcon(IconAction::DisallowLaunching)))]);
    }
}
