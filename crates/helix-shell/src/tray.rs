//! Apps' status icons (helix-desktop-spec
//! `contracts/dbus/org.kde.StatusNotifierItem.md`): the shell is the
//! StatusNotifierWatcher and the host. Items are followed here; the panel
//! (`helix:tray`) draws them and sends clicks back.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::mpsc;
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Value};
use zbus::Connection;

use crate::UiEvent;

pub const BUS_NAME: &str = "org.kde.StatusNotifierWatcher";
pub const OBJECT_PATH: &str = "/StatusNotifierWatcher";
const ITEM: &str = "org.kde.StatusNotifierItem";
const MENU: &str = "com.canonical.dbusmenu";
/// At most this many items.
pub const MAX_ITEMS: usize = 64;
/// Pixmaps larger than this are ignored.
const MAX_PIXMAP: i32 = 256;
/// Menus: entries per level and levels.
const MAX_ENTRIES: usize = 100;
const MAX_DEPTH: usize = 3;
const CALL_TIMEOUT: Duration = Duration::from_secs(2);

/// An item's icon: a name (with the item's own theme directory), or
/// pixmaps as RGBA (width, height, pixels).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Icon {
    pub name: String,
    pub theme_path: String,
    pub pixmaps: Vec<(u32, u32, Vec<u8>)>,
}

impl Icon {
    pub fn is_empty(&self) -> bool {
        self.name.is_empty() && self.pixmaps.is_empty()
    }
}

/// What the panel needs of an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// `<unique name><path>`.
    pub key: String,
    pub title: String,
    pub status: String,
    pub icon: Icon,
    pub attention_icon: Icon,
    /// Drawn small over the icon's corner.
    pub overlay_icon: Icon,
    /// The tooltip: its title and its text (markup removed).
    pub tooltip_title: String,
    pub tooltip_text: String,
    pub item_is_menu: bool,
    pub has_menu: bool,
}

impl Item {
    /// Whether the panel shows it.
    pub fn shown(&self) -> bool {
        self.status != "Passive" && !self.icon_now().is_empty()
    }

    /// The tooltip's lines: the title (else the item's), then at most 6
    /// lines of text; empty for none.
    pub fn tooltip_lines(&self) -> Vec<String> {
        let title = if self.tooltip_title.trim().is_empty() { self.title.trim() } else { self.tooltip_title.trim() };
        let mut lines = Vec::new();
        if !title.is_empty() {
            lines.push(title.to_owned());
        }
        lines.extend(self.tooltip_text.lines().map(str::trim).filter(|l| !l.is_empty()).take(6).map(str::to_owned));
        if lines.len() == 1 && self.tooltip_title.trim().is_empty() && self.tooltip_text.trim().is_empty() {
            // Only the item's title: that is no tooltip.
            return Vec::new();
        }
        lines
    }

    pub fn needs_attention(&self) -> bool {
        self.status == "NeedsAttention"
    }

    /// The icon to draw now.
    pub fn icon_now(&self) -> &Icon {
        if self.needs_attention() && !self.attention_icon.is_empty() {
            &self.attention_icon
        } else {
            &self.icon
        }
    }
}

/// A menu entry from `com.canonical.dbusmenu`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuEntry {
    Separator,
    Item {
        id: i32,
        label: String,
        enabled: bool,
        /// A check box (`checkmark`) and whether it is on.
        check: Option<bool>,
        /// A radio item that is on.
        radio_on: bool,
        children: Vec<MenuEntry>,
    },
}

/// What the panel asks of an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Primary,
    Secondary,
    Middle,
    /// Wheel steps (negative: up or left) and whether horizontal.
    Scroll(i32, bool),
    /// A menu entry was chosen.
    MenuClicked(i32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub key: String,
    pub action: Action,
    /// The icon's bottom-left corner, in the layout's coordinates.
    pub x: i32,
    pub y: i32,
}

/// `__` is one underscore; other underscores (access keys) are dropped.
pub fn plain_label(label: &str) -> String {
    let mut out = String::new();
    let mut chars = label.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '_' {
            if chars.peek() == Some(&'_') {
                chars.next();
                out.push('_');
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Pixmaps (`a(iiay)`, ARGB32 in network byte order) as RGBA; bad or
/// too large ones are skipped.
pub fn pixmaps(raw: Vec<(i32, i32, Vec<u8>)>) -> Vec<(u32, u32, Vec<u8>)> {
    raw.into_iter()
        .filter(|(w, h, data)| *w > 0 && *h > 0 && *w <= MAX_PIXMAP && *h <= MAX_PIXMAP && data.len() == (*w as usize) * (*h as usize) * 4)
        .map(|(w, h, data)| {
            let rgba = data.as_chunks::<4>().0.iter().flat_map(|p| [p[1], p[2], p[3], p[0]]).collect();
            (w as u32, h as u32, rgba)
        })
        .collect()
}

/// The pixmap to draw at `px`: the smallest at least that large, else
/// the largest.
pub fn best_pixmap(pixmaps: &[(u32, u32, Vec<u8>)], px: u32) -> Option<&(u32, u32, Vec<u8>)> {
    pixmaps.iter().filter(|p| p.0.max(p.1) >= px).min_by_key(|p| p.0.max(p.1)).or_else(|| pixmaps.iter().max_by_key(|p| p.0.max(p.1)))
}

/// A menu from a `GetLayout` reply's children.
pub fn menu_entries(children: &[OwnedValue], depth: usize) -> Vec<MenuEntry> {
    let mut out = Vec::new();
    for child in children.iter().take(MAX_ENTRIES) {
        let Ok((id, props, kids)) = <(i32, HashMap<String, OwnedValue>, Vec<OwnedValue>)>::try_from(child.clone()) else { continue };
        let string = |k: &str| props.get(k).and_then(|v| String::try_from(v.clone()).ok());
        let flag = |k: &str, default: bool| props.get(k).and_then(|v| bool::try_from(v.clone()).ok()).unwrap_or(default);
        if !flag("visible", true) {
            continue;
        }
        if string("type").as_deref() == Some("separator") {
            out.push(MenuEntry::Separator);
            continue;
        }
        let state = props.get("toggle-state").and_then(|v| i32::try_from(v.clone()).ok()).unwrap_or(-1);
        let toggle = string("toggle-type").unwrap_or_default();
        let children = if depth + 1 < MAX_DEPTH { menu_entries(&kids, depth + 1) } else { Vec::new() };
        out.push(MenuEntry::Item {
            id,
            label: plain_label(&string("label").unwrap_or_default()),
            enabled: flag("enabled", true),
            check: (toggle == "checkmark").then_some(state == 1),
            radio_on: toggle == "radio" && state == 1,
            children,
        });
    }
    out
}

/// The items, shared by the watcher and their followers.
#[derive(Clone)]
pub struct Tray {
    items: Arc<Mutex<Vec<Item>>>,
    ui: mpsc::UnboundedSender<UiEvent>,
}

impl Tray {
    pub fn new(ui: mpsc::UnboundedSender<UiEvent>) -> Self {
        Self { items: Default::default(), ui }
    }

    fn list(&self) -> std::sync::MutexGuard<'_, Vec<Item>> {
        self.items.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn publish(&self) {
        let _ = self.ui.send(UiEvent::Tray(self.list().clone()));
    }

    fn keys(&self) -> Vec<String> {
        self.list().iter().map(|i| i.key.clone()).collect()
    }

    fn update(&self, item: Item) {
        let mut list = self.list();
        if let Some(slot) = list.iter_mut().find(|i| i.key == item.key) {
            if *slot == item {
                return;
            }
            *slot = item;
        }
        drop(list);
        self.publish();
    }

    fn remove(&self, key: &str) {
        self.list().retain(|i| i.key != key);
        self.publish();
    }

    /// Carry out a panel request.
    pub async fn request(&self, conn: &Connection, request: Request) {
        let Some((bus, path)) = split_key(&request.key) else { return };
        let Some(item) = self.list().iter().find(|i| i.key == request.key).cloned() else { return };
        let call = |method: &'static str, body: (i32, i32)| async move {
            let result = tokio::time::timeout(CALL_TIMEOUT, conn.call_method(Some(bus), path, Some(ITEM), method, &body)).await;
            matches!(result, Ok(Ok(_)))
        };
        let (x, y) = (request.x, request.y);
        let show_menu = match request.action {
            Action::Primary if item.item_is_menu && item.has_menu => true,
            Action::Primary => !call("Activate", (x, y)).await && item.has_menu,
            Action::Secondary if item.has_menu => true,
            Action::Secondary => {
                call("ContextMenu", (x, y)).await;
                false
            }
            Action::Middle => {
                call("SecondaryActivate", (x, y)).await;
                false
            }
            Action::Scroll(steps, horizontal) => {
                let orientation = if horizontal { "horizontal" } else { "vertical" };
                let _ = tokio::time::timeout(CALL_TIMEOUT, conn.call_method(Some(bus), path, Some(ITEM), "Scroll", &(steps * 120, orientation))).await;
                false
            }
            Action::MenuClicked(id) => {
                if let Some(menu) = menu_path(conn, bus, path).await {
                    let time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as u32);
                    let body = (id, "clicked", Value::from(0i32), time);
                    let _ = tokio::time::timeout(CALL_TIMEOUT, conn.call_method(Some(bus), menu.as_str(), Some(MENU), "Event", &body)).await;
                }
                false
            }
        };
        if show_menu {
            match read_menu(conn, bus, path).await {
                Some(entries) if !entries.is_empty() => {
                    let _ = self.ui.send(UiEvent::TrayMenu { key: request.key, x, entries });
                }
                _ => tracing::info!(key = request.key, "status icon has no menu to show"),
            }
        }
    }
}

/// `<unique name><path>` → (unique name, path).
fn split_key(key: &str) -> Option<(&str, &str)> {
    let at = key.find('/')?;
    Some((&key[..at], &key[at..]))
}

async fn menu_path(conn: &Connection, bus: &str, path: &str) -> Option<String> {
    let proxy = zbus::Proxy::new(conn, bus.to_owned(), path.to_owned(), ITEM).await.ok()?;
    let menu: zbus::zvariant::OwnedObjectPath = proxy.get_property("Menu").await.ok()?;
    (menu.as_str() != "/").then(|| menu.to_string())
}

/// `ToolTip`: icon name, icon pixmaps, title, text.
type ToolTip = (String, Vec<(i32, i32, Vec<u8>)>, String, String);

/// A `GetLayout` node: id, properties, children.
type Layout = (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);

async fn read_menu(conn: &Connection, bus: &str, path: &str) -> Option<Vec<MenuEntry>> {
    let menu = menu_path(conn, bus, path).await?;
    let _ = tokio::time::timeout(CALL_TIMEOUT, conn.call_method(Some(bus), menu.as_str(), Some(MENU), "AboutToShow", &(0i32,))).await;
    let reply = tokio::time::timeout(CALL_TIMEOUT, conn.call_method(Some(bus), menu.as_str(), Some(MENU), "GetLayout", &(0i32, -1i32, Vec::<String>::new()))).await.ok()?.ok()?;
    let (_revision, (_id, _props, children)): (u32, Layout) = reply.body().deserialize().ok()?;
    Some(menu_entries(&children, 0))
}

/// Read an item's properties.
async fn read_item(proxy: &zbus::Proxy<'_>, key: &str) -> Item {
    async fn get<T: TryFrom<OwnedValue>>(proxy: &zbus::Proxy<'_>, name: &str) -> Option<T> {
        let value: OwnedValue = proxy.get_property(name).await.ok()?;
        T::try_from(value).ok()
    }
    let string = |v: Option<String>| v.unwrap_or_default();
    let theme_path = string(get(proxy, "IconThemePath").await);
    let icon = Icon {
        name: string(get(proxy, "IconName").await),
        theme_path: theme_path.clone(),
        pixmaps: pixmaps(get::<Vec<(i32, i32, Vec<u8>)>>(proxy, "IconPixmap").await.unwrap_or_default()),
    };
    let attention_icon = Icon {
        name: string(get(proxy, "AttentionIconName").await),
        theme_path,
        pixmaps: pixmaps(get::<Vec<(i32, i32, Vec<u8>)>>(proxy, "AttentionIconPixmap").await.unwrap_or_default()),
    };
    let overlay_icon = Icon {
        name: string(get(proxy, "OverlayIconName").await),
        theme_path: attention_icon.theme_path.clone(),
        pixmaps: pixmaps(get::<Vec<(i32, i32, Vec<u8>)>>(proxy, "OverlayIconPixmap").await.unwrap_or_default()),
    };
    let tooltip: Option<ToolTip> = get(proxy, "ToolTip").await;
    let (tooltip_title, tooltip_text) = tooltip.map(|(_, _, title, text)| (title, crate::markup::plain(&text))).unwrap_or_default();
    let menu: Option<zbus::zvariant::OwnedObjectPath> = get(proxy, "Menu").await;
    Item {
        key: key.to_owned(),
        title: string(get(proxy, "Title").await),
        status: get(proxy, "Status").await.unwrap_or_else(|| "Active".to_owned()),
        icon,
        attention_icon,
        overlay_icon,
        tooltip_title,
        tooltip_text,
        item_is_menu: get(proxy, "ItemIsMenu").await.unwrap_or(false),
        has_menu: menu.is_some_and(|m| m.as_str() != "/"),
    }
}

/// Follow item `key` until its connection leaves the bus.
async fn follow_item(tray: Tray, conn: Connection, key: String, emitter: SignalEmitter<'static>) {
    let Some((bus, path)) = split_key(&key) else { return };
    let (bus, path) = (bus.to_owned(), path.to_owned());
    let gone = async {
        let Ok(dbus) = zbus::fdo::DBusProxy::new(&conn).await else { return };
        let Ok(mut changes) = dbus.receive_name_owner_changed_with_args(&[(0, bus.as_str())]).await else { return };
        // It may have left before we listened.
        if dbus.get_name_owner(bus.as_str().try_into().expect("a unique name")).await.is_err() {
            return;
        }
        while let Some(change) = changes.next().await {
            if change.args().is_ok_and(|a| a.new_owner().is_none()) {
                return;
            }
        }
    };
    let watch = async {
        let builder = zbus::proxy::Builder::<zbus::Proxy<'_>>::new(&conn)
            .destination(bus.as_str())
            .and_then(|b| b.path(path.as_str()))
            .and_then(|b| b.interface(ITEM))
            .map(|b| b.cache_properties(zbus::proxy::CacheProperties::No));
        let Ok(builder) = builder else { return };
        let Ok(proxy) = builder.build().await else { return };
        tray.update(read_item(&proxy, &key).await);
        let Ok(mut signals) = proxy.receive_all_signals().await else { return };
        while let Some(signal) = signals.next().await {
            let member = signal.header().member().map(|m| m.to_string()).unwrap_or_default();
            if member.starts_with("New") {
                tray.update(read_item(&proxy, &key).await);
            }
        }
        std::future::pending::<()>().await;
    };
    tokio::select! {
        _ = gone => {}
        _ = watch => {}
    }
    tracing::info!(key, "status icon gone");
    tray.remove(&key);
    let _ = Watcher::status_notifier_item_unregistered(&emitter, &key).await;
}

/// `org.kde.StatusNotifierWatcher`.
pub struct Watcher {
    pub tray: Tray,
}

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl Watcher {
    async fn register_status_notifier_item(
        &self,
        service: String,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        let sender = header.sender().map(|s| s.to_string()).unwrap_or_default();
        let (bus, path) = if service.starts_with('/') {
            (sender, service.clone())
        } else {
            let dbus = zbus::fdo::DBusProxy::new(conn).await?;
            let name = zbus::names::BusName::try_from(service.as_str()).map_err(|e| zbus::fdo::Error::InvalidArgs(e.to_string()))?;
            let owner = dbus.get_name_owner(name).await.map_err(|_| zbus::fdo::Error::InvalidArgs(format!("{service} has no owner")))?;
            (owner.to_string(), "/StatusNotifierItem".to_owned())
        };
        zbus::zvariant::ObjectPath::try_from(path.as_str()).map_err(|e| zbus::fdo::Error::InvalidArgs(e.to_string()))?;
        let key = format!("{bus}{path}");
        {
            let mut list = self.tray.list();
            if list.iter().any(|i| i.key == key) {
                return Ok(());
            }
            if list.len() >= MAX_ITEMS {
                return Err(zbus::fdo::Error::LimitsExceeded("too many status icons".into()));
            }
            list.push(Item {
                key: key.clone(),
                title: String::new(),
                status: "Active".into(),
                icon: Icon::default(),
                attention_icon: Icon::default(),
                overlay_icon: Icon::default(),
                tooltip_title: String::new(),
                tooltip_text: String::new(),
                item_is_menu: false,
                has_menu: false,
            });
        }
        tracing::info!(key, service, "status icon registered");
        tokio::spawn(follow_item(self.tray.clone(), conn.clone(), key.clone(), emitter.to_owned()));
        let _ = Self::status_notifier_item_registered(&emitter, &key).await;
        Ok(())
    }

    fn register_status_notifier_host(&self, _service: String) {}

    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        self.tray.keys()
    }

    #[zbus(property)]
    fn is_status_notifier_host_registered(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn protocol_version(&self) -> i32 {
        0
    }

    #[zbus(signal)]
    async fn status_notifier_item_registered(emitter: &SignalEmitter<'_>, service: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_item_unregistered(emitter: &SignalEmitter<'_>, service: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_host_registered(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_host_unregistered(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_and_pixmaps() {
        assert_eq!(plain_label("_Open"), "Open");
        assert_eq!(plain_label("Save__As_x"), "Save_Asx");
        let argb = vec![0xff, 0x10, 0x20, 0x30];
        let p = pixmaps(vec![(1, 1, argb), (2, 2, vec![0; 3]), (300, 1, vec![0; 1200])]);
        assert_eq!(p, [(1, 1, vec![0x10, 0x20, 0x30, 0xff])]);
        let set = vec![(16, 16, vec![]), (32, 32, vec![]), (64, 64, vec![])];
        assert_eq!(best_pixmap(&set, 24).map(|p| p.0), Some(32));
        assert_eq!(best_pixmap(&set, 128).map(|p| p.0), Some(64));
        assert_eq!(split_key(":1.42/org/x"), Some((":1.42", "/org/x")));
        let mut item = Item {
            key: String::new(),
            title: "App".into(),
            status: "Active".into(),
            icon: Icon::default(),
            attention_icon: Icon::default(),
            overlay_icon: Icon::default(),
            tooltip_title: String::new(),
            tooltip_text: String::new(),
            item_is_menu: false,
            has_menu: false,
        };
        assert!(item.tooltip_lines().is_empty(), "the title alone is no tooltip");
        item.tooltip_text = "3 new\n\n a\nb\nc\nd\ne\nf\ng".to_owned();
        assert_eq!(item.tooltip_lines(), ["App", "3 new", "a", "b", "c", "d", "e"]);
        item.tooltip_title = "Mail".into();
        assert_eq!(item.tooltip_lines()[0], "Mail");
    }

    #[test]
    fn menus() {
        let v = |x: Value<'_>| OwnedValue::try_from(x).unwrap();
        let entry = |id: i32, props: Vec<(&str, OwnedValue)>, kids: Vec<OwnedValue>| {
            let props: HashMap<String, OwnedValue> = props.into_iter().map(|(k, v)| (k.to_owned(), v)).collect();
            v(Value::from(zbus::zvariant::StructureBuilder::new().add_field(id).add_field(props).add_field(kids.into_iter().map(Value::from).collect::<Vec<_>>()).build().unwrap()))
        };
        let children = vec![
            entry(2, vec![("label", v(Value::from("_Open")))], vec![]),
            entry(3, vec![("label", v(Value::from("DND"))), ("toggle-type", v(Value::from("checkmark"))), ("toggle-state", v(Value::from(1i32)))], vec![]),
            entry(4, vec![("type", v(Value::from("separator")))], vec![]),
            entry(5, vec![("label", v(Value::from("Hidden"))), ("visible", v(Value::from(false)))], vec![]),
            entry(6, vec![("label", v(Value::from("More"))), ("children-display", v(Value::from("submenu")))], vec![entry(7, vec![("label", v(Value::from("Inner"))), ("enabled", v(Value::from(false)))], vec![])]),
        ];
        let menu = menu_entries(&children, 0);
        assert_eq!(menu.len(), 4);
        assert_eq!(menu[0], MenuEntry::Item { id: 2, label: "Open".into(), enabled: true, check: None, radio_on: false, children: vec![] });
        assert!(matches!(&menu[1], MenuEntry::Item { check: Some(true), .. }));
        assert_eq!(menu[2], MenuEntry::Separator);
        let MenuEntry::Item { children, .. } = &menu[3] else { panic!() };
        assert!(matches!(&children[0], MenuEntry::Item { id: 7, enabled: false, .. }));
    }
}
