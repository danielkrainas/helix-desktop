//! `org.gnome.Mutter.ScreenCast` (helix-desktop-spec
//! `org.gnome.Mutter.ScreenCast.md`): sessions and streams on D-Bus; the
//! compositor records monitors into PipeWire streams when asked with
//! [`CastRequest`]s.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::{mpsc, oneshot};
use zbus::fdo::{self, DBusProxy};
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{interface, Connection};

use crate::display_config::Layout;

pub const BUS_NAME: &str = "org.gnome.Mutter.ScreenCast";
pub const OBJECT_PATH: &str = "/org/gnome/Mutter/ScreenCast";
pub const VERSION: i32 = 2;
/// Only Helix's portal backend may record the screen.
pub const ALLOWED_CALLER: &str = "org.freedesktop.impl.portal.desktop.helix";

/// Ask the compositor to start or stop recording a monitor.
#[derive(Debug)]
pub enum CastRequest {
    /// Reply: the PipeWire node id.
    Start { stream: u32, connector: String, cursor: bool, reply: oneshot::Sender<Result<u32, String>> },
    Stop { stream: u32 },
}

#[derive(Debug)]
struct SessionState {
    owner: String,
    streams: Vec<u32>,
    started: bool,
}

#[derive(Debug, Clone)]
struct StreamState {
    connector: String,
    cursor: bool,
}

#[derive(Debug, Default)]
pub struct Casts {
    sessions: HashMap<u32, SessionState>,
    streams: HashMap<u32, StreamState>,
    next: u32,
}

impl Casts {
    fn next_id(&mut self) -> u32 {
        self.next += 1;
        self.next
    }

    fn active(&self) -> bool {
        self.sessions.values().any(|s| s.started)
    }

    /// The session a stream belongs to.
    pub fn session_of(&self, stream: u32) -> Option<u32> {
        self.sessions.iter().find(|(_, s)| s.streams.contains(&stream)).map(|(id, _)| *id)
    }

    /// Sessions created by `owner`.
    pub fn sessions_of(&self, owner: &str) -> Vec<u32> {
        self.sessions.iter().filter(|(_, s)| s.owner == owner).map(|(id, _)| *id).collect()
    }
}

pub type Shared = Arc<Mutex<Casts>>;

fn lock(shared: &Shared) -> MutexGuard<'_, Casts> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

fn session_path(id: u32) -> String {
    format!("{OBJECT_PATH}/Session/u{id}")
}

fn stream_path(id: u32) -> String {
    format!("{OBJECT_PATH}/Stream/u{id}")
}

/// Whether the caller owns [`ALLOWED_CALLER`].
async fn check_caller(conn: &Connection, header: &Header<'_>) -> fdo::Result<String> {
    let sender = header.sender().map(|s| s.to_string()).unwrap_or_default();
    let allowed = match DBusProxy::new(conn).await {
        Ok(dbus) => match zbus::names::BusName::try_from(ALLOWED_CALLER) {
            Ok(name) => dbus.get_name_owner(name).await.is_ok_and(|o| o.as_str() == sender),
            Err(_) => false,
        },
        Err(_) => false,
    };
    if allowed {
        Ok(sender)
    } else {
        Err(fdo::Error::AccessDenied("screen casting is restricted".into()))
    }
}

/// The served root object.
pub struct ScreenCast {
    pub shared: Shared,
    pub layout: Arc<Mutex<Layout>>,
    pub compositor: Option<mpsc::UnboundedSender<CastRequest>>,
}

#[interface(name = "org.gnome.Mutter.ScreenCast")]
impl ScreenCast {
    async fn create_session(
        &self,
        properties: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> fdo::Result<OwnedObjectPath> {
        let owner = check_caller(conn, &header).await?;
        if properties.contains_key("remote-desktop-session-id") {
            return Err(fdo::Error::NotSupported("no remote desktop sessions".into()));
        }
        let compositor = self.compositor.clone().ok_or_else(|| fdo::Error::NotSupported("cannot record".into()))?;
        let id = {
            let mut casts = lock(&self.shared);
            let id = casts.next_id();
            casts.sessions.insert(id, SessionState { owner: owner.clone(), streams: Vec::new(), started: false });
            id
        };
        let path = session_path(id);
        let session = Session { id, shared: self.shared.clone(), layout: self.layout.clone(), compositor, owner };
        server.at(path.as_str(), session).await?;
        tracing::info!(path, "screen cast session created");
        OwnedObjectPath::try_from(path).map_err(|e| fdo::Error::Failed(e.to_string()))
    }

    #[zbus(property)]
    async fn version(&self) -> i32 {
        VERSION
    }
}

/// `org.helix.ScreenCastState` on the root object.
pub struct ScreenCastState {
    pub shared: Shared,
}

#[interface(name = "org.helix.ScreenCastState")]
impl ScreenCastState {
    #[zbus(property)]
    async fn active(&self) -> bool {
        lock(&self.shared).active()
    }
}

async fn active_changed(conn: &Connection) {
    if let Ok(iface) = conn.object_server().interface::<_, ScreenCastState>(OBJECT_PATH).await {
        let _ = iface.get().await.active_changed(iface.signal_emitter()).await;
    }
}

/// End session `id`: stop its streams, emit `Closed`, remove the objects.
pub async fn close_session(conn: &Connection, shared: &Shared, compositor: &mpsc::UnboundedSender<CastRequest>, id: u32) {
    let Some(session) = lock(shared).sessions.remove(&id) else { return };
    for stream in &session.streams {
        let _ = compositor.send(CastRequest::Stop { stream: *stream });
        lock(shared).streams.remove(stream);
        let _ = conn.object_server().remove::<Stream, _>(stream_path(*stream).as_str()).await;
    }
    let path = session_path(id);
    if let Ok(emitter) = SignalEmitter::new(conn, path.as_str()) {
        let _ = Session::closed(&emitter).await;
    }
    let _ = conn.object_server().remove::<Session, _>(path.as_str()).await;
    tracing::info!(path, "screen cast session closed");
    active_changed(conn).await;
}

pub struct Session {
    id: u32,
    shared: Shared,
    layout: Arc<Mutex<Layout>>,
    compositor: mpsc::UnboundedSender<CastRequest>,
    owner: String,
}

impl Session {
    fn check_owner(&self, header: &Header<'_>) -> fdo::Result<()> {
        if header.sender().is_some_and(|s| s.as_str() == self.owner) {
            Ok(())
        } else {
            Err(fdo::Error::AccessDenied("not your session".into()))
        }
    }
}

#[interface(name = "org.gnome.Mutter.ScreenCast.Session")]
impl Session {
    async fn start(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<()> {
        self.check_owner(&header)?;
        let streams: Vec<(u32, StreamState)> = {
            let mut casts = lock(&self.shared);
            let session = casts.sessions.get_mut(&self.id).ok_or_else(|| fdo::Error::Failed("session closed".into()))?;
            if session.started {
                return Err(fdo::Error::Failed("already started".into()));
            }
            session.started = true;
            let ids = session.streams.clone();
            ids.into_iter().filter_map(|id| casts.streams.get(&id).cloned().map(|s| (id, s))).collect()
        };
        active_changed(conn).await;
        for (id, stream) in streams {
            let (reply, answer) = oneshot::channel();
            let request = CastRequest::Start { stream: id, connector: stream.connector.clone(), cursor: stream.cursor, reply };
            if self.compositor.send(request).is_err() {
                return Err(fdo::Error::Failed("the compositor is gone".into()));
            }
            let node = match answer.await {
                Ok(Ok(node)) => node,
                failed => {
                    let err = match failed {
                        Ok(Err(err)) => err,
                        _ => "the compositor did not answer".to_owned(),
                    };
                    tracing::warn!(err, connector = stream.connector, "cannot record the monitor");
                    close_session(conn, &self.shared, &self.compositor, self.id).await;
                    return Err(fdo::Error::Failed(format!("cannot record the monitor: {err}")));
                }
            };
            let path = stream_path(id);
            if let Ok(emitter) = SignalEmitter::new(conn, path.as_str()) {
                let _ = Stream::pipe_wire_stream_added(&emitter, node).await;
            }
            tracing::info!(stream = id, node, connector = stream.connector, "screen cast started");
        }
        Ok(())
    }

    async fn stop(&self, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> fdo::Result<()> {
        self.check_owner(&header)?;
        close_session(conn, &self.shared, &self.compositor, self.id).await;
        Ok(())
    }

    #[zbus(signal)]
    async fn closed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    async fn record_monitor(
        &self,
        connector: &str,
        properties: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> fdo::Result<OwnedObjectPath> {
        self.check_owner(&header)?;
        let cursor = match properties.get("cursor-mode").map(|v| &**v) {
            None | Some(Value::U32(0)) => false,
            Some(Value::U32(1)) => true,
            _ => return Err(fdo::Error::InvalidArgs("unsupported cursor mode".into())),
        };
        let parameters = {
            let layout = self.layout.lock().unwrap_or_else(|e| e.into_inner());
            let monitor = layout
                .monitors
                .iter()
                .find(|m| m.connector == connector && m.modes.iter().any(|mode| mode.current))
                .ok_or_else(|| fdo::Error::InvalidArgs(format!("no monitor {connector}")))?;
            let mode = monitor.modes.iter().find(|m| m.current).expect("checked above");
            let (w, h) = crate::display_config::logical_size(mode.width, mode.height, monitor.scale, monitor.transform);
            stream_parameters((monitor.x, monitor.y), (w, h), connector)
        };
        let id = {
            let mut casts = lock(&self.shared);
            let started = casts.sessions.get(&self.id).map(|s| s.started);
            match started {
                None => return Err(fdo::Error::Failed("session closed".into())),
                Some(true) => return Err(fdo::Error::Failed("session already started".into())),
                Some(false) => {}
            }
            let id = casts.next_id();
            casts.streams.insert(id, StreamState { connector: connector.to_owned(), cursor });
            casts.sessions.get_mut(&self.id).expect("checked above").streams.push(id);
            id
        };
        let path = stream_path(id);
        server.at(path.as_str(), Stream { parameters }).await?;
        OwnedObjectPath::try_from(path).map_err(|e| fdo::Error::Failed(e.to_string()))
    }

    async fn record_window(&self, _properties: HashMap<String, OwnedValue>) -> fdo::Result<OwnedObjectPath> {
        Err(fdo::Error::NotSupported("recording windows is not supported".into()))
    }

    async fn record_area(
        &self,
        _x: i32,
        _y: i32,
        _width: i32,
        _height: i32,
        _properties: HashMap<String, OwnedValue>,
    ) -> fdo::Result<OwnedObjectPath> {
        Err(fdo::Error::NotSupported("recording areas is not supported".into()))
    }

    async fn record_virtual(&self, _properties: HashMap<String, OwnedValue>) -> fdo::Result<OwnedObjectPath> {
        Err(fdo::Error::NotSupported("virtual monitors are not supported".into()))
    }
}

fn stream_parameters(position: (i32, i32), size: (i32, i32), connector: &str) -> HashMap<String, OwnedValue> {
    let owned = |v: Value<'_>| OwnedValue::try_from(v).expect("plain values");
    HashMap::from([
        ("position".to_owned(), owned(Value::from(position))),
        ("size".to_owned(), owned(Value::from(size))),
        ("output-name".to_owned(), owned(Value::from(connector))),
    ])
}

pub struct Stream {
    parameters: HashMap<String, OwnedValue>,
}

#[interface(name = "org.gnome.Mutter.ScreenCast.Stream")]
impl Stream {
    async fn start(&self) -> fdo::Result<()> {
        Err(fdo::Error::NotSupported("start the session instead".into()))
    }

    async fn stop(&self) -> fdo::Result<()> {
        Err(fdo::Error::NotSupported("stop the session instead".into()))
    }

    #[zbus(signal, name = "PipeWireStreamAdded")]
    async fn pipe_wire_stream_added(emitter: &SignalEmitter<'_>, node_id: u32) -> zbus::Result<()>;

    #[zbus(property)]
    async fn parameters(&self) -> HashMap<String, OwnedValue> {
        self.parameters.iter().map(|(k, v)| (k.clone(), v.try_clone().expect("plain values"))).collect()
    }
}
