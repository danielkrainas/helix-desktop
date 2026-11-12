//! IBus, for `ibus` input sources (helix-desktop-spec
//! `contracts/gsettings/input-settings.md`, "Input methods", and
//! `contracts/dbus/org.freedesktop.IBus.md`): one input context on IBus's
//! own bus. The UI thread (the Wayland input method) sends [`ImRequest`]s
//! and gets [`ImEvent`]s back.

use std::path::PathBuf;
use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::mpsc;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Structure, Value};
use zbus::Connection;

use crate::UiEvent;

const BUS_NAME: &str = "org.freedesktop.IBus";
const PATH: &str = "/org/freedesktop/IBus";
const IFACE: &str = "org.freedesktop.IBus";
const IC_IFACE: &str = "org.freedesktop.IBus.InputContext";
/// Pre-edit text, lookup table, focus, properties, surrounding text.
pub const CAPABILITIES: u32 = 1 | 4 | 8 | 16 | 32;
/// `state` bit of a key release.
pub const RELEASE_MASK: u32 = 1 << 30;

/// From the input method to IBus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImRequest {
    /// The engine of the active `ibus` source (`None`: an `xkb` source).
    Engine(Option<String>),
    FocusIn,
    FocusOut,
    /// A key while a text field has focus; answered with
    /// [`ImEvent::KeyHandled`] carrying `seq`.
    Key { seq: u64, keyval: u32, keycode: u32, state: u32 },
    /// The text cursor, in screen coordinates.
    Cursor { x: i32, y: i32, width: i32, height: i32 },
    /// An engine property was chosen (`state`: 1 checked, 0 not).
    PropertyActivate { key: String, state: u32 },
}

/// An engine property's kind (`IBusPropType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropKind {
    Normal,
    Toggle,
    Radio,
    Menu,
    Separator,
}

/// An engine property (`IBusProperty`): an input mode, a switch, a menu.
#[derive(Debug, Clone, PartialEq)]
pub struct Prop {
    pub key: String,
    pub kind: PropKind,
    pub label: String,
    /// `IBusPropState`: 1 checked.
    pub state: u32,
    pub sensitive: bool,
    pub visible: bool,
    /// A short text for the panel (e.g. the input mode's "あ").
    pub symbol: String,
    pub sub: Vec<Prop>,
}

/// A serialized `IBusProperty`.
pub fn prop(value: &Value<'_>) -> Option<Prop> {
    let Value::Structure(s) = unwrap_variant(value) else { return None };
    let f = s.fields();
    if !matches!(f.first(), Some(Value::Str(n)) if n.as_str() == "IBusProperty") || f.len() < 11 {
        return None;
    }
    let text = |v: &Value<'_>| ibus_text(v).unwrap_or_default();
    let string = |v: &Value<'_>| match unwrap_variant(v) {
        Value::Str(s) => s.to_string(),
        _ => String::new(),
    };
    let number = |v: &Value<'_>| match unwrap_variant(v) {
        Value::U32(n) => *n,
        _ => 0,
    };
    let flag = |v: &Value<'_>| matches!(unwrap_variant(v), Value::Bool(true));
    Some(Prop {
        key: string(&f[2]),
        kind: match number(&f[3]) {
            1 => PropKind::Toggle,
            2 => PropKind::Radio,
            3 => PropKind::Menu,
            4 => PropKind::Separator,
            _ => PropKind::Normal,
        },
        label: text(&f[4]),
        sensitive: flag(&f[7]),
        visible: flag(&f[8]),
        state: number(&f[9]),
        sub: prop_list(&f[10]),
        symbol: f.get(11).map(text).unwrap_or_default(),
    })
}

/// A serialized `IBusPropList`.
pub fn prop_list(value: &Value<'_>) -> Vec<Prop> {
    let Value::Structure(s) = unwrap_variant(value) else { return Vec::new() };
    let f = s.fields();
    if !matches!(f.first(), Some(Value::Str(n)) if n.as_str() == "IBusPropList") {
        return Vec::new();
    }
    match f.get(2).map(unwrap_variant) {
        Some(Value::Array(items)) => items.iter().filter_map(prop).collect(),
        _ => Vec::new(),
    }
}

/// Replace the property with `new`'s key (at any depth); false if none.
pub fn update_prop(list: &mut [Prop], new: &Prop) -> bool {
    for p in list.iter_mut() {
        if p.key == new.key {
            // An update keeps the sub-properties it does not carry.
            let sub = if new.sub.is_empty() { std::mem::take(&mut p.sub) } else { new.sub.clone() };
            *p = Prop { sub, ..new.clone() };
            return true;
        }
        if update_prop(&mut p.sub, new) {
            return true;
        }
    }
    false
}

/// From IBus to the input method.
#[derive(Debug, Clone, PartialEq)]
pub enum ImEvent {
    KeyHandled { seq: u64, handled: bool },
    Commit(String),
    /// Pre-edit text and the cursor in it (characters); empty hides it.
    Preedit { text: String, cursor: u32 },
    /// Candidates and the selected one; `None` hides them.
    Candidates(Option<(Vec<String>, usize)>),
    /// Deliver this key to the app (`keycode` evdev).
    Forward { keycode: u32, pressed: bool },
    /// The engine's properties (all of them).
    Properties(Vec<Prop>),
    /// One property changed.
    Property(Prop),
}

/// The value inside any number of variant wrappers.
fn unwrap_variant<'a>(mut value: &'a Value<'a>) -> &'a Value<'a> {
    while let Value::Value(inner) = value {
        value = inner.as_ref();
    }
    value
}

/// The text of an `IBusText` value.
pub fn ibus_text(value: &Value<'_>) -> Option<String> {
    let value = unwrap_variant(value);
    let Value::Structure(s) = value else { return None };
    match s.fields() {
        [Value::Str(kind), _, Value::Str(text), ..] if kind.as_str() == "IBusText" => Some(text.to_string()),
        _ => None,
    }
}

/// Candidates and cursor position of an `IBusLookupTable` value, the
/// current page only.
pub fn lookup_table(value: &Value<'_>) -> Option<(Vec<String>, usize)> {
    let value = unwrap_variant(value);
    let Value::Structure(s) = value else { return None };
    let fields = s.fields();
    let (Some(Value::Str(kind)), Some(Value::U32(page_size)), Some(Value::U32(cursor)), Some(Value::Array(candidates))) =
        (fields.first(), fields.get(2), fields.get(3), fields.get(7))
    else {
        return None;
    };
    if kind.as_str() != "IBusLookupTable" {
        return None;
    }
    let all: Vec<String> = candidates.iter().filter_map(ibus_text).collect();
    let page_size = (*page_size).max(1) as usize;
    let page = *cursor as usize / page_size;
    let shown: Vec<String> = all.iter().skip(page * page_size).take(page_size).cloned().collect();
    Some((shown, *cursor as usize % page_size))
}

/// An `IBusText` value for `SetSurroundingText`.
pub fn make_text(text: &str) -> Value<'static> {
    let attrs = Structure::from(("IBusAttrList".to_owned(), std::collections::HashMap::<String, Value<'static>>::new(), Vec::<Value<'static>>::new()));
    Value::Value(Box::new(Value::Structure(Structure::from((
        "IBusText".to_owned(),
        std::collections::HashMap::<String, Value<'static>>::new(),
        text.to_owned(),
        Value::Value(Box::new(Value::Structure(attrs))),
    )))))
}

/// Where Helix's own ibus-daemon listens.
fn private_address() -> (PathBuf, String) {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let socket = runtime.join("helix").join("ibus");
    let address = format!("unix:path={}", socket.display());
    (socket, address)
}

/// The IBus side of the input method.
pub struct Ibus {
    ui: mpsc::UnboundedSender<UiEvent>,
    conn: Option<Connection>,
    context: Option<OwnedObjectPath>,
    /// Start a private daemon when there is none (off in tests).
    start_daemon: bool,
    engine: Option<String>,
}

impl Ibus {
    pub fn new(ui: mpsc::UnboundedSender<UiEvent>) -> Self {
        Self { ui, conn: None, context: None, start_daemon: true, engine: None }
    }

    /// Use an existing connection to an IBus (tests).
    pub fn with_connection(ui: mpsc::UnboundedSender<UiEvent>, conn: Connection) -> Self {
        Self { ui, conn: Some(conn), context: None, start_daemon: false, engine: None }
    }

    fn send(&self, event: ImEvent) {
        let _ = self.ui.send(UiEvent::Ime(event));
    }

    async fn connect(&mut self) -> zbus::Result<Connection> {
        if let Some(conn) = &self.conn {
            return Ok(conn.clone());
        }
        let (socket, address) = private_address();
        let try_connect = || async { zbus::connection::Builder::address(address.as_str())?.build().await };
        if let Ok(conn) = try_connect().await {
            self.conn = Some(conn.clone());
            return Ok(conn);
        }
        if !self.start_daemon {
            return Err(zbus::Error::Failure("no IBus".into()));
        }
        if let Some(dir) = socket.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::remove_file(&socket);
        tracing::info!(address, "starting ibus-daemon");
        std::process::Command::new("ibus-daemon")
            .args(["--panel", "disable", "--address", &address])
            .spawn()
            .map_err(|e| zbus::Error::Failure(format!("cannot start ibus-daemon: {e}")))?;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if let Ok(conn) = try_connect().await {
                self.conn = Some(conn.clone());
                return Ok(conn);
            }
        }
        Err(zbus::Error::Failure("ibus-daemon did not come up".into()))
    }

    /// The input context, made (and followed) on first use.
    async fn context(&mut self) -> zbus::Result<(Connection, OwnedObjectPath)> {
        let conn = self.connect().await?;
        if let Some(path) = &self.context {
            return Ok((conn, path.clone()));
        }
        let reply = conn.call_method(Some(BUS_NAME), PATH, Some(IFACE), "CreateInputContext", &("helix-shell",)).await?;
        let path: OwnedObjectPath = reply.body().deserialize()?;
        conn.call_method(Some(BUS_NAME), path.as_str(), Some(IC_IFACE), "SetCapabilities", &(CAPABILITIES,)).await?;
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface(IC_IFACE)?
            .path(path.clone())?
            .build();
        let mut signals = zbus::MessageStream::for_match_rule(rule, &conn, None).await?;
        let ui = self.ui.clone();
        tokio::spawn(async move {
            while let Some(Ok(message)) = signals.next().await {
                let Some(member) = message.header().member().map(|m| m.to_string()) else { continue };
                let body = message.body();
                let event = match member.as_str() {
                    "CommitText" => body.deserialize::<OwnedValue>().ok().and_then(|v| ibus_text(&v)).map(ImEvent::Commit),
                    "UpdatePreeditText" => body.deserialize::<(OwnedValue, u32, bool)>().ok().map(|(v, cursor, visible)| {
                        let text = if visible { ibus_text(&v).unwrap_or_default() } else { String::new() };
                        ImEvent::Preedit { text, cursor }
                    }),
                    "HidePreeditText" => Some(ImEvent::Preedit { text: String::new(), cursor: 0 }),
                    "UpdateLookupTable" => body
                        .deserialize::<(OwnedValue, bool)>()
                        .ok()
                        .map(|(v, visible)| ImEvent::Candidates(if visible { lookup_table(&v) } else { None })),
                    "HideLookupTable" => Some(ImEvent::Candidates(None)),
                    "RegisterProperties" => body.deserialize::<OwnedValue>().ok().map(|v| ImEvent::Properties(prop_list(&v))),
                    "UpdateProperty" => body.deserialize::<OwnedValue>().ok().and_then(|v| prop(&v)).map(ImEvent::Property),
                    "ForwardKeyEvent" => body
                        .deserialize::<(u32, u32, u32)>()
                        .ok()
                        .map(|(_, keycode, state)| ImEvent::Forward { keycode, pressed: state & RELEASE_MASK == 0 }),
                    _ => None,
                };
                if let Some(event) = event {
                    let _ = ui.send(UiEvent::Ime(event));
                }
            }
        });
        tracing::info!(%path, "IBus input context");
        self.context = Some(path.clone());
        Ok((conn, path))
    }

    async fn call<B>(&mut self, method: &str, body: &B) -> zbus::Result<zbus::Message>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        let (conn, path) = self.context().await?;
        conn.call_method(Some(BUS_NAME), path.as_str(), Some(IC_IFACE), method, body).await
    }

    pub async fn handle(&mut self, request: ImRequest) {
        let result = match request {
            ImRequest::Engine(engine) => {
                if engine == self.engine {
                    return;
                }
                self.engine = engine.clone();
                // The new engine registers its own properties.
                self.send(ImEvent::Properties(Vec::new()));
                match engine {
                    Some(name) => match self.connect().await {
                        Ok(conn) => {
                            tracing::info!(name, "IBus engine");
                            conn.call_method(Some(BUS_NAME), PATH, Some(IFACE), "SetGlobalEngine", &(name.as_str(),)).await.map(drop)
                        }
                        Err(err) => Err(err),
                    },
                    None => Ok(()),
                }
            }
            ImRequest::FocusIn if self.engine.is_some() => self.call("FocusIn", &()).await.map(drop),
            ImRequest::FocusOut if self.engine.is_some() => {
                let _ = self.call("Reset", &()).await;
                self.call("FocusOut", &()).await.map(drop)
            }
            ImRequest::PropertyActivate { key, state } if self.engine.is_some() => self.call("PropertyActivate", &(key.as_str(), state)).await.map(drop),
            ImRequest::Cursor { x, y, width, height } if self.engine.is_some() => {
                self.call("SetCursorLocation", &(x, y, width, height)).await.map(drop)
            }
            ImRequest::Key { seq, keyval, keycode, state } => {
                let handled = if self.engine.is_some() {
                    match self.call("ProcessKeyEvent", &(keyval, keycode, state)).await {
                        Ok(reply) => reply.body().deserialize::<bool>().unwrap_or(false),
                        Err(err) => {
                            tracing::warn!(%err, "IBus did not take the key");
                            false
                        }
                    }
                } else {
                    false
                };
                self.send(ImEvent::KeyHandled { seq, handled });
                Ok(())
            }
            _ => Ok(()),
        };
        if let Err(err) = result {
            tracing::warn!(%err, "IBus request failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Value<'static> {
        make_text(s)
    }

    #[test]
    fn texts_and_tables() {
        assert_eq!(ibus_text(&text("你好")).as_deref(), Some("你好"));
        assert_eq!(ibus_text(&Value::from("plain")), None);
        let candidates: Vec<Value<'static>> = ["一", "二", "三", "四", "五"].iter().map(|s| text(s)).collect();
        let table = Value::Structure(Structure::from((
            "IBusLookupTable".to_owned(),
            std::collections::HashMap::<String, Value<'static>>::new(),
            3u32,
            4u32,
            true,
            false,
            0i32,
            candidates,
            Vec::<Value<'static>>::new(),
        )));
        assert_eq!(lookup_table(&table), Some((vec!["四".to_owned(), "五".to_owned()], 1)), "second page, second entry");
    }
}
