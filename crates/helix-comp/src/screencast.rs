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
/// Only Helix's portal backend may record the screen (and the shell, for
/// screen recording).
pub const ALLOWED_CALLER: &str = "org.freedesktop.impl.portal.desktop.helix";
pub const SHELL: &str = "org.gnome.Shell";

/// What a stream records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CastSource {
    /// A monitor, by connector.
    Monitor(String),
    /// A window, by its id in the window list.
    Window(u32),
    /// A rectangle (layout coordinates), on one monitor or several.
    Area { x: i32, y: i32, width: i32, height: i32 },
    /// A virtual monitor made for the stream, by its name.
    Virtual(String),
}

/// A virtual monitor: name, position and size (layout coordinates).
pub type VirtualMonitor = (String, (i32, i32), (i32, i32));
/// A rectangle: x, y, width, height.
pub type Rect = (i32, i32, i32, i32);

/// `cursor-mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CursorMode {
    #[default]
    Hidden,
    Embedded,
    /// PipeWire cursor metadata.
    Metadata,
}

/// Ask the compositor to start or stop recording, or about a window.
#[derive(Debug)]
pub enum CastRequest {
    /// Reply: the PipeWire node id.
    Start { stream: u32, source: CastSource, cursor: CursorMode, reply: oneshot::Sender<Result<u32, String>> },
    /// Make a virtual monitor for `stream`; reply: its name, position and
    /// size (layout coordinates).
    CreateVirtual { stream: u32, reply: oneshot::Sender<Result<VirtualMonitor, String>> },
    Stop { stream: u32 },
    /// A window's size in layout coordinates, if it exists.
    WindowSize { window: u32, reply: oneshot::Sender<Option<(i32, i32)>> },
    /// Input from a remote desktop session.
    Input(crate::remote_desktop::RemoteInput),
    /// A socket to an EIS server for remote desktop session `session`
    /// offering `devices` (1 keyboard, 2 pointer, 4 touchscreen).
    ConnectEis { session: u32, devices: u32, reply: oneshot::Sender<Result<std::os::fd::OwnedFd, String>> },
    /// Remote desktop session `session` ended: close its EIS connections.
    EndRemote { session: u32 },
    /// Caps Lock and Num Lock.
    LockKeys { reply: oneshot::Sender<(bool, bool)> },
    /// An input capture session's request.
    Capture(crate::input_capture::CaptureRequest),
    /// Clipboard sharing or keymap request of remote desktop session
    /// `session`.
    Remote {
        session: u32,
        request: crate::remote_desktop::RemoteRequest,
        reply: oneshot::Sender<Result<crate::remote_desktop::RemoteReply, crate::remote_desktop::RemoteError>>,
    },
}

#[derive(Debug)]
struct SessionState {
    owner: String,
    streams: Vec<u32>,
    started: bool,
    /// Belongs to a remote desktop session (starts and stops with it).
    remote: bool,
    /// The shell's own recording (no "Sharing" indicator).
    shell: bool,
}

#[derive(Debug, Clone)]
struct StreamState {
    source: CastSource,
    cursor: CursorMode,
}

#[derive(Debug, Default)]
pub struct Casts {
    sessions: HashMap<u32, SessionState>,
    streams: HashMap<u32, StreamState>,
    /// Virtual monitors, by stream: name and rectangle (part of the layout
    /// for `RecordArea`).
    virtuals: HashMap<u32, (String, Rect)>,
    next: u32,
}

impl Casts {
    fn next_id(&mut self) -> u32 {
        self.next += 1;
        self.next
    }

    fn active(&self) -> bool {
        self.sessions.values().any(|s| s.started && !s.shell)
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

/// Whether the caller (`sender`) owns `name`.
async fn owns(conn: &Connection, name: &str, sender: &str) -> bool {
    match (DBusProxy::new(conn).await, zbus::names::BusName::try_from(name)) {
        (Ok(dbus), Ok(name)) => dbus.get_name_owner(name).await.is_ok_and(|o| o.as_str() == sender),
        _ => false,
    }
}

/// Whether the caller owns [`ALLOWED_CALLER`].
pub async fn check_caller(conn: &Connection, header: &Header<'_>) -> fdo::Result<String> {
    let sender = header.sender().map(|s| s.to_string()).unwrap_or_default();
    if owns(conn, ALLOWED_CALLER, &sender).await {
        Ok(sender)
    } else {
        Err(fdo::Error::AccessDenied("screen casting is restricted".into()))
    }
}

/// [`check_caller`], also allowing the shell: (caller, is the shell).
async fn check_cast_caller(conn: &Connection, header: &Header<'_>) -> fdo::Result<(String, bool)> {
    match check_caller(conn, header).await {
        Ok(sender) => Ok((sender, false)),
        Err(err) => {
            let sender = header.sender().map(|s| s.to_string()).unwrap_or_default();
            if owns(conn, SHELL, &sender).await {
                Ok((sender, true))
            } else {
                Err(err)
            }
        }
    }
}

/// The served root object.
pub struct ScreenCast {
    pub shared: Shared,
    pub layout: Arc<Mutex<Layout>>,
    pub compositor: Option<mpsc::UnboundedSender<CastRequest>>,
    pub remotes: crate::remote_desktop::Shared,
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
        let (owner, shell) = check_cast_caller(conn, &header).await?;
        let remote = match properties.get("remote-desktop-session-id").map(|v| &**v) {
            Some(Value::Str(id)) => Some(id.to_string()),
            Some(_) => return Err(fdo::Error::InvalidArgs("remote-desktop-session-id (s)".into())),
            None => None,
        };
        let compositor = self.compositor.clone().ok_or_else(|| fdo::Error::NotSupported("cannot record".into()))?;
        let id = {
            let mut casts = lock(&self.shared);
            let id = casts.next_id();
            casts.sessions.insert(id, SessionState { owner: owner.clone(), streams: Vec::new(), started: false, remote: remote.is_some(), shell });
            id
        };
        if let Some(remote) = remote {
            if !crate::remote_desktop::attach_cast(&self.remotes, &remote, id) {
                lock(&self.shared).sessions.remove(&id);
                return Err(fdo::Error::InvalidArgs(format!("no remote desktop session {remote}")));
            }
        }
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

/// Start session `id`'s streams.
pub async fn start_session(conn: &Connection, shared: &Shared, compositor: &mpsc::UnboundedSender<CastRequest>, id: u32) -> fdo::Result<()> {
    let streams: Vec<(u32, StreamState)> = {
        let mut casts = lock(shared);
        let session = casts.sessions.get_mut(&id).ok_or_else(|| fdo::Error::Failed("session closed".into()))?;
        if session.started {
            return Err(fdo::Error::Failed("already started".into()));
        }
        session.started = true;
        let ids = session.streams.clone();
        ids.into_iter().filter_map(|id| casts.streams.get(&id).cloned().map(|s| (id, s))).collect()
    };
    active_changed(conn).await;
    for (stream_id, stream) in streams {
        let (reply, answer) = oneshot::channel();
        let request = CastRequest::Start { stream: stream_id, source: stream.source.clone(), cursor: stream.cursor, reply };
        if compositor.send(request).is_err() {
            return Err(fdo::Error::Failed("the compositor is gone".into()));
        }
        let node = match answer.await {
            Ok(Ok(node)) => node,
            failed => {
                let err = match failed {
                    Ok(Err(err)) => err,
                    _ => "the compositor did not answer".to_owned(),
                };
                tracing::warn!(err, source = ?stream.source, "cannot record");
                close_session(conn, shared, compositor, id).await;
                return Err(fdo::Error::Failed(format!("cannot record: {err}")));
            }
        };
        let path = stream_path(stream_id);
        if let Ok(emitter) = SignalEmitter::new(conn, path.as_str()) {
            let _ = Stream::pipe_wire_stream_added(&emitter, node).await;
        }
        tracing::info!(stream = stream_id, node, source = ?stream.source, "screen cast started");
    }
    Ok(())
}

/// The stream id of a stream object path, if it is one of session `id`'s.
pub fn stream_of_session(shared: &Shared, id: u32, path: &str) -> Option<u32> {
    let stream: u32 = path.strip_prefix(&format!("{OBJECT_PATH}/Stream/u"))?.parse().ok()?;
    (lock(shared).session_of(stream) == Some(id)).then_some(stream)
}

/// End session `id`: stop its streams, emit `Closed`, remove the objects.
pub async fn close_session(conn: &Connection, shared: &Shared, compositor: &mpsc::UnboundedSender<CastRequest>, id: u32) {
    let Some(session) = lock(shared).sessions.remove(&id) else { return };
    for stream in &session.streams {
        let _ = compositor.send(CastRequest::Stop { stream: *stream });
        lock(shared).streams.remove(stream);
        lock(shared).virtuals.remove(stream);
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

fn cursor_mode(properties: &HashMap<String, OwnedValue>) -> fdo::Result<CursorMode> {
    match properties.get("cursor-mode").map(|v| &**v) {
        None | Some(Value::U32(0)) => Ok(CursorMode::Hidden),
        Some(Value::U32(1)) => Ok(CursorMode::Embedded),
        Some(Value::U32(2)) => Ok(CursorMode::Metadata),
        _ => Err(fdo::Error::InvalidArgs("unsupported cursor mode".into())),
    }
}

/// `rect` (x, y, w, h) clipped to the monitor (connector, its rectangle)
/// it mostly covers.
/// `rect` clipped to the layout's bounding box, with the monitor it
/// covers most; `None` when it is on no monitor.
pub fn clip_to_layout(rect: Rect, monitors: &[(String, Rect)]) -> Option<(String, Rect)> {
    let (connector, _) = clip_to_monitor(rect, monitors)?;
    let (x0, y0) = monitors.iter().fold((i32::MAX, i32::MAX), |(x, y), (_, m)| (x.min(m.0), y.min(m.1)));
    let (x1, y1) = monitors.iter().fold((i32::MIN, i32::MIN), |(x, y), (_, m)| (x.max(m.0 + m.2), y.max(m.1 + m.3)));
    let (x, y) = (rect.0.max(x0), rect.1.max(y0));
    let (right, bottom) = ((rect.0 + rect.2).min(x1), (rect.1 + rect.3).min(y1));
    Some((connector, (x, y, right - x, bottom - y)))
}

pub fn clip_to_monitor(rect: Rect, monitors: &[(String, Rect)]) -> Option<(String, Rect)> {
    let (x, y, w, h) = rect;
    if w <= 0 || h <= 0 {
        return None;
    }
    let overlap = |(mx, my, mw, mh): (i32, i32, i32, i32)| {
        let (x0, y0) = (x.max(mx), y.max(my));
        let (x1, y1) = ((x + w).min(mx + mw), (y + h).min(my + mh));
        (x1 > x0 && y1 > y0).then_some((x0, y0, x1 - x0, y1 - y0))
    };
    monitors
        .iter()
        .filter_map(|(c, m)| overlap(*m).map(|o| (c.clone(), o)))
        .max_by_key(|(_, (_, _, w, h))| i64::from(*w) * i64::from(*h))
}

impl Session {
    /// Add a stream to the (not yet started) session.
    async fn add_stream(
        &self,
        source: CastSource,
        cursor: CursorMode,
        parameters: HashMap<String, OwnedValue>,
        server: &zbus::ObjectServer,
    ) -> fdo::Result<OwnedObjectPath> {
        let id = self.reserve_stream()?;
        self.add_stream_as(id, source, cursor, parameters, server).await
    }

    /// A stream id for a session that is not started yet.
    fn reserve_stream(&self) -> fdo::Result<u32> {
        let mut casts = lock(&self.shared);
        match casts.sessions.get(&self.id).map(|s| s.started) {
            None => Err(fdo::Error::Failed("session closed".into())),
            Some(true) => Err(fdo::Error::Failed("session already started".into())),
            Some(false) => Ok(casts.next_id()),
        }
    }

    async fn add_stream_as(
        &self,
        id: u32,
        source: CastSource,
        cursor: CursorMode,
        parameters: HashMap<String, OwnedValue>,
        server: &zbus::ObjectServer,
    ) -> fdo::Result<OwnedObjectPath> {
        let id = {
            let mut casts = lock(&self.shared);
            let started = casts.sessions.get(&self.id).map(|s| s.started);
            match started {
                None => return Err(fdo::Error::Failed("session closed".into())),
                Some(true) => return Err(fdo::Error::Failed("session already started".into())),
                Some(false) => {}
            }
            casts.streams.insert(id, StreamState { source, cursor });
            casts.sessions.get_mut(&self.id).expect("checked above").streams.push(id);
            id
        };
        let path = stream_path(id);
        server.at(path.as_str(), Stream { parameters }).await?;
        OwnedObjectPath::try_from(path).map_err(|e| fdo::Error::Failed(e.to_string()))
    }

    fn is_remote(&self) -> bool {
        lock(&self.shared).sessions.get(&self.id).is_some_and(|s| s.remote)
    }

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
        if self.is_remote() {
            return Err(fdo::Error::Failed("start the remote desktop session instead".into()));
        }
        start_session(conn, &self.shared, &self.compositor, self.id).await
    }

    async fn stop(&self, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> fdo::Result<()> {
        self.check_owner(&header)?;
        if self.is_remote() {
            return Err(fdo::Error::Failed("stop the remote desktop session instead".into()));
        }
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
        let cursor = cursor_mode(&properties)?;
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
        self.add_stream(CastSource::Monitor(connector.to_owned()), cursor, parameters, server).await
    }

    async fn record_window(
        &self,
        properties: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> fdo::Result<OwnedObjectPath> {
        self.check_owner(&header)?;
        let cursor = cursor_mode(&properties)?;
        let window = match properties.get("window-id").map(|v| &**v) {
            Some(Value::U64(id)) => u32::try_from(*id).map_err(|_| fdo::Error::InvalidArgs("unknown window".into()))?,
            _ => return Err(fdo::Error::InvalidArgs("window-id (t) is required".into())),
        };
        let (reply, answer) = oneshot::channel();
        self.compositor
            .send(CastRequest::WindowSize { window, reply })
            .map_err(|_| fdo::Error::Failed("the compositor is gone".into()))?;
        let (w, h) = answer.await.ok().flatten().ok_or_else(|| fdo::Error::InvalidArgs(format!("no window {window}")))?;
        let owned = |v: Value<'_>| OwnedValue::try_from(v).expect("plain values");
        let parameters = HashMap::from([("size".to_owned(), owned(Value::from((w, h))))]);
        self.add_stream(CastSource::Window(window), cursor, parameters, server).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn record_area(
        &self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        properties: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> fdo::Result<OwnedObjectPath> {
        self.check_owner(&header)?;
        let cursor = cursor_mode(&properties)?;
        let monitors: Vec<(String, Rect)> = {
            let layout = self.layout.lock().unwrap_or_else(|e| e.into_inner());
            layout
                .monitors
                .iter()
                .filter_map(|m| {
                    let mode = m.modes.iter().find(|mode| mode.current)?;
                    let (w, h) = crate::display_config::logical_size(mode.width, mode.height, m.scale, m.transform);
                    Some((m.connector.clone(), (m.x, m.y, w, h)))
                })
                .collect()
        };
        let mut monitors = monitors;
        monitors.extend(lock(&self.shared).virtuals.values().cloned());
        let (connector, (cx, cy, cw, ch)) = clip_to_layout((x, y, width, height), &monitors)
            .ok_or_else(|| fdo::Error::InvalidArgs("the area is on no monitor".into()))?;
        let parameters = stream_parameters((cx, cy), (cw, ch), &connector);
        let source = CastSource::Area { x: cx, y: cy, width: cw, height: ch };
        self.add_stream(source, cursor, parameters, server).await
    }

    async fn record_virtual(
        &self,
        properties: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> fdo::Result<OwnedObjectPath> {
        self.check_owner(&header)?;
        let cursor = cursor_mode(&properties)?;
        let id = self.reserve_stream()?;
        let (reply, answer) = oneshot::channel();
        self.compositor
            .send(CastRequest::CreateVirtual { stream: id, reply })
            .map_err(|_| fdo::Error::Failed("the compositor is gone".into()))?;
        let (name, position, size) = answer
            .await
            .map_err(|_| fdo::Error::Failed("the compositor did not answer".into()))?
            .map_err(fdo::Error::Failed)?;
        let parameters = stream_parameters(position, size, &name);
        lock(&self.shared).virtuals.insert(id, (name.clone(), (position.0, position.1, size.0, size.1)));
        self.add_stream_as(id, CastSource::Virtual(name), cursor, parameters, server).await
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

#[cfg(test)]
mod clip_tests {
    use super::*;

    #[test]
    fn clipping() {
        let monitors = vec![("A".to_owned(), (0, 0, 1920, 1080)), ("B".to_owned(), (1920, 0, 1280, 1024))];
        assert_eq!(clip_to_monitor((100, 100, 200, 100), &monitors), Some(("A".into(), (100, 100, 200, 100))));
        assert_eq!(clip_to_monitor((1800, 0, 400, 100), &monitors), Some(("B".into(), (1920, 0, 280, 100))), "mostly on B");
        assert_eq!(clip_to_monitor((5000, 0, 10, 10), &monitors), None);
        assert_eq!(clip_to_monitor((0, 0, 0, 10), &monitors), None);
        // Across monitors: kept whole within the layout.
        assert_eq!(clip_to_layout((1800, 0, 400, 100), &monitors), Some(("B".into(), (1800, 0, 400, 100))));
        assert_eq!(clip_to_layout((-50, -50, 4000, 2000), &monitors), Some(("A".into(), (0, 0, 3200, 1080))));
        assert_eq!(clip_to_layout((5000, 0, 10, 10), &monitors), None);
    }
}
