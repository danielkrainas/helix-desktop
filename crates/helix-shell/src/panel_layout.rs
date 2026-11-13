//! The panel's layout (helix-desktop-spec `contracts/wayland/README.md`,
//! "Panel layout"; keys in `contracts/gsettings/org.helix.shell.md`):
//! which items each zone shows, and where they go.

use tokio::sync::mpsc;

use crate::UiEvent;

pub const SCHEMA: &str = "org.helix.shell";

/// A built-in panel item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Item {
    Apps,
    Workspaces,
    Clock,
    Tray,
    Accessibility,
    Keyboard,
    Status,
    Power,
}

impl Item {
    pub const ALL: [Item; 8] = [Item::Apps, Item::Workspaces, Item::Clock, Item::Tray, Item::Accessibility, Item::Keyboard, Item::Status, Item::Power];

    pub fn id(self) -> &'static str {
        match self {
            Item::Apps => "helix:apps",
            Item::Workspaces => "helix:workspaces",
            Item::Clock => "helix:clock",
            Item::Tray => "helix:tray",
            Item::Accessibility => "helix:accessibility",
            Item::Keyboard => "helix:keyboard",
            Item::Status => "helix:status",
            Item::Power => "helix:power",
        }
    }

    pub fn from_id(id: &str) -> Option<Item> {
        Item::ALL.into_iter().find(|i| i.id() == id)
    }

    /// The name the panel editor shows.
    pub fn name(self) -> &'static str {
        match self {
            Item::Apps => "Apps",
            Item::Workspaces => "Workspaces",
            Item::Clock => "Clock",
            Item::Tray => "Status Icons",
            Item::Accessibility => "Accessibility",
            Item::Keyboard => "Input Source",
            Item::Status => "System Status",
            Item::Power => "Power",
        }
    }
}

/// The quick settings switches' ids, in their default order.
pub const QUICK_SWITCHES: [&str; 7] = ["helix:wifi", "helix:bluetooth", "helix:airplane-mode", "helix:auto-rotate", "helix:night-light", "helix:dark-style", "helix:do-not-disturb"];

/// The zones' items and the quick settings switches, as the keys hold them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keys {
    pub left: Vec<String>,
    pub center: Vec<String>,
    pub right: Vec<String>,
    pub quick: Vec<String>,
}

impl Default for Keys {
    fn default() -> Self {
        let ids = |items: &[Item]| items.iter().map(|i| i.id().to_owned()).collect();
        Keys {
            left: ids(&[Item::Apps, Item::Workspaces]),
            center: ids(&[Item::Clock]),
            right: ids(&[Item::Tray, Item::Accessibility, Item::Keyboard, Item::Status, Item::Power]),
            quick: QUICK_SWITCHES.iter().map(|s| s.to_string()).collect(),
        }
    }
}

/// An enabled extension's panel item, as the layout needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtPanel {
    /// `<extension id>/<item id>`.
    pub id: String,
    pub name: String,
    /// Allowed in left, center, right.
    pub zones: [bool; 3],
    /// Shown without being in the keys (development extensions).
    pub development: bool,
}

/// The items shown in each zone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub left: Vec<Slot>,
    pub center: Vec<Slot>,
    pub right: Vec<Slot>,
}

impl Default for Layout {
    fn default() -> Self {
        resolve(&Keys::default(), &[])
    }
}

/// The items to show: unknown ids are skipped, an item is shown at its
/// first place only, an extension's item only in a zone it allows, and
/// Power always somewhere (at the right end). Development extensions'
/// items not in the keys go at the end of the first zone they allow.
pub fn resolve(keys: &Keys, ext: &[ExtPanel]) -> Layout {
    let mut seen: Vec<String> = Vec::new();
    let mut zone = |ids: &[String], z: usize| -> Vec<Slot> {
        let mut out = Vec::new();
        for id in ids {
            if seen.contains(id) {
                continue;
            }
            let slot = match Item::from_id(id) {
                Some(item) => Some(Slot::Item(item)),
                None => ext.iter().position(|e| e.id == *id && e.zones[z]).map(Slot::Ext),
            };
            if let Some(slot) = slot {
                seen.push(id.clone());
                out.push(slot);
            }
        }
        out
    };
    let mut zones = [zone(&keys.left, 0), zone(&keys.center, 1), zone(&keys.right, 2)];
    for (i, e) in ext.iter().enumerate() {
        if e.development && !seen.contains(&e.id) {
            if let Some(z) = e.zones.iter().position(|ok| *ok) {
                zones[z].push(Slot::Ext(i));
            }
        }
    }
    let [left, center, mut right] = zones;
    if !seen.iter().any(|s| s == Item::Power.id()) {
        right.push(Slot::Item(Item::Power));
    }
    Layout { left, center, right }
}

/// Something placed on the panel: a layout item, an extension's item (by
/// index into the extensions' panel items), or the privacy indicators
/// ("Recording", "Sharing"), which are not layout items.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    Item(Item),
    Ext(usize),
    Privacy,
}

pub const GAP: i32 = 4;

/// Where everything goes on a panel `width` wide: (slot, x, width), left
/// to right. `size` gives each slot's width (0: not shown). The privacy
/// indicators go left of the right zone's items; center items are
/// centered, moved clear of the other zones, and left out when they
/// still do not fit.
pub fn place(layout: &Layout, width: i32, size: impl Fn(Slot) -> i32) -> Vec<(Slot, i32, i32)> {
    let mut placed = Vec::new();
    let mut x = 0;
    for slot in &layout.left {
        let w = size(*slot);
        if w > 0 {
            placed.push((*slot, x, w));
            x += w + GAP;
        }
    }
    let left_end = x;
    let mut right: Vec<(Slot, i32)> = Vec::new();
    let mut slots: Vec<Slot> = vec![Slot::Privacy];
    slots.extend(layout.right.iter().copied());
    let mut rx = width;
    for slot in slots.into_iter().rev() {
        let w = size(slot);
        if w > 0 {
            rx -= w;
            right.push((slot, rx));
            rx -= GAP;
        }
    }
    let right_start = rx + GAP;
    let centers: Vec<(Slot, i32)> = layout.center.iter().map(|s| (*s, size(*s))).filter(|(_, w)| *w > 0).collect();
    let total: i32 = centers.iter().map(|(_, w)| w).sum::<i32>() + GAP * (centers.len() as i32 - 1).max(0);
    let mut cx = (width - total) / 2;
    cx = cx.max(left_end).min(right_start - GAP - total);
    for (slot, w) in centers {
        if cx >= left_end && cx + w <= right_start - GAP {
            placed.push((slot, cx, w));
        }
        cx += w + GAP;
    }
    placed.extend(right.into_iter().rev().map(|(slot, x)| (slot, x, size(slot))));
    placed
}

/// The editor's columns: the three zones, then the quick settings
/// switches.
pub const COLUMNS: [&str; 4] = ["Left", "Center", "Right", "Quick Settings"];
pub const QUICK: usize = 3;

/// A switch's name in the editor.
pub fn switch_name(id: &str) -> Option<&'static str> {
    Some(match id {
        "helix:wifi" => "Wi-Fi",
        "helix:bluetooth" => "Bluetooth",
        "helix:airplane-mode" => "Airplane Mode",
        "helix:auto-rotate" => "Auto Rotate",
        "helix:night-light" => "Night Light",
        "helix:dark-style" => "Dark Style",
        "helix:do-not-disturb" => "Do Not Disturb",
        _ => return None,
    })
}

impl Keys {
    fn column(&self, col: usize) -> &Vec<String> {
        match col {
            0 => &self.left,
            1 => &self.center,
            2 => &self.right,
            _ => &self.quick,
        }
    }

    fn column_mut(&mut self, col: usize) -> &mut Vec<String> {
        match col {
            0 => &mut self.left,
            1 => &mut self.center,
            2 => &mut self.right,
            _ => &mut self.quick,
        }
    }

    /// With Power in a zone (it is shown at the right end otherwise).
    pub fn normalized(&self) -> Keys {
        let mut keys = self.clone();
        let power = Item::Power.id();
        if ![&keys.left, &keys.center, &keys.right].iter().any(|z| z.iter().any(|i| i == power)) {
            keys.right.push(power.to_owned());
        }
        keys
    }
}

/// Extensions' items, for the editor: panel items (with the zones they
/// allow) and quick settings toggles and sliders.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Names {
    pub panel: Vec<ExtPanel>,
    pub quick: Vec<(String, String)>,
}

impl Names {
    /// An item's name in column `col`, if it may be shown there.
    fn name(&self, id: &str, col: usize) -> Option<String> {
        if col == QUICK {
            return switch_name(id).map(str::to_owned).or_else(|| self.quick.iter().find(|(i, _)| i == id).map(|(_, n)| n.clone()));
        }
        Item::from_id(id).map(|i| i.name().to_owned()).or_else(|| self.panel.iter().find(|e| e.id == id && e.zones[col]).map(|e| e.name.clone()))
    }

    fn allowed(&self, id: &str, col: usize) -> bool {
        Item::from_id(id).is_some() || self.panel.iter().any(|e| e.id == id && e.zones[col])
    }
}

/// A row of the editor: where its id is in the column's key, and its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub index: usize,
    pub id: String,
    pub name: String,
}

/// The rows of column `col`: the items it shows (what Helix does not know,
/// and items shown elsewhere first, are left out).
pub fn rows(keys: &Keys, col: usize, names: &Names) -> Vec<Row> {
    let keys = keys.normalized();
    let mut seen: Vec<&str> = Vec::new();
    let first = if col == QUICK { QUICK } else { 0 };
    for c in first..=col {
        let mut rows = Vec::new();
        for (index, id) in keys.column(c).iter().enumerate() {
            let Some(name) = names.name(id, c) else { continue };
            if seen.contains(&id.as_str()) {
                continue;
            }
            seen.push(id);
            rows.push(Row { index, id: id.clone(), name });
        }
        if c == col {
            return rows;
        }
    }
    Vec::new()
}

/// What is shown nowhere: (column it is added to, id, name).
pub fn hidden(keys: &Keys, names: &Names) -> Vec<(usize, String, String)> {
    let shown: Vec<String> = (0..3).flat_map(|c| rows(keys, c, names)).map(|r| r.id).collect();
    let mut out: Vec<(usize, String, String)> = Item::ALL.iter().filter(|i| !shown.iter().any(|s| s == i.id())).map(|i| (2, i.id().to_owned(), i.name().to_owned())).collect();
    out.extend(names.panel.iter().filter(|e| !e.development && !shown.contains(&e.id)).filter_map(|e| Some((e.zones.iter().position(|z| *z)?, e.id.clone(), e.name.clone()))));
    let switches: Vec<String> = rows(keys, QUICK, names).into_iter().map(|r| r.id).collect();
    out.extend(QUICK_SWITCHES.iter().filter(|id| !switches.iter().any(|s| s == *id)).filter_map(|id| Some((QUICK, id.to_string(), switch_name(id)?.to_owned()))));
    out.extend(names.quick.iter().filter(|(id, _)| !switches.contains(id)).map(|(id, name)| (QUICK, id.clone(), name.clone())));
    out
}

/// A change made in the editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
    /// Row `row` of column `col` up or down.
    Up(usize, usize),
    Down(usize, usize),
    /// Row `row` of zone `col` to the end of zone `to` (if it may go there).
    Move(usize, usize, usize),
    /// Row `row` of column `col` out (not Power).
    Remove(usize, usize),
    /// A hidden item or switch back, at the end of its column.
    Add(String),
    Reset,
}

/// `keys` after `edit`. Ids Helix does not know stay where they are.
pub fn edit(keys: &Keys, edit: Edit, names: &Names) -> Keys {
    let mut keys = keys.normalized();
    match edit {
        Edit::Up(col, row) | Edit::Down(col, row) => {
            let rows = rows(&keys, col, names);
            let other = if matches!(edit, Edit::Up(..)) { row.checked_sub(1) } else { Some(row + 1) };
            if let (Some(a), Some(b)) = (rows.get(row), other.and_then(|o| rows.get(o))) {
                keys.column_mut(col).swap(a.index, b.index);
            }
        }
        Edit::Move(col, row, to) if col < QUICK && to < QUICK && col != to => {
            if let Some(r) = rows(&keys, col, names).get(row).filter(|r| names.allowed(&r.id, to)) {
                for z in 0..QUICK {
                    keys.column_mut(z).retain(|i| *i != r.id);
                }
                keys.column_mut(to).push(r.id.clone());
            }
        }
        Edit::Move(..) => {}
        Edit::Remove(col, row) => {
            if let Some(r) = rows(&keys, col, names).get(row).filter(|r| r.id != Item::Power.id()) {
                let zones = if col == QUICK { QUICK..QUICK + 1 } else { 0..QUICK };
                for z in zones {
                    keys.column_mut(z).retain(|i| *i != r.id);
                }
            }
        }
        Edit::Add(id) => {
            if let Some((col, id, _)) = hidden(&keys, names).into_iter().find(|(_, i, _)| *i == id) {
                keys.column_mut(col).push(id);
            }
        }
        Edit::Reset => keys = Keys::default(),
    }
    keys
}

/// The keys as they are now (the defaults without the schema).
pub fn read() -> Keys {
    use gio::prelude::*;
    let Some(source) = gio::SettingsSchemaSource::default() else { return Keys::default() };
    if source.lookup(SCHEMA, true).is_none() {
        return Keys::default();
    }
    let s = gio::Settings::new(SCHEMA);
    let strv = |k: &str| s.strv(k).iter().map(|v| v.to_string()).collect();
    Keys { left: strv("panel-left"), center: strv("panel-center"), right: strv("panel-right"), quick: strv("quick-settings") }
}

/// Write the zones' ids.
pub fn write(keys: &Keys) {
    use gio::prelude::*;
    if !helix_settings::has_schema(SCHEMA) {
        tracing::warn!("{SCHEMA} is not installed; the panel layout cannot be saved");
        return;
    }
    let s = gio::Settings::new(SCHEMA);
    for (key, ids) in [("panel-left", &keys.left), ("panel-center", &keys.center), ("panel-right", &keys.right), ("quick-settings", &keys.quick)] {
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        let _ = s.set_strv(key, refs.as_slice());
    }
    gio::Settings::sync();
}

/// Send the layout to the UI now and whenever the keys change.
pub async fn follow(ui: mpsc::UnboundedSender<UiEvent>) {
    let _ = ui.send(UiEvent::PanelLayout(read()));
    if !helix_settings::has_schema(SCHEMA) {
        tracing::info!("{SCHEMA} is not installed; the panel uses its default layout");
        return;
    }
    let watched = helix_settings::GSettings::new(&[SCHEMA]);
    let (mut changes, _watch) = watched.watch();
    while changes.recv().await.is_some() {
        let _ = ui.send(UiEvent::PanelLayout(read()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(left: &[&str], center: &[&str], right: &[&str]) -> Keys {
        let v = |l: &[&str]| l.iter().map(|s| s.to_string()).collect();
        Keys { left: v(left), center: v(center), right: v(right), quick: Vec::new() }
    }

    #[test]
    fn resolving() {
        assert_eq!(Layout::default().right, [Item::Tray, Item::Accessibility, Item::Keyboard, Item::Status, Item::Power].map(Slot::Item));
        let l = resolve(&keys(&["helix:clock", "org.example.X/item", "helix:apps"], &["helix:clock"], &["helix:status"]), &[]);
        assert_eq!(l.left, [Slot::Item(Item::Clock), Slot::Item(Item::Apps)], "unknown ids skipped");
        assert!(l.center.is_empty(), "first place wins");
        assert_eq!(l.right, [Slot::Item(Item::Status), Slot::Item(Item::Power)], "Power always shown");
        let l = resolve(&keys(&["helix:power"], &[], &[]), &[]);
        assert_eq!((l.left.as_slice(), l.right.as_slice()), ([Slot::Item(Item::Power)].as_slice(), [].as_slice()));
    }

    #[test]
    fn editing() {
        let mut k = keys(&["helix:apps", "org.x/y", "helix:workspaces"], &["helix:clock"], &["helix:status"]);
        k.quick = vec!["helix:wifi".into(), "helix:dark-style".into()];
        let n = Names::default();
        // Power is listed (it is always shown), the unknown id is not.
        assert_eq!(rows(&k, 2, &n).iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["System Status", "Power"]);
        assert_eq!(rows(&k, 0, &n).iter().map(|r| r.index).collect::<Vec<_>>(), [0, 2]);
        // Moving past the unknown id keeps it in place.
        let up = edit(&k, Edit::Up(0, 1), &n);
        assert_eq!(up.left, ["helix:workspaces", "org.x/y", "helix:apps"]);
        assert_eq!(edit(&k, Edit::Up(0, 0), &n).left, k.left, "first row stays");
        let moved = edit(&k, Edit::Move(1, 0, 0), &n);
        assert_eq!((moved.left.last().map(String::as_str), moved.center.len()), (Some("helix:clock"), 0));
        let removed = edit(&k, Edit::Remove(2, 0), &n);
        assert_eq!(removed.right, ["helix:power"]);
        assert_eq!(edit(&k, Edit::Remove(2, 1), &n).right, ["helix:status", "helix:power"], "Power stays");
        assert!(hidden(&removed, &n).iter().any(|(c, id, _)| *c == 2 && id == "helix:status"));
        assert_eq!(edit(&removed, Edit::Add("helix:status".into()), &n).right, ["helix:power", "helix:status"]);
        let fewer = edit(&k, Edit::Remove(QUICK, 0), &n);
        assert_eq!(fewer.quick, ["helix:dark-style"]);
        assert!(hidden(&fewer, &n).iter().any(|(c, id, _)| *c == QUICK && id == "helix:wifi"));
        assert_eq!(edit(&fewer, Edit::Add("helix:wifi".into()), &n).quick, ["helix:dark-style", "helix:wifi"]);
        assert_eq!(edit(&k, Edit::Reset, &n), Keys::default());
    }

    #[test]
    fn extension_items() {
        let ext = vec![
            ExtPanel { id: "org.x.A/clock".into(), name: "World Clock".into(), zones: [false, true, true], development: false },
            ExtPanel { id: "org.x.Dev/p".into(), name: "Dev".into(), zones: [true, true, true], development: true },
        ];
        let k = keys(&["org.x.A/clock", "helix:apps"], &["helix:clock"], &["org.x.A/clock", "helix:power"]);
        let l = resolve(&k, &ext);
        assert_eq!(l.left, [Slot::Item(Item::Apps), Slot::Ext(1)], "not allowed on the left; development item added");
        assert_eq!(l.right, [Slot::Ext(0), Slot::Item(Item::Power)]);
        let names = Names { panel: ext.clone(), quick: vec![("org.x.A/t".into(), "Toggle".into())] };
        assert_eq!(rows(&k, 2, &names)[0].name, "World Clock");
        assert!(rows(&k, 0, &names).iter().all(|r| r.id != "org.x.A/clock"));
        assert_eq!(edit(&k, Edit::Move(2, 0, 0), &names), k.normalized(), "not to a zone it does not allow");
        let moved = edit(&k, Edit::Move(2, 0, 1), &names);
        assert_eq!(moved.center, ["helix:clock", "org.x.A/clock"]);
        let gone = edit(&k, Edit::Remove(2, 0), &names);
        assert!(hidden(&gone, &names).contains(&(1, "org.x.A/clock".into(), "World Clock".into())), "back to its first zone");
        assert!(hidden(&k, &names).iter().any(|(c, id, _)| *c == QUICK && id == "org.x.A/t"));
        assert!(!hidden(&k, &names).iter().any(|(_, id, _)| id == "org.x.Dev/p"), "development items are not listed");
    }

    #[test]
    fn placing() {
        let layout = Layout::default();
        let size = |s: Slot| match s {
            Slot::Item(Item::Apps) => 60,
            Slot::Item(Item::Workspaces) => 0,
            Slot::Item(Item::Clock) => 100,
            Slot::Item(Item::Status) => 40,
            Slot::Item(Item::Power) => 70,
            Slot::Privacy => 30,
            _ => 0,
        };
        let placed = place(&layout, 1000, size);
        assert_eq!(
            placed,
            [
                (Slot::Item(Item::Apps), 0, 60),
                (Slot::Item(Item::Clock), 450, 100),
                (Slot::Privacy, 1000 - 70 - 4 - 40 - 4 - 30, 30),
                (Slot::Item(Item::Status), 1000 - 70 - 4 - 40, 40),
                (Slot::Item(Item::Power), 930, 70),
            ]
        );
        // A crowded panel: the clock moves clear of the left zone…
        let wide = |s: Slot| if s == Slot::Item(Item::Apps) { 480 } else { size(s) };
        assert_eq!(place(&layout, 1000, wide)[1], (Slot::Item(Item::Clock), 484, 100));
        // …and is left out when it cannot fit.
        let wider = |s: Slot| if s == Slot::Item(Item::Apps) { 760 } else { size(s) };
        assert!(!place(&layout, 1000, wider).iter().any(|p| p.0 == Slot::Item(Item::Clock)));
    }
}
