//! `org.gnome.Mutter.RemoteDesktop` (helix-desktop-spec
//! `org.gnome.Mutter.RemoteDesktop.md`): remote desktop sessions inject
//! input into the session (through the compositor), directly or over EIS,
//! and may own a screen cast session.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::{mpsc, oneshot};
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use std::os::fd::AsFd;

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

/// Why a clipboard or keymap request failed.
#[derive(Debug, Clone, PartialEq)]
pub enum RemoteError {
    Failed(String),
    InvalidArgs(String),
}

impl From<RemoteError> for fdo::Error {
    fn from(e: RemoteError) -> Self {
        match e {
            RemoteError::Failed(m) => fdo::Error::Failed(m),
            RemoteError::InvalidArgs(m) => fdo::Error::InvalidArgs(m),
        }
    }
}

/// Clipboard sharing requests of a session.
#[derive(Debug)]
pub enum ClipboardRequest {
    /// Turn sharing on, offering these types (if any).
    Enable(Vec<String>),
    Disable,
    SetSelection(Vec<String>),
    /// The fd for transfer `serial`.
    Write(u32),
    WriteDone(u32, bool),
    /// A fd to read the clipboard in this type from.
    Read(String),
}

/// A keymap set by a session (XKB text format v1).
#[derive(Debug, Clone, PartialEq)]
pub struct SessionKeymap {
    pub text: String,
    pub layout: u32,
    pub lock: bool,
}

#[derive(Debug)]
pub enum KeymapRequest {
    /// Use this keymap, or (None) go back to the configured input sources.
    Set(Option<SessionKeymap>),
    LayoutIndex(u32),
    /// `CurrentKeymap` for the session.
    Current,
}

#[derive(Debug)]
pub enum RemoteRequest {
    Clipboard(ClipboardRequest),
    Keymap(KeymapRequest),
}

#[derive(Debug)]
pub enum RemoteReply {
    Done,
    Fd(std::os::fd::OwnedFd),
    Keymap { name: String, source: u32 },
}

/// Largest keymap accepted.
const KEYMAP_MAX: u64 = 4 << 20;

/// Read an XKB keymap from `fd` (to its end; a final NUL is dropped).
fn read_keymap(fd: std::os::fd::OwnedFd) -> Result<String, String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::from(fd).take(KEYMAP_MAX).read_to_end(&mut bytes).map_err(|e| format!("cannot read the keymap: {e}"))?;
    while bytes.last() == Some(&0) {
        bytes.pop();
    }
    String::from_utf8(bytes).map_err(|_| "the keymap is not text".to_owned())
}

/// `mime-types` of clipboard options (missing: none).
fn mime_types(options: &HashMap<String, OwnedValue>) -> fdo::Result<Vec<String>> {
    match options.get("mime-types") {
        None => Ok(Vec::new()),
        Some(v) => Vec::<String>::try_from(v.try_clone().map_err(|e| fdo::Error::InvalidArgs(e.to_string()))?)
            .map_err(|_| fdo::Error::InvalidArgs("mime-types is an as".into())),
    }
}

/// `SelectionOwnerChanged` options.
pub fn owner_options(mime_types: Vec<String>, is_owner: bool) -> HashMap<String, OwnedValue> {
    HashMap::from([
        ("mime-types".to_owned(), OwnedValue::try_from(Value::from(mime_types)).expect("array")),
        ("session-is-owner".to_owned(), OwnedValue::from(is_owner)),
    ])
}

/// The object path of session `id`.
pub fn path_of(id: u32) -> String {
    session_path(id)
}

/// Sessions now open.
pub fn session_ids(shared: &Shared) -> Vec<u32> {
    lock(shared).sessions.keys().copied().collect()
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

    /// Ask the compositor (clipboard and keymap requests).
    async fn remote(&self, request: RemoteRequest) -> fdo::Result<RemoteReply> {
        let (reply, answer) = oneshot::channel();
        self.compositor
            .send(CastRequest::Remote { session: self.id, request, reply })
            .map_err(|_| fdo::Error::Failed("the compositor is gone".into()))?;
        answer.await.map_err(|_| fdo::Error::Failed("the compositor did not answer".into()))?.map_err(fdo::Error::from)
    }

    async fn clipboard(&self, header: &Header<'_>, request: ClipboardRequest) -> fdo::Result<RemoteReply> {
        self.check_owner(header)?;
        self.started()?;
        self.remote(RemoteRequest::Clipboard(request)).await
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
        HashMap::from([
            ("supported-keymap-types".to_owned(), OwnedValue::try_from(Value::from(vec![0u32])).expect("array")),
            ("supported-xkb-keymap-formats".to_owned(), OwnedValue::try_from(Value::from(vec![1u32])).expect("array")),
        ])
    }

    #[zbus(property)]
    async fn current_keymap(&self) -> HashMap<String, OwnedValue> {
        let (name, source) = match self.remote(RemoteRequest::Keymap(KeymapRequest::Current)).await {
            Ok(RemoteReply::Keymap { name, source }) => (name, source),
            _ => (String::new(), 0),
        };
        HashMap::from([
            ("name".to_owned(), OwnedValue::try_from(Value::from(name)).expect("string")),
            ("source".to_owned(), OwnedValue::from(source)),
        ])
    }

    async fn set_keymap(&self, options: HashMap<String, OwnedValue>, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        self.check_owner(&header)?;
        self.started()?;
        let keymap = match options.get("keymap-type").map(|v| &**v) {
            None => None,
            Some(Value::U32(0)) => {
                if let Some(format) = options.get("xkb-keymap-format") {
                    if !matches!(&**format, Value::U32(1)) {
                        return Err(fdo::Error::InvalidArgs("unsupported XKB keymap format".into()));
                    }
                }
                let fd = match options.get("xkb-keymap").map(|v| &**v) {
                    Some(Value::Fd(fd)) => fd.as_fd().try_clone_to_owned().map_err(|e| fdo::Error::InvalidArgs(e.to_string()))?,
                    _ => return Err(fdo::Error::InvalidArgs("xkb-keymap is required".into())),
                };
                let text = tokio::task::spawn_blocking(move || read_keymap(fd))
                    .await
                    .map_err(|e| fdo::Error::Failed(e.to_string()))?
                    .map_err(fdo::Error::InvalidArgs)?;
                let layout = match options.get("xkb-keymap-layout-index").map(|v| &**v) {
                    Some(Value::U32(i)) => *i,
                    None => 0,
                    Some(_) => return Err(fdo::Error::InvalidArgs("xkb-keymap-layout-index is a u".into())),
                };
                let lock = matches!(options.get("lock-keymap").map(|v| &**v), Some(Value::Bool(true)));
                Some(SessionKeymap { text, layout, lock })
            }
            Some(_) => return Err(fdo::Error::InvalidArgs("unsupported keymap type".into())),
        };
        self.remote(RemoteRequest::Keymap(KeymapRequest::Set(keymap))).await.map(|_| ())
    }

    async fn set_keymap_layout_index(&self, index: u32, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        self.check_owner(&header)?;
        self.started()?;
        self.remote(RemoteRequest::Keymap(KeymapRequest::LayoutIndex(index))).await.map(|_| ())
    }

    async fn enable_clipboard(&self, options: HashMap<String, OwnedValue>, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        let mime_types = mime_types(&options)?;
        self.clipboard(&header, ClipboardRequest::Enable(mime_types)).await.map(|_| ())
    }

    async fn disable_clipboard(&self, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        self.clipboard(&header, ClipboardRequest::Disable).await.map(|_| ())
    }

    async fn set_selection(&self, options: HashMap<String, OwnedValue>, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        let mime_types = mime_types(&options)?;
        if mime_types.is_empty() {
            return Err(fdo::Error::InvalidArgs("mime-types is required".into()));
        }
        self.clipboard(&header, ClipboardRequest::SetSelection(mime_types)).await.map(|_| ())
    }

    async fn selection_write(&self, serial: u32, #[zbus(header)] header: Header<'_>) -> fdo::Result<OwnedFd> {
        match self.clipboard(&header, ClipboardRequest::Write(serial)).await? {
            RemoteReply::Fd(fd) => Ok(OwnedFd::from(fd)),
            _ => Err(fdo::Error::Failed("no file descriptor".into())),
        }
    }

    async fn selection_write_done(&self, serial: u32, success: bool, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        self.clipboard(&header, ClipboardRequest::WriteDone(serial, success)).await.map(|_| ())
    }

    async fn selection_read(&self, mime_type: &str, #[zbus(header)] header: Header<'_>) -> fdo::Result<OwnedFd> {
        match self.clipboard(&header, ClipboardRequest::Read(mime_type.to_owned())).await? {
            RemoteReply::Fd(fd) => Ok(OwnedFd::from(fd)),
            _ => Err(fdo::Error::Failed("no file descriptor".into())),
        }
    }

    #[zbus(signal)]
    pub async fn selection_owner_changed(emitter: &SignalEmitter<'_>, options: HashMap<String, OwnedValue>) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn selection_transfer(emitter: &SignalEmitter<'_>, mime_type: &str, serial: u32) -> zbus::Result<()>;
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
    use super::*;

    #[test]
    fn keymap_text() {
        use std::io::Write;
        let (r, w) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut w = w;
        w.write_all(b"xkb_keymap {};\0").unwrap();
        drop(w);
        assert_eq!(read_keymap(r.into()).unwrap(), "xkb_keymap {};");
    }

    #[test]
    fn clipboard_options() {
        let o = owner_options(vec!["text/plain".into()], true);
        assert_eq!(Vec::<String>::try_from(o["mime-types"].try_clone().unwrap()).unwrap(), ["text/plain"]);
        assert!(bool::try_from(&o["session-is-owner"]).unwrap());
        let mut opts = HashMap::new();
        assert!(mime_types(&opts).unwrap().is_empty());
        opts.insert("mime-types".to_owned(), OwnedValue::try_from(Value::from(vec!["a/b".to_owned()])).unwrap());
        assert_eq!(mime_types(&opts).unwrap(), ["a/b"]);
        opts.insert("mime-types".to_owned(), OwnedValue::from(3u32));
        assert!(mime_types(&opts).is_err());
    }

    #[test]
    fn flags() {
        assert_eq!(super::axis_flags(0), (false, 0), "finger by default");
        assert_eq!(super::axis_flags(1 | 2), (true, 1));
        assert_eq!(super::axis_flags(8), (false, 2));
    }
}
