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

/// The dialog's message.
pub fn message(app: &str) -> String {
    format!("{app} wants to take over the pointer and keyboard when the pointer leaves the screen. Super+Escape gives them back.")
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
async fn follow(conn: Connection, sessions: Sessions, portal: OwnedObjectPath, compositor: OwnedObjectPath) {
    let rule = match zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).interface(SESSION_IFACE).and_then(|r| r.path(compositor.clone())) {
        Ok(rule) => rule.build(),
        Err(_) => return,
    };
    let Ok(mut signals) = zbus::MessageStream::for_match_rule(rule, &conn, None).await else { return };
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
    let Some(compositor) = lock(sessions).remove(portal) else { return };
    if close_compositor {
        let _ = conn.call_method(Some(COMPOSITOR), compositor.as_str(), Some(SESSION_IFACE), "Close", &()).await;
    }
    if let Ok(emitter) = SignalEmitter::new(conn, portal.as_str()) {
        let _ = CaptureSession::closed(&emitter).await;
    }
    let _ = conn.object_server().remove::<CaptureSession, _>(portal.as_str()).await;
    tracing::info!(%portal, "input capture session ended");
}

pub struct CaptureSession {
    sessions: Sessions,
}

#[interface(name = "org.freedesktop.impl.portal.Session")]
impl CaptureSession {
    async fn close(&self, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> fdo::Result<()> {
        if let Some(path) = header.path() {
            end(conn, &self.sessions, &OwnedObjectPath::from(path.to_owned()), true).await;
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
        let (conn, sessions) = (self.conn.clone(), self.sessions.clone());
        tracing::info!(app_id, wanted, "input capture asked");
        Ok(with_request(server, handle, || async move {
            if wanted == 0 {
                return (response::OTHER, Results::new());
            }
            let who = if app_id.is_empty() { "An app".to_owned() } else { app_name(&app_id) };
            let asked = HashMap::from([
                ("title".to_owned(), value(Value::from("Allow Input Capture?"))),
                ("message".to_owned(), value(Value::from(message(&who)))),
                ("accept".to_owned(), value(Value::from("Allow"))),
                ("cancel".to_owned(), value(Value::from("Deny"))),
            ]);
            if dialog(&conn, asked).await.is_none() {
                return (response::CANCELLED, Results::new());
            }
            let created = conn.call_method(Some(COMPOSITOR), COMPOSITOR_PATH, Some(COMPOSITOR), "CreateSession", &(wanted,)).await;
            let compositor: OwnedObjectPath = match created.and_then(|r| r.body().deserialize()) {
                Ok(path) => path,
                Err(err) => {
                    tracing::warn!(%err, "the compositor cannot capture input");
                    return (response::OTHER, Results::new());
                }
            };
            lock(&sessions).insert(portal.clone(), compositor.clone());
            if !server.at(&portal, CaptureSession { sessions: sessions.clone() }).await.unwrap_or(false) {
                lock(&sessions).remove(&portal);
                return (response::OTHER, Results::new());
            }
            tokio::spawn(follow(conn.clone(), sessions.clone(), portal.clone(), compositor));
            tracing::info!(%portal, "input capture session");
            (response::SUCCESS, HashMap::from([("capabilities".to_owned(), value(Value::from(wanted)))]))
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
        1
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
        assert!(message("Input Leap").contains("Super+Escape"));
    }
}
