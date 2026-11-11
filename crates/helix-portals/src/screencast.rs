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
pub(crate) const REMOTE: &str = "org.gnome.Mutter.RemoteDesktop";
pub(crate) const REMOTE_PATH: &str = "/org/gnome/Mutter/RemoteDesktop";
const MUTTER_PATH: &str = "/org/gnome/Mutter/ScreenCast";
const SHELL: &str = "org.gnome.Shell";
const CHOOSER_PATH: &str = "/org/helix/ScreenCastChooser";
const DISPLAY_CONFIG: &str = "org.gnome.Mutter.DisplayConfig";

type Results = HashMap<String, OwnedValue>;

#[derive(Debug, Default)]
pub(crate) struct Cast {
    pub(crate) app_id: String,
    cursor_mode: u32,
    types: u32,
    pub(crate) persist_mode: u32,
    /// What to share (and allow) again, from `restore_data`.
    pub(crate) restore: Option<Remembered>,
    /// The compositor's session, once started.
    mutter: Option<OwnedObjectPath>,
    /// `SelectSources` was called (a remote desktop session shares the
    /// screen only then).
    sources_selected: bool,
    /// A RemoteDesktop session (`None`: a plain ScreenCast session).
    pub(crate) remote: Option<Remote>,
}

impl Cast {
    /// A RemoteDesktop session of `app_id` (no screen sharing until
    /// sources are selected).
    pub(crate) fn remote(app_id: &str, devices: u32) -> Self {
        Self {
            app_id: app_id.to_owned(),
            cursor_mode: CURSOR_HIDDEN,
            types: 0,
            remote: Some(Remote { devices, ..Remote::default() }),
            ..Self::default()
        }
    }
}

/// The remote desktop part of a session.
#[derive(Debug, Default)]
pub(crate) struct Remote {
    /// Device types asked for (`SelectDevices`).
    pub devices: u32,
    /// Device types the user allowed (once started).
    pub granted: u32,
    /// The compositor's remote desktop session, once started.
    pub mutter: Option<OwnedObjectPath>,
    /// Stream objects by PipeWire node id.
    pub streams: HashMap<u32, OwnedObjectPath>,
    /// `RequestClipboard` was called (before `Start`).
    pub clipboard_requested: bool,
    /// The clipboard is shared (once started).
    pub clipboard: bool,
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

/// What a session remembers (`restore_data` `("Helix", 1, a{sv})`): a
/// monitor, a window (app id and title) and, for remote desktop, devices.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Remembered {
    pub monitor: Option<MonitorId>,
    pub window: Option<(String, String)>,
    pub devices: Option<u32>,
}

impl Remembered {
    pub fn restore_data(&self) -> (String, u32, OwnedValue) {
        let mut dict: HashMap<&str, Value<'_>> = HashMap::new();
        if let Some(m) = &self.monitor {
            dict.insert("connector", Value::from(m.connector.as_str()));
            dict.insert("vendor", Value::from(m.vendor.as_str()));
            dict.insert("product", Value::from(m.product.as_str()));
            dict.insert("serial", Value::from(m.serial.as_str()));
        }
        if let Some((app, title)) = &self.window {
            dict.insert("window-app-id", Value::from(app.as_str()));
            dict.insert("window-title", Value::from(title.as_str()));
        }
        if let Some(devices) = self.devices {
            dict.insert("devices", Value::from(devices));
        }
        (RESTORE_VENDOR.to_owned(), RESTORE_VERSION, owned(Value::from(dict)))
    }

    /// What `restore_data` remembers, if it is Helix's.
    pub fn from_restore_data(value: &Value<'_>) -> Option<Self> {
        let Value::Structure(s) = value else { return None };
        let [Value::Str(vendor), Value::U32(version), Value::Value(data)] = s.fields() else { return None };
        if vendor.as_str() != RESTORE_VENDOR || *version != RESTORE_VERSION {
            return None;
        }
        let dict = HashMap::<String, OwnedValue>::try_from(OwnedValue::try_from((**data).try_clone().ok()?).ok()?).ok()?;
        let get = |k: &str| dict.get(k).and_then(|v| String::try_from(v.clone()).ok());
        let monitor = get("connector").map(|connector| MonitorId {
            connector,
            vendor: get("vendor").unwrap_or_default(),
            product: get("product").unwrap_or_default(),
            serial: get("serial").unwrap_or_default(),
        });
        let window = get("window-app-id").zip(get("window-title"));
        let devices = dict.get("devices").and_then(|v| u32::try_from(v).ok());
        Some(Self { monitor, window, devices })
    }
}

#[derive(Debug, Default)]
pub struct Casts {
    pub(crate) sessions: HashMap<OwnedObjectPath, Cast>,
}

pub type Shared = Arc<Mutex<Casts>>;

pub(crate) fn lock(shared: &Shared) -> MutexGuard<'_, Casts> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn owned(v: Value<'_>) -> OwnedValue {
    OwnedValue::try_from(v).expect("plain values")
}

/// The portal Session object of one screen cast (or remote desktop
/// session).
pub struct CastSession {
    pub(crate) shared: Shared,
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
pub(crate) async fn end(conn: &Connection, shared: &Shared, path: &OwnedObjectPath, notify: bool) {
    let Some(cast) = lock(shared).sessions.remove(path) else { return };
    if let Some(remote) = cast.remote.as_ref().and_then(|r| r.mutter.as_ref()) {
        // Ends its screen cast too.
        let _ = conn
            .call_method(Some(REMOTE), remote.as_str(), Some("org.gnome.Mutter.RemoteDesktop.Session"), "Stop", &())
            .await;
    } else if let Some(mutter) = &cast.mutter {
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

/// Device names for the dialogs.
fn device_names(devices: u32) -> Vec<&'static str> {
    [(1, "keyboard"), (2, "pointer"), (4, "touchscreen")].into_iter().filter(|(bit, _)| devices & bit != 0).map(|(_, n)| n).collect()
}

pub(crate) async fn start(conn: &Connection, shared: &Shared, session: &OwnedObjectPath) -> (u32, Results) {
    let (app_id, cursor_mode, types, persist_mode, restore, remote, sources_selected) = match lock(shared).sessions.get(session) {
        Some(c) => (c.app_id.clone(), c.cursor_mode, c.types, c.persist_mode, c.restore.clone(), c.remote.as_ref().map(|r| r.devices), c.sources_selected),
        None => return (response::OTHER, Results::new()),
    };
    let types = if remote.is_some() && !sources_selected { 0 } else { types };
    // Remote control may be restored when every device type asked for was
    // allowed before (restore data without devices is a screen cast's).
    let devices_remembered = match (remote, restore.as_ref().and_then(|r| r.devices)) {
        (Some(asked), Some(allowed)) => asked & !allowed == 0,
        (Some(_), None) => false,
        (None, _) => true,
    };
    let on = if types & SOURCE_MONITOR != 0 {
        match monitors(conn).await {
            Ok(m) => m,
            Err(err) => {
                tracing::warn!(%err, "cannot list monitors");
                return (response::OTHER, Results::new());
            }
        }
    } else {
        Vec::new()
    };
    // What to share again without asking.
    let restored: Option<Option<Source>> = match restore.as_ref().filter(|_| devices_remembered) {
        None => None,
        Some(r) if types == 0 => (r.monitor.is_none() && r.window.is_none()).then_some(None),
        Some(r) => {
            if let Some(connector) = r.monitor.as_ref().filter(|_| types & SOURCE_MONITOR != 0).and_then(|m| m.find_in(&on)) {
                Some(Some(Source::Monitor(connector)))
            } else if let Some((app, title)) = r.window.as_ref().filter(|_| types & SOURCE_WINDOW != 0) {
                find_window(conn, app, title).await.map(|w| Some(Source::Window(w)))
            } else {
                None
            }
        }
    };
    let source: Option<Source> = match restored {
        Some(source) => {
            tracing::info!(app_id, ?source, "restoring what was shared before");
            source
        }
        // Only a virtual monitor: nothing that exists is shown, so no asking.
        None if types == SOURCE_VIRTUAL => Some(Source::Virtual),
        None if types == 0 => {
            // A remote desktop session without screen sharing: just ask.
            let devices = remote.unwrap_or(0);
            let allowed = conn
                .call_method(Some(SHELL), CHOOSER_PATH, Some(CHOOSER), "AllowRemoteControl", &(app_id.as_str(), device_names(devices)))
                .await
                .and_then(|r| r.body().deserialize::<bool>());
            match allowed {
                Ok(true) => None,
                Ok(false) => return (response::CANCELLED, Results::new()),
                Err(err) => {
                    tracing::warn!(%err, "cannot ask about remote control");
                    return (response::OTHER, Results::new());
                }
            }
        }
        None => {
            let offered: Vec<(String, String)> = on.iter().map(|(m, name)| (m.connector.clone(), name.clone())).collect();
            let mut options = HashMap::from([
                ("monitors", Value::from(types & SOURCE_MONITOR != 0)),
                ("windows", Value::from(types & SOURCE_WINDOW != 0)),
                ("remember", Value::from(persist_mode != 0)),
            ]);
            if let Some(devices) = remote {
                options.insert("control", Value::from(device_names(devices)));
            }
            let choice = conn
                .call_method(Some(SHELL), CHOOSER_PATH, Some(CHOOSER), "ChooseSource", &(app_id.as_str(), &offered, options))
                .await
                .and_then(|r| r.body().deserialize::<(bool, u32, String, u32)>());
            match choice {
                Ok((true, SOURCE_MONITOR, connector, _)) => Some(Source::Monitor(connector)),
                Ok((true, SOURCE_WINDOW, _, window)) => Some(Source::Window(window)),
                Ok(_) => return (response::CANCELLED, Results::new()),
                Err(err) => {
                    tracing::warn!(%err, "cannot ask what to share");
                    return (response::OTHER, Results::new());
                }
            }
        }
    };
    // What to remember for next time.
    let remembered = if persist_mode == 0 {
        None
    } else {
        let mut r = Remembered { devices: remote, ..Remembered::default() };
        match &source {
            Some(Source::Monitor(connector)) => r.monitor = on.iter().find(|(m, _)| m.connector == *connector).map(|(m, _)| m.clone()),
            Some(Source::Window(id)) => r.window = describe_window(conn, *id).await,
            _ => {}
        }
        // Nothing identifiable shared (and not remote control): nothing to remember.
        (r.monitor.is_some() || r.window.is_some() || (remote.is_some() && source.is_none())).then_some(r)
    };
    let remember = |results: &mut Results| match &remembered {
        Some(r) => {
            results.insert("persist_mode".to_owned(), owned(Value::from(persist_mode)));
            results.insert("restore_data".to_owned(), owned(Value::from(r.restore_data())));
        }
        None => {
            results.insert("persist_mode".to_owned(), owned(Value::from(0u32)));
        }
    };
    if let Some(devices) = remote {
        return match start_remote(conn, shared, session, devices, source.as_ref(), cursor_mode).await {
            Ok(mut results) => {
                remember(&mut results);
                (response::SUCCESS, results)
            }
            Err(err) => {
                tracing::warn!(%err, ?source, "cannot start remote control");
                end(conn, shared, session, true).await;
                (response::OTHER, Results::new())
            }
        };
    }
    let Some(source) = source else { return (response::OTHER, Results::new()) };
    match record(conn, shared, session, &source, cursor_mode, None).await {
        Ok(stream) => {
            let mut results = HashMap::from([("streams".to_owned(), owned(Value::from(vec![stream])))]);
            remember(&mut results);
            (response::SUCCESS, results)
        }
        Err(err) => {
            tracing::warn!(%err, ?source, "cannot start the screen cast");
            end(conn, shared, session, true).await;
            (response::OTHER, Results::new())
        }
    }
}

const CHOOSER: &str = "org.helix.ScreenCastChooser";

/// The open window with `app_id` and `title` (the shell knows them).
async fn find_window(conn: &Connection, app_id: &str, title: &str) -> Option<u32> {
    let reply = conn.call_method(Some(SHELL), CHOOSER_PATH, Some(CHOOSER), "FindWindow", &(app_id, title)).await.ok()?;
    let (found, id): (bool, u32) = reply.body().deserialize().ok()?;
    found.then_some(id)
}

/// A window's app id and title.
async fn describe_window(conn: &Connection, id: u32) -> Option<(String, String)> {
    let reply = conn.call_method(Some(SHELL), CHOOSER_PATH, Some(CHOOSER), "DescribeWindow", &(id,)).await.ok()?;
    reply.body().deserialize().ok()
}

/// Record `connector` through the compositor; the stream for the app.
async fn record(
    conn: &Connection,
    shared: &Shared,
    session: &OwnedObjectPath,
    source: &Source,
    cursor_mode: u32,
    remote: Option<(&OwnedObjectPath, &str)>,
) -> zbus::Result<(u32, HashMap<String, OwnedValue>)> {
    let mut create = HashMap::<&str, Value<'_>>::new();
    if let Some((_, id)) = remote {
        create.insert("remote-desktop-session-id", Value::from(id));
    }
    let reply = conn
        .call_method(Some(MUTTER), MUTTER_PATH, Some("org.gnome.Mutter.ScreenCast"), "CreateSession", &(create,))
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
    match remote {
        // The remote desktop session starts its screen cast.
        Some((remote, _)) => {
            conn.call_method(Some(REMOTE), remote.as_str(), Some("org.gnome.Mutter.RemoteDesktop.Session"), "Start", &()).await?;
        }
        None => {
            conn.call_method(Some(MUTTER), mutter.as_str(), Some("org.gnome.Mutter.ScreenCast.Session"), "Start", &()).await?;
        }
    }
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
    if let Some(r) = lock(shared).sessions.get_mut(session).and_then(|c| c.remote.as_mut()) {
        r.streams.insert(node, stream.clone());
    }
    tracing::info!(node, ?source, "screen cast stream ready");
    Ok((node, props))
}

/// Start a remote desktop session in the compositor (with a screen cast of
/// `source`, if given); the `Start` results.
async fn start_remote(
    conn: &Connection,
    shared: &Shared,
    session: &OwnedObjectPath,
    devices: u32,
    source: Option<&Source>,
    cursor_mode: u32,
) -> zbus::Result<Results> {
    let reply = conn.call_method(Some(REMOTE), REMOTE_PATH, Some("org.gnome.Mutter.RemoteDesktop"), "CreateSession", &()).await?;
    let remote: OwnedObjectPath = reply.body().deserialize()?;
    let id: String = zbus::Proxy::new(conn, REMOTE, remote.as_str(), "org.gnome.Mutter.RemoteDesktop.Session")
        .await?
        .get_property("SessionId")
        .await?;
    if let Some(r) = lock(shared).sessions.get_mut(session).and_then(|c| c.remote.as_mut()) {
        r.mutter = Some(remote.clone());
        r.granted = devices;
    }
    // The compositor may end it (its creator gone, a monitor unplugged).
    let closed = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.gnome.Mutter.RemoteDesktop.Session")?
        .member("Closed")?
        .path(remote.clone())?
        .build();
    let mut closed = zbus::MessageStream::for_match_rule(closed, conn, None).await?;
    let mut results = HashMap::from([
        ("devices".to_owned(), owned(Value::from(devices))),
        ("clipboard_enabled".to_owned(), owned(Value::from(false))),
    ]);
    match source {
        Some(source) => {
            let stream = record(conn, shared, session, source, cursor_mode, Some((&remote, &id))).await?;
            results.insert("streams".to_owned(), owned(Value::from(vec![stream])));
        }
        None => {
            conn.call_method(Some(REMOTE), remote.as_str(), Some("org.gnome.Mutter.RemoteDesktop.Session"), "Start", &()).await?;
        }
    }
    let requested = lock(shared).sessions.get(session).and_then(|c| c.remote.as_ref()).is_some_and(|r| r.clipboard_requested);
    if requested && crate::clipboard::enable(conn, shared, session, &remote).await {
        results.insert("clipboard_enabled".to_owned(), owned(Value::from(true)));
    }
    {
        let (conn, shared, session) = (conn.clone(), shared.clone(), session.clone());
        tokio::spawn(async move {
            if closed.next().await.is_some() {
                if let Some(r) = lock(&shared).sessions.get_mut(&session).and_then(|c| c.remote.as_mut()) {
                    r.mutter = None;
                }
                end(&conn, &shared, &session, true).await;
            }
        });
    }
    tracing::info!(%session, devices, "remote control started");
    Ok(results)
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
        let restore = options.get("restore_data").and_then(Remembered::from_restore_data);
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
                cast.sources_selected = true;
                cast.cursor_mode = cursor_mode;
                cast.types = types & known;
                // A remote desktop session remembers through SelectDevices.
                if cast.remote.is_none() {
                    cast.persist_mode = persist_mode;
                    cast.restore = restore;
                }
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
        let round_trip = |r: &Remembered| {
            let (vendor, version, data) = r.restore_data();
            let value = OwnedValue::try_from(Value::from((vendor, version, data))).unwrap();
            Remembered::from_restore_data(&value)
        };
        let m = Remembered { monitor: Some(monitor.clone()), ..Remembered::default() };
        assert_eq!(round_trip(&m), Some(m.clone()));
        let w = Remembered { window: Some(("org.a.App".into(), "Doc".into())), devices: Some(3), ..Remembered::default() };
        assert_eq!(round_trip(&w), Some(w.clone()));
        let d = Remembered { devices: Some(7), ..Remembered::default() };
        assert_eq!(round_trip(&d), Some(d.clone()));
        assert_eq!(Remembered::from_restore_data(&Value::from(("GNOME", 1u32, Value::from(1u32)))), None, "not ours");
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
