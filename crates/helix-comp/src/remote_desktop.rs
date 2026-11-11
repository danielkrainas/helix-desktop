//! `org.gnome.Mutter.RemoteDesktop` (helix-desktop-spec
//! `org.gnome.Mutter.RemoteDesktop.md`): remote desktop sessions inject
//! input into the session (through the compositor), directly or over EIS,
//! and may own a screen cast session.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::{mpsc, oneshot};
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedFd, OwnedObjectPath, OwnedValue, Value};
use zbus::{fdo, interface, Connection};

use crate::screencast::{self, CastRequest};

pub const BUS_NAME: &str = "org.gnome.Mutter.RemoteDesktop";
pub const OBJECT_PATH: &str = "/org/gnome/Mutter/RemoteDesktop";
pub const VERSION: i32 = 1;
/// Keyboard, pointer, touchscreen.
pub const DEVICE_TYPES: u32 = 7;

/// Input a remote desktop session injects. Positions with a `stream` are
/// in that screen cast stream's logical coordinates.
#[derive(Debug, Clone, PartialEq)]
pub enum RemoteInput {
    /// An evdev key code.
    Key { keycode: u32, pressed: bool },
    Keysym { keysym: u32, pressed: bool },
    /// An evdev button code.
    Button { button: u32, pressed: bool },
    Motion { dx: f64, dy: f64 },
    MotionAbsolute { stream: u32, x: f64, y: f64 },
    /// Smooth scrolling in pixels; `source` 0 finger, 1 wheel, 2 continuous.
    Axis { dx: f64, dy: f64, finish: bool, source: u32 },
    /// `axis` 0 vertical, 1 horizontal.
    AxisDiscrete { axis: u32, steps: i32 },
    TouchDown { stream: u32, slot: u32, x: f64, y: f64 },
    TouchMotion { stream: u32, slot: u32, x: f64, y: f64 },
    TouchUp { slot: u32 },
}

#[derive(Debug)]
struct SessionState {
    owner: String,
    /// `SessionId`.
    name: String,
    started: bool,
    /// Its screen cast session, if any.
    cast: Option<u32>,
}

#[derive(Debug, Default)]
pub struct Remotes {
    sessions: HashMap<u32, SessionState>,
    next: u32,
}

impl Remotes {
    /// Sessions created by `owner`.
    pub fn sessions_of(&self, owner: &str) -> Vec<u32> {
        self.sessions.iter().filter(|(_, s)| s.owner == owner).map(|(id, _)| *id).collect()
    }
}

pub type Shared = Arc<Mutex<Remotes>>;

fn lock(shared: &Shared) -> MutexGuard<'_, Remotes> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

fn session_path(id: u32) -> String {
    format!("{OBJECT_PATH}/Session/u{id}")
}

/// Tie screen cast session `cast` to the remote desktop session named
/// `name` (not started yet); false if there is none.
pub fn attach_cast(shared: &Shared, name: &str, cast: u32) -> bool {
    let mut remotes = lock(shared);
    match remotes.sessions.values_mut().find(|s| s.name == name && !s.started && s.cast.is_none()) {
        Some(session) => {
            session.cast = Some(cast);
            true
        }
        None => false,
    }
}

/// The served root object.
pub struct RemoteDesktop {
    pub shared: Shared,
    pub casts: screencast::Shared,
    pub compositor: Option<mpsc::UnboundedSender<CastRequest>>,
}

#[interface(name = "org.gnome.Mutter.RemoteDesktop")]
impl RemoteDesktop {
    async fn create_session(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> fdo::Result<OwnedObjectPath> {
        let owner = screencast::check_caller(conn, &header).await?;
        let compositor = self.compositor.clone().ok_or_else(|| fdo::Error::NotSupported("no remote input".into()))?;
        let id = {
            let mut remotes = lock(&self.shared);
            remotes.next += 1;
            let id = remotes.next;
            let name = format!("helix-remote-{}-{id}", std::process::id());
            remotes.sessions.insert(id, SessionState { owner: owner.clone(), name, started: false, cast: None });
            id
        };
        let path = session_path(id);
        let session = Session { id, shared: self.shared.clone(), casts: self.casts.clone(), compositor, owner };
        server.at(path.as_str(), session).await?;
        tracing::info!(path, "remote desktop session created");
        OwnedObjectPath::try_from(path).map_err(|e| fdo::Error::Failed(e.to_string()))
    }

    #[zbus(property)]
    async fn supported_device_types(&self) -> u32 {
        DEVICE_TYPES
    }

    #[zbus(property)]
    async fn version(&self) -> i32 {
        VERSION
    }
}

/// End remote desktop session `id` (and its screen cast).
pub async fn close_session(
    conn: &Connection,
    shared: &Shared,
    casts: &screencast::Shared,
    compositor: &mpsc::UnboundedSender<CastRequest>,
    id: u32,
) {
    let Some(session) = lock(shared).sessions.remove(&id) else { return };
    let _ = compositor.send(CastRequest::EndRemote { session: id });
    if let Some(cast) = session.cast {
        screencast::close_session(conn, casts, compositor, cast).await;
    }
    let path = session_path(id);
    if let Ok(emitter) = SignalEmitter::new(conn, path.as_str()) {
        let _ = Session::closed(&emitter).await;
    }
    let _ = conn.object_server().remove::<Session, _>(path.as_str()).await;
    tracing::info!(path, "remote desktop session closed");
}

pub struct Session {
    id: u32,
    shared: Shared,
    casts: screencast::Shared,
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

    /// The session's screen cast session, if it is started.
    fn started(&self) -> fdo::Result<Option<u32>> {
        match lock(&self.shared).sessions.get(&self.id) {
            Some(s) if s.started => Ok(s.cast),
            _ => Err(fdo::Error::Failed("the session is not started".into())),
        }
    }

    fn inject(&self, header: &Header<'_>, input: RemoteInput) -> fdo::Result<()> {
        self.check_owner(header)?;
        self.started()?;
        self.compositor.send(CastRequest::Input(input)).map_err(|_| fdo::Error::Failed("the compositor is gone".into()))
    }

    async fn lock_keys(&self) -> (bool, bool) {
        let (reply, answer) = oneshot::channel();
        if self.compositor.send(CastRequest::LockKeys { reply }).is_err() {
            return (false, false);
        }
        answer.await.unwrap_or((false, false))
    }

    /// The stream id behind a stream object path of this session's cast.
    fn stream(&self, path: &str) -> fdo::Result<u32> {
        let cast = self.started()?.ok_or_else(|| fdo::Error::InvalidArgs("the session has no screen cast".into()))?;
        screencast::stream_of_session(&self.casts, cast, path).ok_or_else(|| fdo::Error::InvalidArgs(format!("unknown stream {path}")))
    }
}

#[interface(name = "org.gnome.Mutter.RemoteDesktop.Session")]
impl Session {
    #[zbus(property)]
    async fn session_id(&self) -> String {
        lock(&self.shared).sessions.get(&self.id).map(|s| s.name.clone()).unwrap_or_default()
    }

    async fn start(&self, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> fdo::Result<()> {
        self.check_owner(&header)?;
        let cast = {
            let mut remotes = lock(&self.shared);
            let session = remotes.sessions.get_mut(&self.id).ok_or_else(|| fdo::Error::Failed("session closed".into()))?;
            if session.started {
                return Err(fdo::Error::Failed("already started".into()));
            }
            session.started = true;
            session.cast
        };
        if let Some(cast) = cast {
            if let Err(err) = screencast::start_session(conn, &self.casts, &self.compositor, cast).await {
                close_session(conn, &self.shared, &self.casts, &self.compositor, self.id).await;
                return Err(err);
            }
        }
        tracing::info!(session = self.id, "remote desktop session started");
        Ok(())
    }

    async fn stop(&self, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> fdo::Result<()> {
        self.check_owner(&header)?;
        close_session(conn, &self.shared, &self.casts, &self.compositor, self.id).await;
        Ok(())
    }

    #[zbus(signal)]
    async fn closed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    async fn notify_keyboard_keycode(&self, keycode: u32, state: bool, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        self.inject(&header, RemoteInput::Key { keycode, pressed: state })
    }

    async fn notify_keyboard_keysym(&self, keysym: u32, state: bool, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        self.inject(&header, RemoteInput::Keysym { keysym, pressed: state })
    }

    async fn notify_pointer_button(&self, button: i32, state: bool, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        let button = u32::try_from(button).map_err(|_| fdo::Error::InvalidArgs("bad button".into()))?;
        self.inject(&header, RemoteInput::Button { button, pressed: state })
    }

    async fn notify_pointer_axis(&self, dx: f64, dy: f64, flags: u32, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        let (finish, source) = axis_flags(flags);
        self.inject(&header, RemoteInput::Axis { dx, dy, finish, source })
    }

    async fn notify_pointer_axis_discrete(&self, axis: u32, steps: i32, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        self.inject(&header, RemoteInput::AxisDiscrete { axis, steps })
    }

    async fn notify_pointer_motion_relative(&self, dx: f64, dy: f64, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        self.inject(&header, RemoteInput::Motion { dx, dy })
    }

    async fn notify_pointer_motion_absolute(&self, stream: &str, x: f64, y: f64, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        self.check_owner(&header)?;
        let stream = self.stream(stream)?;
        self.inject(&header, RemoteInput::MotionAbsolute { stream, x, y })
    }

    async fn notify_touch_down(&self, stream: &str, slot: u32, x: f64, y: f64, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        self.check_owner(&header)?;
        let stream = self.stream(stream)?;
        self.inject(&header, RemoteInput::TouchDown { stream, slot, x, y })
    }

    async fn notify_touch_motion(&self, stream: &str, slot: u32, x: f64, y: f64, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        self.check_owner(&header)?;
        let stream = self.stream(stream)?;
        self.inject(&header, RemoteInput::TouchMotion { stream, slot, x, y })
    }

    async fn notify_touch_up(&self, slot: u32, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        self.inject(&header, RemoteInput::TouchUp { slot })
    }

    #[zbus(name = "ConnectToEIS")]
    async fn connect_to_eis(&self, options: HashMap<String, OwnedValue>, #[zbus(header)] header: Header<'_>) -> fdo::Result<OwnedFd> {
        self.check_owner(&header)?;
        self.started()?;
        let devices = match options.get("device-types").map(|v| &**v) {
            Some(Value::U32(d)) => d & DEVICE_TYPES,
            _ => DEVICE_TYPES,
        };
        let (reply, answer) = oneshot::channel();
        self.compositor
            .send(CastRequest::ConnectEis { session: self.id, devices, reply })
            .map_err(|_| fdo::Error::Failed("the compositor is gone".into()))?;
        let fd = answer
            .await
            .map_err(|_| fdo::Error::Failed("the compositor did not answer".into()))?
            .map_err(fdo::Error::Failed)?;
        Ok(OwnedFd::from(fd))
    }

    #[zbus(property)]
    async fn caps_lock_state(&self) -> bool {
        self.lock_keys().await.0
    }

    #[zbus(property)]
    async fn num_lock_state(&self) -> bool {
        self.lock_keys().await.1
    }

    #[zbus(property)]
    async fn keymap_capabilities(&self) -> HashMap<String, OwnedValue> {
        HashMap::new()
    }

    #[zbus(property)]
    async fn current_keymap(&self) -> HashMap<String, OwnedValue> {
        HashMap::from([("source".to_owned(), OwnedValue::from(0u32))])
    }

    async fn set_keymap(&self, _options: HashMap<String, OwnedValue>) -> fdo::Result<()> {
        Err(fdo::Error::NotSupported("keymaps can't be set".into()))
    }

    async fn set_keymap_layout_index(&self, _index: u32) -> fdo::Result<()> {
        Err(fdo::Error::NotSupported("keymaps can't be set".into()))
    }

    async fn enable_clipboard(&self, _options: HashMap<String, OwnedValue>) -> fdo::Result<()> {
        Err(fdo::Error::NotSupported("no clipboard sharing".into()))
    }

    async fn disable_clipboard(&self) -> fdo::Result<()> {
        Err(fdo::Error::NotSupported("no clipboard sharing".into()))
    }

    async fn set_selection(&self, _options: HashMap<String, OwnedValue>) -> fdo::Result<()> {
        Err(fdo::Error::NotSupported("no clipboard sharing".into()))
    }

    async fn selection_write(&self, _serial: u32) -> fdo::Result<OwnedFd> {
        Err(fdo::Error::NotSupported("no clipboard sharing".into()))
    }

    async fn selection_write_done(&self, _serial: u32, _success: bool) -> fdo::Result<()> {
        Err(fdo::Error::NotSupported("no clipboard sharing".into()))
    }

    async fn selection_read(&self, _mime_type: &str) -> fdo::Result<OwnedFd> {
        Err(fdo::Error::NotSupported("no clipboard sharing".into()))
    }

    #[zbus(signal)]
    async fn selection_owner_changed(emitter: &SignalEmitter<'_>, options: HashMap<String, OwnedValue>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn selection_transfer(emitter: &SignalEmitter<'_>, mime_type: &str, serial: u32) -> zbus::Result<()>;
}

/// `NotifyPointerAxis` flags → (finish, source).
pub fn axis_flags(flags: u32) -> (bool, u32) {
    let source = if flags & 2 != 0 {
        1
    } else if flags & 8 != 0 {
        2
    } else {
        0
    };
    (flags & 1 != 0, source)
}

#[cfg(test)]
mod tests {
    #[test]
    fn flags() {
        assert_eq!(super::axis_flags(0), (false, 0), "finger by default");
        assert_eq!(super::axis_flags(1 | 2), (true, 1));
        assert_eq!(super::axis_flags(8), (false, 2));
    }
}
