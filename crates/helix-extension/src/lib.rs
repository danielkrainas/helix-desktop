//! Write Helix extensions (helix-desktop-spec `contracts/extensions/`,
//! API 1.0).
//!
//! An extension is started by the Helix shell with a private D-Bus
//! connection to it. With this crate it exports its items, then calls
//! [`Extension::start`], then handles [`Event`]s:
//!
//! ```no_run
//! # async fn run() -> zbus::Result<()> {
//! use helix_extension::{Event, Extension, PanelItemState};
//! let mut ext = Extension::connect().await?;
//! let item = ext.panel_item("count", PanelItemState { label: "0".into(), ..Default::default() }).await?;
//! ext.start().await?;
//! let mut count = 0;
//! while let Some(event) = ext.next_event().await {
//!     if let Event::Activate { .. } = event {
//!         count += 1;
//!         item.update(|s| s.label = count.to_string()).await?;
//!     }
//! }
//! # Ok(()) }
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use futures_util::StreamExt;
use tokio::sync::{mpsc, Mutex};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::Connection;

/// The API version this crate speaks.
pub const API_VERSION: &str = "1.0";
const HOST_PATH: &str = "/org/helix/Extension1/Host";
const HOST: &str = "org.helix.Extension1.Host";

/// Something the user (or the shell) did.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A primary click on panel item `item` (its bottom-left corner).
    Activate { item: String, x: i32, y: i32 },
    /// A middle click.
    SecondaryActivate { item: String, x: i32, y: i32 },
    /// Wheel steps (120 each, negative: up or left).
    Scroll { item: String, delta: i32, horizontal: bool },
    /// The user flipped toggle `toggle` (show it by setting `active`).
    SetActive { toggle: String, active: bool },
    /// The user moved slider `slider`.
    SetValue { slider: String, value: f64 },
    /// Menu entry `id` of menu `menu` was chosen.
    MenuActivated { menu: String, id: i32 },
    /// A setting changed (from the extension, the user, or a tool).
    SettingChanged { key: String, value: OwnedValue },
}

/// A panel item's state (`org.helix.Extension1.PanelItem`).
#[derive(Debug, Clone, PartialEq)]
pub struct PanelItemState {
    /// An icon theme name, or a file in the extension's directory.
    pub icon: String,
    /// A PNG, preferred over `icon` when not empty.
    pub icon_data: Vec<u8>,
    pub label: String,
    pub tooltip: String,
    pub accessible_name: String,
    pub visible: bool,
    pub attention: bool,
    /// 0–1; below 0, no meter.
    pub meter: f64,
    /// Recent values, 0–1, oldest first.
    pub graph: Vec<f64>,
    /// The menu, by the name given to [`Extension::menu`].
    pub menu: Option<String>,
    /// Whether a primary click is for the extension; when false, it opens
    /// the menu.
    pub activatable: bool,
}

impl Default for PanelItemState {
    fn default() -> Self {
        Self {
            icon: String::new(),
            icon_data: Vec::new(),
            label: String::new(),
            tooltip: String::new(),
            accessible_name: String::new(),
            visible: true,
            attention: false,
            meter: -1.0,
            graph: Vec::new(),
            menu: None,
            activatable: true,
        }
    }
}

/// A quick settings toggle's state (`org.helix.Extension1.Toggle`).
#[derive(Debug, Clone, PartialEq)]
pub struct ToggleState {
    pub title: String,
    pub subtitle: String,
    pub icon: String,
    pub active: bool,
    pub sensitive: bool,
    /// A details menu, by name.
    pub details: Option<String>,
}

impl Default for ToggleState {
    fn default() -> Self {
        Self { title: String::new(), subtitle: String::new(), icon: String::new(), active: false, sensitive: true, details: None }
    }
}

/// A quick settings slider's state (`org.helix.Extension1.Slider`).
#[derive(Debug, Clone, PartialEq)]
pub struct SliderState {
    pub title: String,
    pub icon: String,
    /// 0–1.
    pub value: f64,
    pub sensitive: bool,
}

impl Default for SliderState {
    fn default() -> Self {
        Self { title: String::new(), icon: String::new(), value: 0.0, sensitive: true }
    }
}

/// What a menu entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuKind {
    Item,
    Check,
    Radio,
    Separator,
    Heading,
    Submenu,
}

impl MenuKind {
    fn name(self) -> &'static str {
        match self {
            MenuKind::Item => "item",
            MenuKind::Check => "check",
            MenuKind::Radio => "radio",
            MenuKind::Separator => "separator",
            MenuKind::Heading => "heading",
            MenuKind::Submenu => "submenu",
        }
    }
}

/// A menu entry. `parent` is 0 at the top level, else a submenu's id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuEntry {
    pub id: i32,
    pub parent: i32,
    pub kind: MenuKind,
    pub label: String,
    pub enabled: bool,
    pub checked: bool,
}

impl MenuEntry {
    /// An ordinary entry.
    pub fn item(id: i32, label: &str) -> Self {
        Self { id, parent: 0, kind: MenuKind::Item, label: label.to_owned(), enabled: true, checked: false }
    }

    /// A check box.
    pub fn check(id: i32, label: &str, checked: bool) -> Self {
        Self { kind: MenuKind::Check, checked, ..Self::item(id, label) }
    }

    pub fn separator(id: i32) -> Self {
        Self { kind: MenuKind::Separator, ..Self::item(id, "") }
    }
}

fn menu_path(name: &str) -> OwnedObjectPath {
    let safe: String = name.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    OwnedObjectPath::try_from(format!("/org/helix/Extension/menu/{safe}")).expect("a valid path")
}

/// An item's object path: `-` in its id becomes `_`.
fn item_path(kind: &str, id: &str) -> zbus::Result<OwnedObjectPath> {
    Ok(OwnedObjectPath::try_from(format!("/org/helix/Extension/{kind}/{}", id.replace('-', "_")))?)
}

fn optional_path(name: &Option<String>) -> OwnedObjectPath {
    name.as_deref().map(menu_path).unwrap_or_else(|| OwnedObjectPath::try_from("/").expect("root"))
}

type Events = mpsc::UnboundedSender<Event>;

struct PanelItemIface {
    id: String,
    state: PanelItemState,
    events: Events,
}

#[zbus::interface(name = "org.helix.Extension1.PanelItem")]
impl PanelItemIface {
    #[zbus(property)]
    fn icon(&self) -> String {
        self.state.icon.clone()
    }
    #[zbus(property)]
    fn icon_data(&self) -> Vec<u8> {
        self.state.icon_data.clone()
    }
    #[zbus(property)]
    fn label(&self) -> String {
        self.state.label.clone()
    }
    #[zbus(property)]
    fn tooltip(&self) -> String {
        self.state.tooltip.clone()
    }
    #[zbus(property)]
    fn accessible_name(&self) -> String {
        self.state.accessible_name.clone()
    }
    #[zbus(property)]
    fn visible(&self) -> bool {
        self.state.visible
    }
    #[zbus(property)]
    fn attention(&self) -> bool {
        self.state.attention
    }
    #[zbus(property)]
    fn meter(&self) -> f64 {
        self.state.meter
    }
    #[zbus(property)]
    fn graph(&self) -> Vec<f64> {
        self.state.graph.clone()
    }
    #[zbus(property)]
    fn menu(&self) -> OwnedObjectPath {
        optional_path(&self.state.menu)
    }
    fn activate(&self, x: i32, y: i32) -> zbus::fdo::Result<()> {
        if !self.state.activatable {
            return Err(zbus::fdo::Error::NotSupported("this item opens its menu".into()));
        }
        let _ = self.events.send(Event::Activate { item: self.id.clone(), x, y });
        Ok(())
    }
    fn secondary_activate(&self, x: i32, y: i32) {
        let _ = self.events.send(Event::SecondaryActivate { item: self.id.clone(), x, y });
    }
    fn scroll(&self, delta: i32, orientation: String) {
        let _ = self.events.send(Event::Scroll { item: self.id.clone(), delta, horizontal: orientation == "horizontal" });
    }
}

struct ToggleIface {
    id: String,
    state: ToggleState,
    events: Events,
}

#[zbus::interface(name = "org.helix.Extension1.Toggle")]
impl ToggleIface {
    #[zbus(property)]
    fn title(&self) -> String {
        self.state.title.clone()
    }
    #[zbus(property)]
    fn subtitle(&self) -> String {
        self.state.subtitle.clone()
    }
    #[zbus(property)]
    fn icon(&self) -> String {
        self.state.icon.clone()
    }
    #[zbus(property)]
    fn active(&self) -> bool {
        self.state.active
    }
    #[zbus(property)]
    fn sensitive(&self) -> bool {
        self.state.sensitive
    }
    #[zbus(property)]
    fn details(&self) -> OwnedObjectPath {
        optional_path(&self.state.details)
    }
    fn set_active(&self, active: bool) {
        let _ = self.events.send(Event::SetActive { toggle: self.id.clone(), active });
    }
}

struct SliderIface {
    id: String,
    state: SliderState,
    events: Events,
}

#[zbus::interface(name = "org.helix.Extension1.Slider")]
impl SliderIface {
    #[zbus(property)]
    fn title(&self) -> String {
        self.state.title.clone()
    }
    #[zbus(property)]
    fn icon(&self) -> String {
        self.state.icon.clone()
    }
    #[zbus(property)]
    fn value(&self) -> f64 {
        self.state.value
    }
    #[zbus(property)]
    fn sensitive(&self) -> bool {
        self.state.sensitive
    }
    fn set_value(&self, value: f64) {
        let _ = self.events.send(Event::SetValue { slider: self.id.clone(), value: value.clamp(0.0, 1.0) });
    }
}

struct MenuIface {
    name: String,
    entries: Vec<MenuEntry>,
    events: Events,
}

#[zbus::interface(name = "org.helix.Extension1.Menu")]
impl MenuIface {
    #[zbus(property)]
    fn layout(&self) -> Vec<(i32, i32, String, String, bool, bool)> {
        self.entries.iter().map(|e| (e.id, e.parent, e.kind.name().to_owned(), e.label.clone(), e.enabled, e.checked)).collect()
    }
    fn activated(&self, id: i32) {
        let _ = self.events.send(Event::MenuActivated { menu: self.name.clone(), id });
    }
}

/// A panel item the extension shows.
#[derive(Clone)]
pub struct PanelItem {
    conn: Connection,
    path: OwnedObjectPath,
}

impl PanelItem {
    /// Change its state; the shell is told what changed.
    pub async fn update(&self, change: impl FnOnce(&mut PanelItemState)) -> zbus::Result<()> {
        let iface = self.conn.object_server().interface::<_, PanelItemIface>(&self.path).await?;
        let mut guard = iface.get_mut().await;
        let old = guard.state.clone();
        change(&mut guard.state);
        let new = guard.state.clone();
        let e = iface.signal_emitter();
        if old.icon != new.icon {
            guard.icon_changed(e).await?;
        }
        if old.icon_data != new.icon_data {
            guard.icon_data_changed(e).await?;
        }
        if old.label != new.label {
            guard.label_changed(e).await?;
        }
        if old.tooltip != new.tooltip {
            guard.tooltip_changed(e).await?;
        }
        if old.accessible_name != new.accessible_name {
            guard.accessible_name_changed(e).await?;
        }
        if old.visible != new.visible {
            guard.visible_changed(e).await?;
        }
        if old.attention != new.attention {
            guard.attention_changed(e).await?;
        }
        if old.meter != new.meter {
            guard.meter_changed(e).await?;
        }
        if old.graph != new.graph {
            guard.graph_changed(e).await?;
        }
        if old.menu != new.menu {
            guard.menu_changed(e).await?;
        }
        Ok(())
    }
}

/// A quick settings toggle.
#[derive(Clone)]
pub struct Toggle {
    conn: Connection,
    path: OwnedObjectPath,
}

impl Toggle {
    pub async fn update(&self, change: impl FnOnce(&mut ToggleState)) -> zbus::Result<()> {
        let iface = self.conn.object_server().interface::<_, ToggleIface>(&self.path).await?;
        let mut guard = iface.get_mut().await;
        let old = guard.state.clone();
        change(&mut guard.state);
        let new = guard.state.clone();
        let e = iface.signal_emitter();
        if old.title != new.title {
            guard.title_changed(e).await?;
        }
        if old.subtitle != new.subtitle {
            guard.subtitle_changed(e).await?;
        }
        if old.icon != new.icon {
            guard.icon_changed(e).await?;
        }
        if old.active != new.active {
            guard.active_changed(e).await?;
        }
        if old.sensitive != new.sensitive {
            guard.sensitive_changed(e).await?;
        }
        if old.details != new.details {
            guard.details_changed(e).await?;
        }
        Ok(())
    }
}

/// A quick settings slider.
#[derive(Clone)]
pub struct Slider {
    conn: Connection,
    path: OwnedObjectPath,
}

impl Slider {
    pub async fn update(&self, change: impl FnOnce(&mut SliderState)) -> zbus::Result<()> {
        let iface = self.conn.object_server().interface::<_, SliderIface>(&self.path).await?;
        let mut guard = iface.get_mut().await;
        let old = guard.state.clone();
        change(&mut guard.state);
        let new = guard.state.clone();
        let e = iface.signal_emitter();
        if old.title != new.title {
            guard.title_changed(e).await?;
        }
        if old.icon != new.icon {
            guard.icon_changed(e).await?;
        }
        if old.value != new.value {
            guard.value_changed(e).await?;
        }
        if old.sensitive != new.sensitive {
            guard.sensitive_changed(e).await?;
        }
        Ok(())
    }
}

/// A menu, for panel items (`menu`) and toggles (`details`).
#[derive(Clone)]
pub struct Menu {
    conn: Connection,
    path: OwnedObjectPath,
}

impl Menu {
    pub async fn set(&self, entries: Vec<MenuEntry>) -> zbus::Result<()> {
        let iface = self.conn.object_server().interface::<_, MenuIface>(&self.path).await?;
        let mut guard = iface.get_mut().await;
        if guard.entries != entries {
            guard.entries = entries;
            guard.layout_changed(iface.signal_emitter()).await?;
        }
        Ok(())
    }
}

/// What `Hello` answered.
#[derive(Debug, Clone, Default)]
pub struct Granted {
    pub api_version: String,
    pub permissions: Vec<String>,
    pub helix_version: String,
    pub locale: String,
}

/// The extension's connection to the shell.
pub struct Extension {
    conn: Connection,
    id: String,
    events_tx: Events,
    events: mpsc::UnboundedReceiver<Event>,
    granted: Arc<Mutex<Granted>>,
}

impl Extension {
    /// Connect to the shell that started this process
    /// (`HELIX_EXTENSION_FD`).
    pub async fn connect() -> zbus::Result<Self> {
        use std::os::fd::FromRawFd;
        let fd: i32 = std::env::var("HELIX_EXTENSION_FD")
            .ok()
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| zbus::Error::Failure("not started by Helix (no HELIX_EXTENSION_FD)".into()))?;
        // SAFETY: the shell hands this descriptor to the process for this.
        let stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };
        stream.set_nonblocking(true)?;
        let stream = tokio::net::UnixStream::from_std(stream)?;
        Self::connect_with(stream, std::env::var("HELIX_EXTENSION_ID").unwrap_or_default()).await
    }

    /// Connect over `stream` (tests).
    pub async fn connect_with(stream: tokio::net::UnixStream, id: String) -> zbus::Result<Self> {
        let conn = zbus::connection::Builder::unix_stream(stream).p2p().build().await?;
        let (events_tx, events) = mpsc::unbounded_channel();
        // The host's signals.
        let mut messages = zbus::MessageStream::from(&conn);
        let tx = events_tx.clone();
        tokio::spawn(async move {
            while let Some(Ok(msg)) = messages.next().await {
                let header = msg.header();
                if header.message_type() != zbus::message::Type::Signal || header.member().is_none_or(|m| m.as_str() != "SettingChanged") {
                    continue;
                }
                if let Ok((key, value)) = msg.body().deserialize::<(String, OwnedValue)>() {
                    let _ = tx.send(Event::SettingChanged { key, value });
                }
            }
        });
        Ok(Self { conn, id, events_tx, events, granted: Default::default() })
    }

    /// The extension's id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Export panel item `id` (declared in the manifest).
    pub async fn panel_item(&self, id: &str, state: PanelItemState) -> zbus::Result<PanelItem> {
        let path = item_path("panel_item", id)?;
        self.conn.object_server().at(&path, PanelItemIface { id: id.to_owned(), state, events: self.events_tx.clone() }).await?;
        Ok(PanelItem { conn: self.conn.clone(), path })
    }

    /// Export toggle `id`.
    pub async fn toggle(&self, id: &str, state: ToggleState) -> zbus::Result<Toggle> {
        let path = item_path("toggle", id)?;
        self.conn.object_server().at(&path, ToggleIface { id: id.to_owned(), state, events: self.events_tx.clone() }).await?;
        Ok(Toggle { conn: self.conn.clone(), path })
    }

    /// Export slider `id`.
    pub async fn slider(&self, id: &str, state: SliderState) -> zbus::Result<Slider> {
        let path = item_path("slider", id)?;
        self.conn.object_server().at(&path, SliderIface { id: id.to_owned(), state, events: self.events_tx.clone() }).await?;
        Ok(Slider { conn: self.conn.clone(), path })
    }

    /// Export a menu named `name` (use the name in `menu` / `details`).
    pub async fn menu(&self, name: &str, entries: Vec<MenuEntry>) -> zbus::Result<Menu> {
        let path = menu_path(name);
        self.conn.object_server().at(&path, MenuIface { name: name.to_owned(), entries, events: self.events_tx.clone() }).await?;
        Ok(Menu { conn: self.conn.clone(), path })
    }

    async fn call<B, R>(&self, method: &str, body: &B) -> zbus::Result<R>
    where
        B: serde::ser::Serialize + zbus::zvariant::DynamicType,
        R: for<'d> zbus::zvariant::DynamicDeserialize<'d>,
    {
        let reply = self.conn.call_method(None::<&str>, HOST_PATH, Some(HOST), method, body).await?;
        reply.body().deserialize()
    }

    /// Say hello: after this the shell shows the exported items. Call it
    /// once every item is exported.
    pub async fn start(&self) -> zbus::Result<Granted> {
        let (api_version, permissions, info): (String, Vec<String>, HashMap<String, OwnedValue>) = self.call("Hello", &(API_VERSION,)).await?;
        let text = |k: &str| info.get(k).and_then(|v| String::try_from(v.clone()).ok()).unwrap_or_default();
        let granted = Granted { api_version, permissions, helix_version: text("helix-version"), locale: text("locale") };
        *self.granted.lock().await = granted.clone();
        Ok(granted)
    }

    /// The next event; `None` once the shell is gone.
    pub async fn next_event(&mut self) -> Option<Event> {
        tokio::select! {
            event = self.events.recv() => event,
            _ = closed(&self.conn) => None,
        }
    }

    /// A setting's value (the default when not set).
    pub async fn setting(&self, key: &str) -> zbus::Result<OwnedValue> {
        let (value,): (OwnedValue,) = self.call("GetSetting", &(key,)).await?;
        Ok(value)
    }

    pub async fn set_setting(&self, key: &str, value: Value<'_>) -> zbus::Result<()> {
        self.call::<_, ()>("SetSetting", &(key, value)).await
    }

    /// Level 0 error, 1 warning, 2 info, 3 debug.
    pub async fn log(&self, level: u32, message: &str) -> zbus::Result<()> {
        self.call::<_, ()>("Log", &(level, message)).await
    }

    /// Needs the `notifications` permission.
    pub async fn show_notice(&self, title: &str, body: &str, icon: &str) -> zbus::Result<()> {
        self.call::<_, ()>("ShowNotice", &(title, body, icon)).await
    }

    /// Needs `inhibit-idle`.
    pub async fn inhibit_idle(&self, reason: &str) -> zbus::Result<u32> {
        let (handle,): (u32,) = self.call("InhibitIdle", &(reason,)).await?;
        Ok(handle)
    }

    pub async fn uninhibit(&self, handle: u32) -> zbus::Result<()> {
        self.call::<_, ()>("Uninhibit", &(handle,)).await
    }

    /// Needs `launch-apps`.
    pub async fn launch(&self, desktop_id: &str, uris: &[&str]) -> zbus::Result<()> {
        self.call::<_, ()>("Launch", &(desktop_id, uris)).await
    }

    /// The connection, for anything else.
    pub fn connection(&self) -> &Connection {
        &self.conn
    }
}

/// Resolves when the connection to the shell is gone.
async fn closed(conn: &Connection) {
    let mut messages = zbus::MessageStream::from(conn);
    while let Some(Ok(_)) = messages.next().await {}
}
