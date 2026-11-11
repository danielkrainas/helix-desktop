//! `org.freedesktop.impl.portal.ScreenCast` backend.
//!
//! Contract: `helix-desktop-spec/contracts/dbus/org.freedesktop.impl.portal.ScreenCast.xml`
//! and `org.freedesktop.impl.portal.md` ("ScreenCast").
//!
//! The user picks a monitor in the shell (`org.helix.ScreenCastChooser`);
//! the compositor records it (`org.gnome.Mutter.ScreenCast`) into a
//! PipeWire stream, whose node id goes back to the app.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use futures_util::StreamExt;
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zbus::{fdo, interface, Connection, ObjectServer};

use crate::screenshot::{response, with_request};

pub const SOURCE_MONITOR: u32 = 1;
pub const SOURCE_WINDOW: u32 = 2;
pub const SOURCE_VIRTUAL: u32 = 4;
/// The vendor name in `restore_data`.
pub const RESTORE_VENDOR: &str = "Helix";
pub const RESTORE_VERSION: u32 = 1;
pub const CURSOR_HIDDEN: u32 = 1;
pub const CURSOR_EMBEDDED: u32 = 2;
pub const CURSOR_METADATA: u32 = 4;

const MUTTER: &str = "org.gnome.Mutter.ScreenCast";
const MUTTER_PATH: &str = "/org/gnome/Mutter/ScreenCast";
const SHELL: &str = "org.gnome.Shell";
const CHOOSER_PATH: &str = "/org/helix/ScreenCastChooser";
const DISPLAY_CONFIG: &str = "org.gnome.Mutter.DisplayConfig";

type Results = HashMap<String, OwnedValue>;

#[derive(Debug, Default)]
struct Cast {
    app_id: String,
    cursor_mode: u32,
    types: u32,
    persist_mode: u32,
    /// The monitor to share again, from `restore_data`.
    restore: Option<MonitorId>,
    /// The compositor's session, once started.
    mutter: Option<OwnedObjectPath>,
}

/// What identifies a monitor across sessions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MonitorId {
    pub connector: String,
    pub vendor: String,
    pub product: String,
    pub serial: String,
}

impl MonitorId {
    fn known_identity(&self) -> bool {
        !(self.vendor.is_empty() || self.vendor == "Unknown") || !(self.product.is_empty() || self.product == "Unknown")
    }

    /// `restore_data` for this monitor.
    pub fn restore_data(&self) -> (String, u32, OwnedValue) {
        let dict = HashMap::from([
            ("connector", Value::from(self.connector.as_str())),
            ("vendor", Value::from(self.vendor.as_str())),
            ("product", Value::from(self.product.as_str())),
            ("serial", Value::from(self.serial.as_str())),
        ]);
        (RESTORE_VENDOR.to_owned(), RESTORE_VERSION, owned(Value::from(dict)))
    }

    /// The monitor described by `restore_data`, if it is Helix's.
    pub fn from_restore_data(value: &Value<'_>) -> Option<Self> {
        let Value::Structure(s) = value else { return None };
        let [Value::Str(vendor), Value::U32(version), Value::Value(data)] = s.fields() else { return None };
        if vendor.as_str() != RESTORE_VENDOR || *version != RESTORE_VERSION {
            return None;
        }
        let dict = HashMap::<String, OwnedValue>::try_from(OwnedValue::try_from((**data).try_clone().ok()?).ok()?).ok()?;
        let get = |k: &str| dict.get(k).and_then(|v| String::try_from(v.clone()).ok()).unwrap_or_default();
        Some(Self { connector: get("connector"), vendor: get("vendor"), product: get("product"), serial: get("serial") })
    }

    /// The connector of the monitor in `on` this one is, if any: the same
    /// identity, else (when the identity is unknown) the same connector.
    pub fn find_in(&self, on: &[(MonitorId, String)]) -> Option<String> {
        let same = |m: &MonitorId| m.vendor == self.vendor && m.product == self.product && m.serial == self.serial;
        if self.known_identity() {
            on.iter().find(|(m, _)| m.known_identity() && same(m)).map(|(m, _)| m.connector.clone())
        } else {
            on.iter().find(|(m, _)| !m.known_identity() && m.connector == self.connector).map(|(m, _)| m.connector.clone())
        }
    }
}

#[derive(Debug, Default)]
pub struct Casts {
    sessions: HashMap<OwnedObjectPath, Cast>,
}

pub type Shared = Arc<Mutex<Casts>>;

fn lock(shared: &Shared) -> MutexGuard<'_, Casts> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

fn owned(v: Value<'_>) -> OwnedValue {
    OwnedValue::try_from(v).expect("plain values")
}

/// The portal Session object of one screen cast.
pub struct CastSession {
    shared: Shared,
}

#[interface(name = "org.freedesktop.impl.portal.Session")]
impl CastSession {
    async fn close(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<()> {
        if let Some(path) = header.path() {
            end(conn, &self.shared, &OwnedObjectPath::from(path.to_owned()), false).await;
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

/// End a portal session: stop the recording, drop the object; emit
/// `Closed` when the end did not come from the frontend.
async fn end(conn: &Connection, shared: &Shared, path: &OwnedObjectPath, notify: bool) {
    let Some(cast) = lock(shared).sessions.remove(path) else { return };
    if let Some(mutter) = &cast.mutter {
        let _ = conn
            .call_method(Some(MUTTER), mutter.as_str(), Some("org.gnome.Mutter.ScreenCast.Session"), "Stop", &())
            .await;
    }
    if notify {
        if let Ok(emitter) = SignalEmitter::new(conn, path.as_str()) {
            let _ = CastSession::closed(&emitter).await;
        }
    }
    let _ = conn.object_server().remove::<CastSession, _>(path.as_str()).await;
    tracing::info!(%path, app_id = cast.app_id, "screen cast ended");
}

/// Monitors that are on: (identity, display name), from DisplayConfig.
async fn monitors(conn: &Connection) -> zbus::Result<Vec<(MonitorId, String)>> {
    type Spec = (String, String, String, String);
    type Mode = (String, i32, i32, f64, f64, Vec<f64>, HashMap<String, OwnedValue>);
    type Logical = (i32, i32, f64, u32, bool, Vec<Spec>, HashMap<String, OwnedValue>);
    let reply = conn
        .call_method(Some(DISPLAY_CONFIG), "/org/gnome/Mutter/DisplayConfig", Some(DISPLAY_CONFIG), "GetCurrentState", &())
        .await?;
    type Props = HashMap<String, OwnedValue>;
    type State = (u32, Vec<(Spec, Vec<Mode>, Props)>, Vec<Logical>, Props);
    let (_, monitors, logical, _): State = reply.body().deserialize()?;
    let shown: Vec<String> = logical.iter().flat_map(|l| l.5.iter().map(|s| s.0.clone())).collect();
    Ok(monitors
        .into_iter()
        .filter(|(spec, _, _)| shown.contains(&spec.0))
        .map(|(spec, _, props)| {
            let name = props
                .get("display-name")
                .and_then(|v| String::try_from(v.clone()).ok())
                .unwrap_or_else(|| spec.0.clone());
            (MonitorId { connector: spec.0, vendor: spec.1, product: spec.2, serial: spec.3 }, name)
        })
        .collect())
}

/// What the user chose (or what was restored).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Source {
    Monitor(String),
    Window(u32),
    /// A new virtual monitor.
    Virtual,
}

async fn start(conn: &Connection, shared: &Shared, session: &OwnedObjectPath) -> (u32, Results) {
    let (app_id, cursor_mode, types, persist_mode, restore) = match lock(shared).sessions.get(session) {
        Some(c) => (c.app_id.clone(), c.cursor_mode, c.types, c.persist_mode, c.restore.clone()),
        None => return (response::OTHER, Results::new()),
    };
    let on = match monitors(conn).await {
        Ok(m) => m,
        Err(err) => {
            tracing::warn!(%err, "cannot list monitors");
            return (response::OTHER, Results::new());
        }
    };
    let restored = restore.as_ref().filter(|_| types & SOURCE_MONITOR != 0).and_then(|r| r.find_in(&on));
    // Only a virtual monitor: nothing that exists is shown, so no asking.
    let virtual_only = types == SOURCE_VIRTUAL;
    let source = match restored {
        _ if virtual_only => Source::Virtual,
        Some(connector) => {
            tracing::info!(connector, app_id, "sharing a remembered monitor");
            Source::Monitor(connector)
        }
        None => {
            let offered: Vec<(String, String)> = if types & SOURCE_MONITOR != 0 {
                on.iter().map(|(m, name)| (m.connector.clone(), name.clone())).collect()
            } else {
                Vec::new()
            };
            let options = HashMap::from([
                ("monitors", Value::from(types & SOURCE_MONITOR != 0)),
                ("windows", Value::from(types & SOURCE_WINDOW != 0)),
                ("remember", Value::from(persist_mode != 0)),
            ]);
            let choice = conn
                .call_method(Some(SHELL), CHOOSER_PATH, Some("org.helix.ScreenCastChooser"), "ChooseSource", &(app_id.as_str(), &offered, options))
                .await
                .and_then(|r| r.body().deserialize::<(bool, u32, String, u32)>());
            match choice {
                Ok((true, SOURCE_MONITOR, connector, _)) => Source::Monitor(connector),
                Ok((true, SOURCE_WINDOW, _, window)) => Source::Window(window),
                Ok(_) => return (response::CANCELLED, Results::new()),
                Err(err) => {
                    tracing::warn!(%err, "cannot ask what to share");
                    return (response::OTHER, Results::new());
                }
            }
        }
    };
    match record(conn, shared, session, &source, cursor_mode).await {
        Ok(stream) => {
            let mut results = HashMap::from([("streams".to_owned(), owned(Value::from(vec![stream])))]);
            let remembered = match &source {
                Source::Monitor(connector) if persist_mode != 0 => on.iter().find(|(m, _)| m.connector == *connector).map(|(m, _)| m.clone()),
                _ => None,
            };
            match remembered {
                Some(monitor) => {
                    results.insert("persist_mode".to_owned(), owned(Value::from(persist_mode)));
                    results.insert("restore_data".to_owned(), owned(Value::from(monitor.restore_data())));
                }
                None => {
                    results.insert("persist_mode".to_owned(), owned(Value::from(0u32)));
                }
            }
            (response::SUCCESS, results)
        }
        Err(err) => {
            tracing::warn!(%err, ?source, "cannot start the screen cast");
            end(conn, shared, session, true).await;
            (response::OTHER, Results::new())
        }
    }
}

/// Record `connector` through the compositor; the stream for the app.
async fn record(
    conn: &Connection,
    shared: &Shared,
    session: &OwnedObjectPath,
    source: &Source,
    cursor_mode: u32,
) -> zbus::Result<(u32, HashMap<String, OwnedValue>)> {
    let reply = conn
        .call_method(Some(MUTTER), MUTTER_PATH, Some("org.gnome.Mutter.ScreenCast"), "CreateSession", &(HashMap::<&str, Value<'_>>::new(),))
        .await?;
    let mutter: OwnedObjectPath = reply.body().deserialize()?;
    if let Some(cast) = lock(shared).sessions.get_mut(session) {
        cast.mutter = Some(mutter.clone());
    }
    let mutter_cursor = match cursor_mode {
        CURSOR_EMBEDDED => 1u32,
        CURSOR_METADATA => 2,
        _ => 0,
    };
    let mut props = HashMap::from([("cursor-mode", Value::from(mutter_cursor))]);
    let session_iface = Some("org.gnome.Mutter.ScreenCast.Session");
    let (reply, source_type) = match source {
        Source::Monitor(connector) => (
            conn.call_method(Some(MUTTER), mutter.as_str(), session_iface, "RecordMonitor", &(connector.as_str(), props)).await?,
            SOURCE_MONITOR,
        ),
        Source::Window(window) => {
            props.insert("window-id", Value::from(u64::from(*window)));
            (conn.call_method(Some(MUTTER), mutter.as_str(), session_iface, "RecordWindow", &(props,)).await?, SOURCE_WINDOW)
        }
        Source::Virtual => (conn.call_method(Some(MUTTER), mutter.as_str(), session_iface, "RecordVirtual", &(props,)).await?, SOURCE_VIRTUAL),
    };
    let stream: OwnedObjectPath = reply.body().deserialize()?;
    let parameters: HashMap<String, OwnedValue> = zbus::Proxy::new(conn, MUTTER, stream.as_str(), "org.gnome.Mutter.ScreenCast.Stream")
        .await?
        .get_property("Parameters")
        .await?;
    // Watch for the node id and for the compositor ending the session.
    let added = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.gnome.Mutter.ScreenCast.Stream")?
        .member("PipeWireStreamAdded")?
        .path(stream.clone())?
        .build();
    let mut added = zbus::MessageStream::for_match_rule(added, conn, None).await?;
    let closed = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.gnome.Mutter.ScreenCast.Session")?
        .member("Closed")?
        .path(mutter.clone())?
        .build();
    let mut closed = zbus::MessageStream::for_match_rule(closed, conn, None).await?;
    conn.call_method(Some(MUTTER), mutter.as_str(), Some("org.gnome.Mutter.ScreenCast.Session"), "Start", &())
        .await?;
    let node: u32 = match tokio::time::timeout(Duration::from_secs(10), added.next()).await {
        Ok(Some(Ok(message))) => message.body().deserialize()?,
        _ => return Err(zbus::Error::Failure("no PipeWire stream".into())),
    };
    {
        let (conn, shared, session) = (conn.clone(), shared.clone(), session.clone());
        tokio::spawn(async move {
            if closed.next().await.is_some() {
                // Already gone if the frontend closed it.
                if let Some(cast) = lock(&shared).sessions.get_mut(&session) {
                    cast.mutter = None;
                }
                end(&conn, &shared, &session, true).await;
            }
        });
    }
    let mut props = HashMap::from([("source_type".to_owned(), owned(Value::from(source_type)))]);
    for key in ["position", "size"] {
        if let Some(value) = parameters.get(key) {
            props.insert(key.to_owned(), value.try_clone().expect("plain values"));
        }
    }
    tracing::info!(node, ?source, "screen cast stream ready");
    Ok((node, props))
}

/// The exported ScreenCast backend.
pub struct ScreenCastBackend {
    pub shared: Shared,
}

#[interface(name = "org.freedesktop.impl.portal.ScreenCast")]
impl ScreenCastBackend {
    async fn create_session(
        &self,
        handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        app_id: &str,
        _options: HashMap<String, Value<'_>>,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> fdo::Result<(u32, Results)> {
        let session = OwnedObjectPath::from(session_handle.into_owned());
        let shared = self.shared.clone();
        Ok(with_request(server, handle, || async move {
            lock(&shared).sessions.insert(session.clone(), Cast { app_id: app_id.to_owned(), cursor_mode: CURSOR_HIDDEN, types: SOURCE_MONITOR, ..Cast::default() });
            if !server.at(&session, CastSession { shared: shared.clone() }).await.unwrap_or(false) {
                lock(&shared).sessions.remove(&session);
                return (response::OTHER, Results::new());
            }
            tracing::info!(%session, app_id, "screen cast session");
            (response::SUCCESS, HashMap::from([("session_id".to_owned(), owned(Value::from(session.as_str())))]))
        })
        .await)
    }

    async fn select_sources(
        &self,
        handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        _app_id: &str,
        options: HashMap<String, Value<'_>>,
        #[zbus(object_server)] server: &ObjectServer,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<(u32, Results)> {
        let session = OwnedObjectPath::from(session_handle.into_owned());
        let types = match options.get("types") {
            Some(Value::U32(t)) => *t,
            _ => SOURCE_MONITOR,
        };
        let cursor_mode = match options.get("cursor_mode") {
            Some(Value::U32(m)) => *m,
            _ => CURSOR_HIDDEN,
        };
        let persist_mode = match options.get("persist_mode") {
            Some(Value::U32(m)) if *m <= 2 => *m,
            _ => 0,
        };
        let restore = options.get("restore_data").and_then(MonitorId::from_restore_data);
        let shared = self.shared.clone();
        Ok(with_request(server, handle, || async move {
            if !lock(&shared).sessions.contains_key(&session) {
                return (response::OTHER, Results::new());
            }
            if ![CURSOR_HIDDEN, CURSOR_EMBEDDED, CURSOR_METADATA].contains(&cursor_mode) {
                end(conn, &shared, &session, true).await;
                return (response::OTHER, Results::new());
            }
            let known = SOURCE_MONITOR | SOURCE_WINDOW | SOURCE_VIRTUAL;
            if types & known == 0 {
                return (response::OTHER, Results::new());
            }
            if let Some(cast) = lock(&shared).sessions.get_mut(&session) {
                cast.cursor_mode = cursor_mode;
                cast.types = types & known;
                cast.persist_mode = persist_mode;
                cast.restore = restore;
            }
            (response::SUCCESS, Results::new())
        })
        .await)
    }

    #[allow(clippy::too_many_arguments)]
    async fn start(
        &self,
        handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        _app_id: &str,
        _parent_window: &str,
        _options: HashMap<String, Value<'_>>,
        #[zbus(object_server)] server: &ObjectServer,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<(u32, Results)> {
        let session = OwnedObjectPath::from(session_handle.into_owned());
        let shared = self.shared.clone();
        Ok(with_request(server, handle, || async move { start(conn, &shared, &session).await }).await)
    }

    #[zbus(property)]
    async fn available_source_types(&self) -> u32 {
        SOURCE_MONITOR | SOURCE_WINDOW | SOURCE_VIRTUAL
    }

    #[zbus(property)]
    async fn available_cursor_modes(&self) -> u32 {
        CURSOR_HIDDEN | CURSOR_EMBEDDED | CURSOR_METADATA
    }

    #[zbus(property, name = "version")]
    async fn version(&self) -> u32 {
        4
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(connector: &str, vendor: &str, serial: &str) -> MonitorId {
        MonitorId { connector: connector.into(), vendor: vendor.into(), product: "P".into(), serial: serial.into() }
    }

    #[test]
    fn restore_data_round_trip_and_matching() {
        let monitor = id("DP-1", "Acme", "123");
        let (vendor, version, data) = monitor.restore_data();
        let value = Value::from((vendor, version, data));
        let value = OwnedValue::try_from(value).unwrap();
        assert_eq!(MonitorId::from_restore_data(&value), Some(monitor.clone()));
        assert_eq!(MonitorId::from_restore_data(&Value::from(("GNOME", 1u32, Value::from(1u32)))), None, "not ours");
        // Same monitor on another connector: found by identity.
        let on = vec![(id("HDMI-1", "Acme", "123"), "x".to_owned()), (id("DP-1", "Other", "9"), "y".to_owned())];
        assert_eq!(monitor.find_in(&on), Some("HDMI-1".to_owned()));
        assert_eq!(monitor.find_in(&on[1..]), None, "a different monitor on that connector");
        // Unknown identity: the same connector.
        let anonymous = MonitorId { connector: "eDP-1".into(), vendor: "Unknown".into(), product: "Unknown".into(), serial: "0".into() };
        let on = vec![(anonymous.clone(), "z".to_owned())];
        assert_eq!(anonymous.find_in(&on), Some("eDP-1".to_owned()));
    }
}
