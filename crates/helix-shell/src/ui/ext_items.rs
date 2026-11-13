//! Extensions' panel items and their menus (helix-desktop-spec
//! `contracts/extensions/README.md`, "Panel items", "Menus").

use super::window_menu::{Choice, Item as MenuItem};
use super::*;
use crate::ext_host::{Action, MenuEntry};
use crate::panel_layout::{ExtPanel, Names, Slot};
use helix_draw::Image;

const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;
const GRAPH_W: i32 = 32;
/// Largest `IconData` taken.
const ICON_DATA_MAX: usize = 256;

impl Ui {
    /// The extensions' panel items, as the layout needs them.
    pub(super) fn ext_panels(&self) -> Vec<ExtPanel> {
        self.ext
            .panel
            .iter()
            .map(|p| {
                let mut zones = [false; 3];
                for z in &p.zones {
                    zones[z.index()] = true;
                }
                ExtPanel { id: p.full_id.clone(), name: p.name.clone(), zones, development: p.development }
            })
            .collect()
    }

    /// Names for the panel editor.
    pub(super) fn ext_names(&self) -> Names {
        Names { panel: self.ext_panels(), quick: self.ext.toggles.iter().map(|t| (t.full_id.clone(), t.title.clone())).chain(self.ext.sliders.iter().map(|s| (s.full_id.clone(), s.title.clone()))).collect() }
    }

    /// The extensions' items changed.
    pub(super) fn extensions_changed(&mut self, snapshot: crate::ext_host::Snapshot) {
        if snapshot == self.ext {
            return;
        }
        let rebind = snapshot.commands != self.ext.commands;
        self.ext = snapshot;
        if rebind {
            self.rebind_internal();
        }
        let layout = crate::panel_layout::resolve(&self.panel_keys, &self.ext_panels());
        self.panel_layout = layout;
        self.draw_panel();
        self.draw_power_menu();
        self.draw_panel_editor();
        self.dock_update();
        if self.panel_tooltip.is_some() {
            self.show_panel_tooltip();
        }
    }

    /// Panel item `i`'s icon at `scale`.
    pub(super) fn ext_icon(&mut self, i: usize, scale: f64) -> Option<Image> {
        let item = self.ext.panel.get(i)?.clone();
        let px = (f64::from(status::ICON) * scale).round() as u32;
        let rgb = [(FG >> 16) as u8, (FG >> 8) as u8, FG as u8];
        if !item.icon_data.is_empty() {
            let image = crate::icons::decode(&item.icon_data, px)?;
            return (image.width <= ICON_DATA_MAX && image.height <= ICON_DATA_MAX).then_some(image);
        }
        ext_named_icon(&mut self.icon_cache, &item.icon, &item.dir, px, rgb)
    }

    /// Panel item `i`'s width (0: not shown).
    pub(super) fn ext_width(&self, i: usize, has_icon: bool) -> i32 {
        let Some(item) = self.ext.panel.get(i) else { return 0 };
        if !item.visible {
            return 0;
        }
        let label_w = if item.label.is_empty() { 0 } else { self.text.as_ref().map_or(0.0, |t| t.width(&item.label, 14.0)).ceil() as i32 };
        let parts = [if has_icon { status::ICON } else { 0 }, label_w, if item.graph.is_empty() { 0 } else { GRAPH_W }];
        let shown: Vec<i32> = parts.into_iter().filter(|w| *w > 0).collect();
        if shown.is_empty() {
            0
        } else {
            shown.iter().sum::<i32>() + 4 * (shown.len() as i32 - 1) + 8
        }
    }

}

/// Draw extension panel item `item` at `x`, `w` wide, on a panel `h` tall.
pub(super) fn draw_ext_item(text: Option<&Text>, canvas: &mut Canvas, item: &crate::ext_host::PanelItemView, icon: Option<&Image>, x: i32, w: i32, h: i32) {
    {
        let color = if item.running { FG } else { DIM_FG };
        let mut cx = x + 4;
        if let Some(icon) = icon {
            canvas.image(f64::from(cx), f64::from((h - status::ICON) / 2), f64::from(status::ICON), f64::from(status::ICON), icon);
            cx += status::ICON + 4;
        }
        if !item.label.is_empty() {
            let base = baseline(text, 0, h, 14.0);
            let lw = text.map_or(0.0, |t| t.width(&item.label, 14.0)).ceil() as i32;
            label(text, canvas, &item.label, cx as f32, base, 14.0, color, lw as f32 + 1.0);
            cx += lw + 4;
        }
        if !item.graph.is_empty() {
            // A sparkline, newest at the right.
            let (gx, gy, gh) = (cx, 7, h - 14);
            canvas.rect(gx, gy, GRAPH_W, gh, FIELD_BG);
            let n = item.graph.len();
            let step = f64::from(GRAPH_W) / (n.max(2) - 1) as f64;
            let point = |k: usize| (f64::from(gx) + k as f64 * step, f64::from(gy + gh) - item.graph[k] * f64::from(gh));
            for k in 1..n {
                canvas.line(point(k - 1), point(k), 1.4, if item.running { ACCENT } else { DIM_FG });
            }
        }
        if item.meter >= 0.0 {
            canvas.rect(x + 2, h - 3, ((f64::from(w - 4)) * item.meter).round() as i32, 2, ACCENT);
        }
        if item.attention {
            canvas.rect(x + 2, h - 5, w - 4, 2, ACCENT);
        }
    }
}

impl Ui {

    /// Pointer input on extensions' panel items; true if it was for one.
    pub(super) fn ext_pointer(&mut self, event: &PointerEvent) -> bool {
        if &event.surface != self.panel.wl_surface() || self.lock.session.is_some() {
            return false;
        }
        let x = event.position.0;
        let under = self.panel_placed.iter().find_map(|&(slot, px, pw)| match slot {
            Slot::Ext(i) if x >= f64::from(px) && x < f64::from(px + pw) => Some((i, px)),
            _ => None,
        });
        let Some((i, px)) = under else { return false };
        let Some(full_id) = self.ext.panel.get(i).map(|p| p.full_id.clone()) else { return false };
        let y = PANEL_HEIGHT as i32;
        match event.kind {
            PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                self.panel_hover(Some(full_id), px);
            }
            PointerEventKind::Press { button, .. } => {
                self.panel_tooltip = None;
                self.panel_hover.serial += 1;
                crate::click_feedback();
                let action = match button {
                    BTN_RIGHT => Action::Secondary { x: px, y },
                    BTN_MIDDLE => Action::Middle { x: px, y },
                    _ => self.primary_action(i, px),
                };
                let _ = self.requests.send(UiRequest::ExtAction(full_id, action));
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
                let _ = self.requests.send(UiRequest::ExtAction(full_id, Action::Scroll { steps, horizontal }));
            }
            _ => {}
        }
        true
    }

    /// Enter on an extension's panel item.
    pub(super) fn ext_activate(&mut self, i: usize, x: i32) {
        if let Some(full_id) = self.ext.panel.get(i).map(|p| p.full_id.clone()) {
            crate::click_feedback();
            let action = self.primary_action(i, x);
            let _ = self.requests.send(UiRequest::ExtAction(full_id, action));
        }
    }

    /// A primary click on panel item `i` at `x`: its popover (under it,
    /// moved left so 480 pixels fit), else `Activate`.
    fn primary_action(&self, i: usize, x: i32) -> Action {
        if !self.ext.panel.get(i).is_some_and(|p| p.popover) {
            return Action::Primary { x, y: PANEL_HEIGHT as i32 };
        }
        let width = self.panel_width as i32;
        let px = x.min(width - 480 - 4).max(4);
        Action::Popover { x: px, y: PANEL_HEIGHT as i32 + 4, output: self.panel_output_name() }
    }

    /// The connector of the monitor the panel is on (the primary one).
    pub(super) fn panel_output_name(&self) -> String {
        let (x, y, _, _) = self.primary_area;
        self.output_state
            .outputs()
            .filter_map(|o| self.output_state.info(&o))
            .find(|i| i.logical_position == Some((x, y)))
            .or_else(|| self.output_state.outputs().next().and_then(|o| self.output_state.info(&o)))
            .and_then(|i| i.name.clone())
            .unwrap_or_default()
    }

    /// The tooltip lines of extension item `key`.
    pub(super) fn ext_tooltip_lines(&self, key: &str) -> Option<Vec<String>> {
        let item = self.ext.panel.iter().find(|p| p.full_id == key)?;
        Some(item.tooltip.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect())
    }

    /// Show an extension item's menu below it.
    pub(super) fn show_ext_menu(&mut self, full_id: String, menu: String, x: i32, entries: Vec<MenuEntry>) {
        let mut ids = Vec::new();
        let items = menu_tree(&entries, 0, &mut ids);
        if items.is_empty() {
            return;
        }
        // A dock item's menu opens as dock menus do.
        match self.ext_dock_menu_at.take().filter(|_| self.ext.dock.iter().any(|d| d.full_id == full_id)) {
            Some((lx, ly)) => self.open_dock_menu(lx, ly, items),
            None => self.open_menu(0, x, PANEL_HEIGHT as i32, items, Vec::new()),
        }
        self.ext_menu = Some((full_id, menu, ids));
    }

    /// Entry `index` of the open extension menu was chosen.
    pub(super) fn ext_menu_choice(&mut self, index: usize) {
        let Some((full_id, menu, ids)) = self.ext_menu.take() else { return };
        if let Some(&id) = ids.get(index) {
            let _ = self.requests.send(UiRequest::ExtAction(full_id, Action::MenuChosen { menu, id }));
        }
    }
}

/// The menu entries under `parent`, as the panel's menu items; `ids`
/// collects the chosen entries' ids, which `Choice::ExtMenu` indexes.
fn menu_tree(entries: &[MenuEntry], parent: i32, ids: &mut Vec<i32>) -> Vec<Option<MenuItem>> {
    entries
        .iter()
        .filter(|e| e.parent == parent)
        .map(|e| match e.kind.as_str() {
            "separator" => None,
            "submenu" => Some(MenuItem { label: e.label.clone(), choice: Choice::Submenu, checked: None, active: false, sub: menu_tree(entries, e.id, ids) }),
            kind => {
                let choice = if kind == "heading" || !e.enabled {
                    Choice::Disabled
                } else {
                    ids.push(e.id);
                    Choice::ExtMenu(ids.len() - 1)
                };
                Some(MenuItem { label: e.label.clone(), choice, checked: (kind == "check").then_some(e.checked), active: kind == "radio" && e.checked, sub: Vec::new() })
            }
        })
        .collect()
}

/// An icon named by an extension: a theme name, or a file inside its
/// directory (other paths are ignored).
pub(super) fn ext_named_icon(cache: &mut std::collections::HashMap<String, Option<Image>>, name: &str, dir: &std::path::Path, px: u32, rgb: [u8; 3]) -> Option<Image> {
    if name.is_empty() {
        return None;
    }
    let key = format!("ext:{}:{name}@{px}", dir.display());
    cache
        .entry(key)
        .or_insert_with(|| {
            if name.contains('/') {
                let path = dir.join(name);
                let (real, root) = (path.canonicalize().ok()?, dir.canonicalize().ok()?);
                if !real.starts_with(&root) {
                    return None;
                }
                crate::icons::decode(&std::fs::read(real).ok()?, px)
            } else if name.ends_with("-symbolic") {
                crate::icons::symbolic(name, px, rgb)
            } else {
                crate::icons::load(&crate::icons::IconSource::Names(vec![name.to_owned()]), px)
            }
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_trees() {
        let e = |id, parent, kind: &str, label: &str, enabled| MenuEntry { id, parent, kind: kind.into(), label: label.into(), enabled, checked: kind == "check" || kind == "radio" };
        let entries = vec![e(1, 0, "item", "One", true), e(2, 0, "separator", "", true), e(3, 0, "submenu", "More", true), e(4, 3, "radio", "Inner", true), e(5, 0, "heading", "Title", true), e(6, 0, "check", "Off", false)];
        let mut ids = Vec::new();
        let tree = menu_tree(&entries, 0, &mut ids);
        assert_eq!(tree.len(), 5);
        assert!(tree[1].is_none());
        let more = tree[2].as_ref().unwrap();
        assert_eq!((more.choice, more.sub.len()), (Choice::Submenu, 1));
        assert!(more.sub[0].as_ref().unwrap().active);
        assert_eq!(tree[3].as_ref().unwrap().choice, Choice::Disabled, "headings cannot be chosen");
        assert_eq!(tree[4].as_ref().unwrap().choice, Choice::Disabled, "disabled");
        assert_eq!(ids, [1, 4]);
    }
}
