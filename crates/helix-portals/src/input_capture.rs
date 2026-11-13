//! The InputCapture portal backend (helix-desktop-spec
//! `org.freedesktop.impl.portal.md`, "InputCapture"): asks the user, then
//! runs the session through the compositor's `org.helix.InputCapture`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedFd, OwnedObjectPath, OwnedValue, Value};
use zbus::{fdo, interface, Connection, ObjectServer};

use crate::dialogs::{app_name, dialog, value};
use crate::screenshot::{response, with_request, Results};

const COMPOSITOR: &str = "org.helix.InputCapture";
const COMPOSITOR_PATH: &str = "/org/helix/InputCapture";
const SESSION_IFACE: &str = "org.helix.InputCapture.Session";
/// Keyboard and pointer (touchscreens are not captured).
pub const CAPABILITIES: u32 = 1 | 2;

/// Portal session → the compositor's session.
pub type Sessions = Arc<Mutex<HashMap<OwnedObjectPath, OwnedObjectPath>>>;

fn lock(s: &Sessions) -> std::sync::MutexGuard<'_, HashMap<OwnedObjectPath, OwnedObjectPath>> {
    s.lock().unwrap_or_else(|e| e.into_inner())
}

/// The dialog's message, naming the key that ends a capture.
pub fn message(app: &str, cancel: &str) -> String {
    format!("{app} wants to take over the pointer and keyboard when the pointer leaves the screen. {cancel} gives them back.")
}

/// An accelerator as people write it: `<Super><Shift>Escape` →
/// `Super+Shift+Escape` (`<Primary>`/`<Control>` → `Ctrl`).
pub fn accelerator_label(accel: &str) -> String {
    let mut parts = Vec::new();
    let mut rest = accel.trim();
    while let Some(after) = rest.strip_prefix('<') {
        let Some(end) = after.find('>') else { break };
        parts.push(match after[..end].to_ascii_lowercase().as_str() {
            "primary" | "control" | "ctrl" => "Ctrl".to_owned(),
            "super" | "mod4" => "Super".to_owned(),
            "alt" | "mod1" => "Alt".to_owned(),
            "shift" => "Shift".to_owned(),
            other => other.to_owned(),
        });
        rest = &after[end + 1..];
    }
    parts.push(rest.to_owned());
    parts.join("+")
}

/// `org.gnome.mutter.keybindings cancel-input-capture`'s first
/// accelerator, for people (its default without the key).
fn cancel_label() -> String {
    use gio::prelude::*;
    const SCHEMA: &str = "org.gnome.mutter.keybindings";
    let configured = gio::SettingsSchemaSource::default()
        .and_then(|src| src.lookup(SCHEMA, true))
        .filter(|s| s.has_key("cancel-input-capture"))
        .and_then(|_| gio::Settings::new(SCHEMA).strv("cancel-input-capture").first().map(|a| a.to_string()));
    accelerator_label(configured.as_deref().unwrap_or("<Super><Shift>Escape"))
}

/// The barriers of `SetPointerBarriers` (id and position), skipping
/// entries without both.
pub fn barriers_of(list: &[HashMap<String, OwnedValue>]) -> Vec<(u32, (i32, i32, i32, i32))> {
    list.iter()
        .filter_map(|b| {
            let id = u32::try_from(b.get("barrier_id")?.try_clone().ok()?).ok()?;
            let position = <(i32, i32, i32, i32)>::try_from(b.get("position")?.try_clone().ok()?).ok()?;
            Some((id, position))
        })
        .collect()
}

pub struct InputCaptureBackend {
    pub conn: Connection,
    pub sessions: Sessions,
    /// Sessions made with `CreateSession2`, not started yet.
    pub pending: Pending,
}

pub type Pending = Arc<Mutex<std::collections::HashSet<OwnedObjectPath>>>;

fn lock_pending(p: &Pending) -> std::sync::MutexGuard<'_, std::collections::HashSet<OwnedObjectPath>> {
    p.lock().unwrap_or_else(|e| e.into_inner())
}

/// Clipboard sharing of capture sessions: those that asked for it
/// (`RequestClipboard` before starting) and those sharing it (portal
/// session → compositor session).
#[derive(Default)]
struct Clipboards {
    requested: std::collections::HashSet<OwnedObjectPath>,
    shared: HashMap<OwnedObjectPath, OwnedObjectPath>,
}

static CLIPBOARDS: std::sync::LazyLock<Mutex<Clipboards>> = std::sync::LazyLock::new(Default::default);

fn clipboards() -> std::sync::MutexGuard<'static, Clipboards> {
    CLIPBOARDS.lock().unwrap_or_else(|e| e.into_inner())
}

/// `RequestClipboard` for portal session `portal`, if it is a capture
/// session: `None` when it is not one.
pub(crate) fn request_clipboard(sessions: &Sessions, pending: &Pending, portal: &OwnedObjectPath) -> Option<fdo::Result<()>> {
    if lock_pending(pending).contains(portal) {
        clipboards().requested.insert(portal.clone());
        return Some(Ok(()));
    }
    lock(sessions).contains_key(portal).then(|| Err(fdo::Error::Failed("the session is already started".into())))
}

/// The compositor session of capture session `portal`, when it shares
/// the clipboard; `None` when it is not a capture session.
pub(crate) fn clipboard_session(sessions: &Sessions, portal: &OwnedObjectPath) -> Option<fdo::Result<OwnedObjectPath>> {
    if !lock(sessions).contains_key(portal) {
        return None;
    }
    Some(clipboards().shared.get(portal).cloned().ok_or_else(|| fdo::Error::Failed("the clipboard is not shared".into())))
}

/// The bus name and interface of the compositor's capture sessions.
pub(crate) const CLIPBOARD_TARGET: (&str, &str) = (COMPOSITOR, SESSION_IFACE);

/// Whether the user must be asked: unless `restored` (the capabilities
/// allowed before) covers what is wanted.
pub fn must_ask(wanted: u32, restored: Option<u32>) -> bool {
    restored.is_none_or(|r| wanted & !r != 0)
}

/// Start the portal session `portal`: ask (unless restored), make the
/// compositor's session and follow it.
async fn start(conn: &Connection, sessions: &Sessions, portal: &OwnedObjectPath, app_id: &str, wanted: u32, restored: Option<u32>, persist: u32) -> (u32, Results) {
    if wanted == 0 {
        return (response::OTHER, Results::new());
    }
    if must_ask(wanted, restored) {
        let who = if app_id.is_empty() { "An app".to_owned() } else { app_name(app_id) };
        let asked = HashMap::from([
            ("title".to_owned(), value(Value::from("Allow Input Capture?"))),
            ("message".to_owned(), value(Value::from(message(&who, &cancel_label())))),
            ("accept".to_owned(), value(Value::from("Allow"))),
            ("cancel".to_owned(), value(Value::from("Deny"))),
        ]);
        if dialog(conn, asked).await.is_none() {
            return (response::CANCELLED, Results::new());
        }
    }
    let created = conn.call_method(Some(COMPOSITOR), COMPOSITOR_PATH, Some(COMPOSITOR), "CreateSession", &(wanted,)).await;
    let compositor: OwnedObjectPath = match created.and_then(|r| r.body().deserialize()) {
        Ok(path) => path,
        Err(err) => {
            tracing::warn!(%err, "the compositor cannot capture input");
            return (response::OTHER, Results::new());
        }
    };
    lock(sessions).insert(portal.clone(), compositor.clone());
    // Follow its signals from before the clipboard is turned on (which may
    // say at once what the clipboard holds).
    let rule = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).interface(SESSION_IFACE).and_then(|r| r.path(compositor.clone())).map(|r| r.build());
    let signals = match rule {
        Ok(rule) => zbus::MessageStream::for_match_rule(rule, conn, None).await.ok(),
        Err(_) => None,
    };
    let clipboard = clipboards().requested.remove(portal) && signals.is_some() && {
        let none: HashMap<&str, Value<'_>> = HashMap::new();
        match conn.call_method(Some(COMPOSITOR), compositor.as_str(), Some(SESSION_IFACE), "EnableClipboard", &(none,)).await {
            Ok(_) => {
                clipboards().shared.insert(portal.clone(), compositor.clone());
                true
            }
            Err(err) => {
                tracing::warn!(%err, "cannot share the clipboard with input capture");
                false
            }
        }
    };
    if let Some(signals) = signals {
        tokio::spawn(follow(conn.clone(), sessions.clone(), portal.clone(), signals));
    }
    tracing::info!(%portal, clipboard, "input capture session started");
    let mut results = HashMap::from([
        ("capabilities".to_owned(), value(Value::from(wanted))),
        ("clipboard_enabled".to_owned(), value(Value::from(clipboard))),
    ]);
    if persist != 0 {
        let remembered = crate::screencast::Remembered { monitor: None, window: None, devices: Some(wanted) };
        let (vendor, version, data) = remembered.restore_data();
        results.insert("restore_data".to_owned(), value(Value::from((vendor, version, data))));
    }
    (response::SUCCESS, results)
}

impl InputCaptureBackend {
    fn compositor_session(&self, session: &ObjectPath<'_>) -> Option<OwnedObjectPath> {
        lock(&self.sessions).get(&OwnedObjectPath::from(session.to_owned())).cloned()
    }

    async fn call<B>(&self, session: &ObjectPath<'_>, method: &str, body: &B) -> fdo::Result<zbus::Message>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        let path = self.compositor_session(session).ok_or_else(|| fdo::Error::InvalidArgs("no such session".into()))?;
        self.conn.call_method(Some(COMPOSITOR), path.as_str(), Some(SESSION_IFACE), method, body).await.map_err(|e| fdo::Error::Failed(e.to_string()))
    }
}

/// Pass the compositor session's signals on as the portal's, until it
/// closes (then the portal session closes too).
async fn follow(conn: Connection, sessions: Sessions, portal: OwnedObjectPath, mut signals: zbus::MessageStream) {
    let Ok(emitter) = SignalEmitter::new(&conn, crate::OBJECT_PATH) else { return };
    let session = ObjectPath::from(&portal);
    while let Some(Ok(message)) = signals.next().await {
        let Some(member) = message.header().member().map(|m| m.to_string()) else { continue };
        let body = message.body();
        let o = |pairs: Vec<(&str, Value<'_>)>| -> HashMap<String, OwnedValue> { pairs.into_iter().map(|(k, v)| (k.to_owned(), value(v))).collect() };
        let result = match member.as_str() {
            "Activated" => match body.deserialize::<(u32, u32, (f64, f64))>() {
                Ok((activation, barrier, position)) => {
                    InputCaptureBackend::activated(&emitter, session.clone(), o(vec![("activation_id", Value::from(activation)), ("barrier_id", Value::from(barrier)), ("cursor_position", Value::from(position))])).await
                }
                Err(_) => continue,
            },
            "Deactivated" => match body.deserialize::<(u32, (f64, f64))>() {
                Ok((activation, position)) => InputCaptureBackend::deactivated(&emitter, session.clone(), o(vec![("activation_id", Value::from(activation)), ("cursor_position", Value::from(position))])).await,
                Err(_) => continue,
            },
            "Disabled" => InputCaptureBackend::disabled(&emitter, session.clone(), HashMap::new()).await,
            "SelectionOwnerChanged" => match body.deserialize::<HashMap<String, OwnedValue>>() {
                Ok(options) => crate::clipboard::ClipboardBackend::selection_owner_changed(&emitter, session.clone(), crate::clipboard::rename(options, false)).await,
                Err(_) => continue,
            },
            "SelectionTransfer" => match body.deserialize::<(String, u32)>() {
                Ok((mime, serial)) => crate::clipboard::ClipboardBackend::selection_transfer(&emitter, session.clone(), &mime, serial).await,
                Err(_) => continue,
            },
            "ZonesChanged" => match body.deserialize::<u32>() {
                Ok(set) => InputCaptureBackend::zones_changed(&emitter, session.clone(), o(vec![("zone_set", Value::from(set))])).await,
                Err(_) => continue,
            },
            "Closed" => {
                end(&conn, &sessions, &portal, false).await;
                break;
            }
            _ => continue,
        };
        if let Err(err) = result {
            tracing::warn!(%err, member, "cannot pass on an input capture signal");
        }
    }
}

/// End portal session `portal` (and, unless the compositor ended it, the
/// compositor's).
async fn end(conn: &Connection, sessions: &Sessions, portal: &OwnedObjectPath, close_compositor: bool) {
    let compositor = lock(sessions).remove(portal);
    {
        let mut c = clipboards();
        c.requested.remove(portal);
        c.shared.remove(portal);
    }
    if let (true, Some(compositor)) = (close_compositor, &compositor) {
        let _ = conn.call_method(Some(COMPOSITOR), compositor.as_str(), Some(SESSION_IFACE), "Close", &()).await;
    }
    if conn.object_server().remove::<CaptureSession, _>(portal.as_str()).await.unwrap_or(false) {
        if let Ok(emitter) = SignalEmitter::new(conn, portal.as_str()) {
            let _ = CaptureSession::closed(&emitter).await;
        }
        tracing::info!(%portal, "input capture session ended");
    }
}

pub struct CaptureSession {
    sessions: Sessions,
    pending: Pending,
}

#[interface(name = "org.freedesktop.impl.portal.Session")]
impl CaptureSession {
    async fn close(&self, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> fdo::Result<()> {
        if let Some(path) = header.path() {
            let path = OwnedObjectPath::from(path.to_owned());
            lock_pending(&self.pending).remove(&path);
            clipboards().requested.remove(&path);
            end(conn, &self.sessions, &path, true).await;
        }
        Ok(())
    }

    #[zbus(signal)]
    pub async fn closed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(property, name = "version")]
    async fn version(&self) -> u32 {
        1
    }
}

fn empty() -> (u32, Results) {
    (response::SUCCESS, Results::new())
}

#[interface(name = "org.freedesktop.impl.portal.InputCapture")]
impl InputCaptureBackend {
    /// Version 1: create and start at once (asking the user).
    async fn create_session(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        app_id: String,
        _parent_window: String,
        options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<(u32, Results)> {
        let wanted = options.get("capabilities").and_then(|v| u32::try_from(v.clone()).ok()).unwrap_or(0) & CAPABILITIES;
        let portal = OwnedObjectPath::from(session_handle.into_owned());
        if !server.at(&portal, CaptureSession { sessions: self.sessions.clone(), pending: self.pending.clone() }).await.unwrap_or(false) {
            return Ok((response::OTHER, Results::new()));
        }
        let (conn, sessions) = (self.conn.clone(), self.sessions.clone());
        tracing::info!(app_id, wanted, "input capture asked");
        Ok(with_request(server, handle, || async move {
            let started = start(&conn, &sessions, &portal, &app_id, wanted, None, 0).await;
            if started.0 != response::SUCCESS {
                end(&conn, &sessions, &portal, false).await;
            }
            started
        })
        .await)
    }

    /// Version 2: a session that is not started yet.
    async fn create_session2(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        session_handle: ObjectPath<'_>,
        app_id: String,
        _options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<Results> {
        let portal = OwnedObjectPath::from(session_handle.into_owned());
        lock_pending(&self.pending).insert(portal.clone());
        if !server.at(&portal, CaptureSession { sessions: self.sessions.clone(), pending: self.pending.clone() }).await.unwrap_or(false) {
            lock_pending(&self.pending).remove(&portal);
            return Err(fdo::Error::Failed("cannot make the session".into()));
        }
        tracing::info!(%portal, app_id, "input capture session made");
        Ok(Results::new())
    }

    /// Version 2: start a session made with `CreateSession2` (asking the
    /// user unless `restore_data` says it was allowed before).
    #[allow(clippy::too_many_arguments)]
    async fn start(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        app_id: String,
        _parent_window: String,
        options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<(u32, Results)> {
        let portal = OwnedObjectPath::from(session_handle.into_owned());
        if !lock_pending(&self.pending).remove(&portal) {
            return Err(fdo::Error::Failed("no session waiting to start".into()));
        }
        let wanted = options.get("capabilities").and_then(|v| u32::try_from(v.clone()).ok()).unwrap_or(0) & CAPABILITIES;
        let restored = options.get("restore_data").and_then(|v| crate::screencast::Remembered::from_restore_data(v)).and_then(|r| r.devices);
        let persist = options.get("persist_mode").and_then(|v| u32::try_from(v.clone()).ok()).filter(|m| *m <= 2).unwrap_or(0);
        let (conn, sessions) = (self.conn.clone(), self.sessions.clone());
        tracing::info!(app_id, wanted, restored = ?restored, persist, "input capture start");
        Ok(with_request(server, handle, || async move {
            let started = start(&conn, &sessions, &portal, &app_id, wanted, restored, persist).await;
            if started.0 != response::SUCCESS {
                end(&conn, &sessions, &portal, false).await;
            }
            started
        })
        .await)
    }

    async fn get_zones(&self, #[zbus(object_server)] server: &ObjectServer, handle: ObjectPath<'_>, session_handle: ObjectPath<'_>, _app_id: String, _options: HashMap<String, OwnedValue>) -> fdo::Result<(u32, Results)> {
        let reply = self.call(&session_handle, "GetZones", &()).await;
        Ok(with_request(server, handle, || async move {
            match reply.and_then(|r| r.body().deserialize::<(u32, Vec<(u32, u32, i32, i32)>)>().map_err(|e| fdo::Error::Failed(e.to_string()))) {
                Ok((set, zones)) => (response::SUCCESS, HashMap::from([("zones".to_owned(), value(Value::from(zones))), ("zone_set".to_owned(), value(Value::from(set)))])),
                Err(_) => (response::OTHER, Results::new()),
            }
        })
        .await)
    }

    #[allow(clippy::too_many_arguments)]
    async fn set_pointer_barriers(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        _app_id: String,
        _options: HashMap<String, OwnedValue>,
        barriers: Vec<HashMap<String, OwnedValue>>,
        zone_set: u32,
    ) -> fdo::Result<(u32, Results)> {
        let given = barriers_of(&barriers);
        // Entries without an id and position fail too (they have no id to name).
        let reply = self.call(&session_handle, "SetPointerBarriers", &(given, zone_set)).await;
        Ok(with_request(server, handle, || async move {
            match reply.and_then(|r| r.body().deserialize::<Vec<u32>>().map_err(|e| fdo::Error::Failed(e.to_string()))) {
                Ok(failed) => (response::SUCCESS, HashMap::from([("failed_barriers".to_owned(), value(Value::from(failed)))])),
                Err(_) => (response::OTHER, Results::new()),
            }
        })
        .await)
    }

    async fn enable(&self, session_handle: ObjectPath<'_>, _app_id: String, _options: HashMap<String, OwnedValue>) -> fdo::Result<(u32, Results)> {
        self.call(&session_handle, "Enable", &()).await?;
        Ok(empty())
    }

    async fn disable(&self, session_handle: ObjectPath<'_>, _app_id: String, _options: HashMap<String, OwnedValue>) -> fdo::Result<(u32, Results)> {
        self.call(&session_handle, "Disable", &()).await?;
        Ok(empty())
    }

    async fn release(&self, session_handle: ObjectPath<'_>, _app_id: String, options: HashMap<String, OwnedValue>) -> fdo::Result<(u32, Results)> {
        let activation = options.get("activation_id").and_then(|v| u32::try_from(v.clone()).ok()).unwrap_or(0);
        let position = options.get("cursor_position").and_then(|v| v.try_clone().ok()).and_then(|v| <(f64, f64)>::try_from(v).ok());
        self.call(&session_handle, "Release", &(activation, position.is_some(), position.unwrap_or_default())).await?;
        Ok(empty())
    }

    #[zbus(name = "ConnectToEIS")]
    async fn connect_to_eis(&self, session_handle: ObjectPath<'_>, _app_id: String, _options: HashMap<String, OwnedValue>) -> fdo::Result<OwnedFd> {
        let reply = self.call(&session_handle, "ConnectToEIS", &()).await?;
        reply.body().deserialize::<OwnedFd>().map_err(|e| fdo::Error::Failed(e.to_string()))
    }

    #[zbus(signal)]
    async fn disabled(emitter: &SignalEmitter<'_>, session_handle: ObjectPath<'_>, options: HashMap<String, OwnedValue>) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn activated(emitter: &SignalEmitter<'_>, session_handle: ObjectPath<'_>, options: HashMap<String, OwnedValue>) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn deactivated(emitter: &SignalEmitter<'_>, session_handle: ObjectPath<'_>, options: HashMap<String, OwnedValue>) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn zones_changed(emitter: &SignalEmitter<'_>, session_handle: ObjectPath<'_>, options: HashMap<String, OwnedValue>) -> zbus::Result<()>;

    #[zbus(property)]
    async fn supported_capabilities(&self) -> u32 {
        CAPABILITIES
    }

    #[zbus(property, name = "version")]
    async fn version(&self) -> u32 {
        2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn barrier_entries() {
        let entry = |id: Option<u32>, pos: Option<(i32, i32, i32, i32)>| {
            let mut m = HashMap::new();
            if let Some(id) = id {
                m.insert("barrier_id".to_owned(), value(Value::from(id)));
            }
            if let Some(p) = pos {
                m.insert("position".to_owned(), value(Value::from(p)));
            }
            m
        };
        let got = barriers_of(&[entry(Some(3), Some((0, 0, 0, 799))), entry(Some(4), None), entry(None, Some((1, 2, 3, 4)))]);
        assert_eq!(got, [(3, (0, 0, 0, 799))]);
        assert!(message("Input Leap", "Super+Shift+Escape").ends_with("Super+Shift+Escape gives them back."));
        assert_eq!(accelerator_label("<Super><Shift>Escape"), "Super+Shift+Escape");
        assert_eq!(accelerator_label("<Primary><Alt>F1"), "Ctrl+Alt+F1");
        assert!(must_ask(3, None));
        assert!(!must_ask(3, Some(3)));
        assert!(must_ask(3, Some(2)), "more than allowed before");
        assert!(!must_ask(2, Some(3)));
    }
}
