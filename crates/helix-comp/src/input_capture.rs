//! Input capture (helix-desktop-spec `contracts/dbus/org.helix.InputCapture.xml`):
//! the compositor's side of the InputCapture portal. A session's pointer
//! barriers, on monitor edges, start a capture when the pointer crosses
//! one; while captured, input goes to the session's EIS client instead of
//! the desktop.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot};
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedFd, OwnedObjectPath};
use zbus::{fdo, interface, Connection};

use crate::remote_desktop::{mime_types, ClipboardRequest, RemoteReply, RemoteRequest};
use crate::screencast::CastRequest;

pub const BUS_NAME: &str = "org.helix.InputCapture";
pub const OBJECT_PATH: &str = "/org/helix/InputCapture";
/// Keyboard and pointer.
pub const CAPABILITIES: u32 = 1 | 2;

/// A zone (a monitor's logical area): width, height, x, y.
pub type Zone = (u32, u32, i32, i32);
/// A barrier: its id and x1, y1, x2, y2.
pub type Barrier = (u32, (i32, i32, i32, i32));

/// What the D-Bus side asks the compositor.
#[derive(Debug)]
pub enum CaptureRequest {
    Create { session: u32, capabilities: u32 },
    Zones { reply: oneshot::Sender<(u32, Vec<Zone>)> },
    /// Set `session`'s barriers for `zone_set`; the reply lists the ids
    /// refused.
    Barriers { session: u32, zone_set: u32, barriers: Vec<Barrier>, reply: oneshot::Sender<Vec<u32>> },
    Enable { session: u32 },
    Disable { session: u32 },
    Release { session: u32, activation: u32, position: Option<(f64, f64)> },
    ConnectEis { session: u32, reply: oneshot::Sender<Result<std::os::fd::OwnedFd, String>> },
    Close { session: u32 },
}

/// What the compositor tells a session.
#[derive(Debug, Clone, PartialEq)]
pub enum CaptureSignal {
    Activated { session: u32, activation: u32, barrier: u32, position: (f64, f64) },
    Deactivated { session: u32, activation: u32, position: (f64, f64) },
    Disabled { session: u32 },
    ZonesChanged { zone_set: u32 },
}

/// Whether barrier `(x1, y1, x2, y2)` lies on an edge of one of `zones`
/// (horizontal or vertical, within the edge).
pub fn on_zone_edge(zones: &[Zone], (x1, y1, x2, y2): (i32, i32, i32, i32)) -> bool {
    let (lo_x, hi_x, lo_y, hi_y) = (x1.min(x2), x1.max(x2), y1.min(y2), y1.max(y2));
    zones.iter().any(|&(w, h, zx, zy)| {
        let (right, bottom) = (zx + w as i32, zy + h as i32);
        if x1 == x2 {
            (x1 == zx || x1 == right) && lo_y >= zy && hi_y <= bottom
        } else if y1 == y2 {
            (y1 == zy || y1 == bottom) && lo_x >= zx && hi_x <= right
        } else {
            false
        }
    })
}

/// The barrier (of `barriers`) the move from `from` to `to` crosses, if
/// any: a vertical barrier at x = X is crossed when X lies between the
/// two (`from` on one side, `to` on or past it), with the crossing's y
/// within the barrier; likewise for horizontal ones.
pub fn crossed(barriers: &[Barrier], from: (f64, f64), to: (f64, f64)) -> Option<u32> {
    barriers.iter().find_map(|&(id, (x1, y1, x2, y2))| {
        let side = |v: f64, at: f64| v >= at;
        let hit = if x1 == x2 {
            let x = f64::from(x1);
            if side(from.0, x) == side(to.0, x) || (to.0 - from.0).abs() < f64::EPSILON {
                return None;
            }
            let t = (x - from.0) / (to.0 - from.0);
            let y = from.1 + t * (to.1 - from.1);
            y >= f64::from(y1.min(y2)) && y <= f64::from(y1.max(y2)) + 1.0
        } else {
            let y = f64::from(y1);
            if side(from.1, y) == side(to.1, y) || (to.1 - from.1).abs() < f64::EPSILON {
                return None;
            }
            let t = (y - from.1) / (to.1 - from.1);
            let x = from.0 + t * (to.0 - from.0);
            x >= f64::from(x1.min(x2)) && x <= f64::from(x1.max(x2)) + 1.0
        };
        hit.then_some(id)
    })
}

#[derive(Default)]
pub struct Sessions {
    next: u32,
    /// Session id → its owner's unique bus name.
    owners: HashMap<u32, String>,
}

pub type Shared = Arc<Mutex<Sessions>>;

fn lock(shared: &Shared) -> std::sync::MutexGuard<'_, Sessions> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

/// Capture sessions share the clipboard like remote desktop sessions;
/// their clipboard ids have this bit set to keep the two apart.
pub const CLIPBOARD_BIT: u32 = 1 << 31;

/// The clipboard id of capture session `id`.
pub fn clipboard_id(id: u32) -> u32 {
    id | CLIPBOARD_BIT
}

/// The capture session of a clipboard id, if it is one.
pub fn of_clipboard_id(id: u32) -> Option<u32> {
    (id & CLIPBOARD_BIT != 0).then_some(id & !CLIPBOARD_BIT)
}

pub fn session_path(id: u32) -> String {
    format!("{OBJECT_PATH}/Session/u{id}")
}

pub struct InputCapture {
    pub shared: Shared,
    pub compositor: Option<mpsc::UnboundedSender<CastRequest>>,
}

#[interface(name = "org.helix.InputCapture")]
impl InputCapture {
    async fn create_session(
        &self,
        capabilities: u32,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> fdo::Result<OwnedObjectPath> {
        let owner = crate::screencast::check_caller(conn, &header).await?;
        let compositor = self.compositor.clone().ok_or_else(|| fdo::Error::NotSupported("no input capture".into()))?;
        let capabilities = capabilities & CAPABILITIES;
        if capabilities == 0 {
            return Err(fdo::Error::InvalidArgs("no capability Helix can capture".into()));
        }
        let id = {
            let mut sessions = lock(&self.shared);
            sessions.next += 1;
            let id = sessions.next;
            sessions.owners.insert(id, owner.clone());
            id
        };
        compositor.send(CastRequest::Capture(CaptureRequest::Create { session: id, capabilities })).map_err(|_| fdo::Error::Failed("the compositor is gone".into()))?;
        let path = session_path(id);
        server.at(path.as_str(), Session { id, owner, compositor, shared: self.shared.clone() }).await?;
        tracing::info!(path, capabilities, "input capture session created");
        OwnedObjectPath::try_from(path).map_err(|e| fdo::Error::Failed(e.to_string()))
    }

    #[zbus(property)]
    async fn supported_capabilities(&self) -> u32 {
        CAPABILITIES
    }
}

pub struct Session {
    id: u32,
    owner: String,
    compositor: mpsc::UnboundedSender<CastRequest>,
    shared: Shared,
}

impl Session {
    fn check_owner(&self, header: &Header<'_>) -> fdo::Result<()> {
        if header.sender().is_some_and(|s| s.as_str() == self.owner) {
            Ok(())
        } else {
            Err(fdo::Error::AccessDenied("not your session".into()))
        }
    }

    fn send(&self, request: CaptureRequest) -> fdo::Result<()> {
        self.compositor.send(CastRequest::Capture(request)).map_err(|_| fdo::Error::Failed("the compositor is gone".into()))
    }

    /// A clipboard request, as a remote desktop session's.
    async fn clipboard(&self, header: &Header<'_>, request: ClipboardRequest) -> fdo::Result<RemoteReply> {
        self.check_owner(header)?;
        let (reply, answer) = oneshot::channel();
        self.compositor
            .send(CastRequest::Remote { session: clipboard_id(self.id), request: RemoteRequest::Clipboard(request), reply })
            .map_err(|_| fdo::Error::Failed("the compositor is gone".into()))?;
        answer.await.map_err(|_| fdo::Error::Failed("the compositor did not answer".into()))?.map_err(fdo::Error::from)
    }
}

/// Close input capture session `id` (its owner closed it or left).
pub async fn close_session(conn: &Connection, shared: &Shared, compositor: &mpsc::UnboundedSender<CastRequest>, id: u32) {
    if lock(shared).owners.remove(&id).is_none() {
        return;
    }
    let _ = compositor.send(CastRequest::Capture(CaptureRequest::Close { session: id }));
    let path = session_path(id);
    if let Ok(emitter) = SignalEmitter::new(conn, path.as_str()) {
        let _ = Session::closed(&emitter).await;
    }
    let _ = conn.object_server().remove::<Session, _>(path.as_str()).await;
    tracing::info!(path, "input capture session closed");
}

/// Sessions of a bus peer that left.
pub fn sessions_of(shared: &Shared, owner: &str) -> Vec<u32> {
    lock(shared).owners.iter().filter(|(_, o)| o.as_str() == owner).map(|(id, _)| *id).collect()
}

#[interface(name = "org.helix.InputCapture.Session")]
impl Session {
    async fn get_zones(&self, #[zbus(header)] header: Header<'_>) -> fdo::Result<(u32, Vec<Zone>)> {
        self.check_owner(&header)?;
        let (reply, answer) = oneshot::channel();
        self.send(CaptureRequest::Zones { reply })?;
        answer.await.map_err(|_| fdo::Error::Failed("the compositor did not answer".into()))
    }

    async fn set_pointer_barriers(&self, barriers: Vec<Barrier>, zone_set: u32, #[zbus(header)] header: Header<'_>) -> fdo::Result<Vec<u32>> {
        self.check_owner(&header)?;
        let (reply, answer) = oneshot::channel();
        self.send(CaptureRequest::Barriers { session: self.id, zone_set, barriers, reply })?;
        answer.await.map_err(|_| fdo::Error::Failed("the compositor did not answer".into()))
    }

    async fn enable(&self, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        self.check_owner(&header)?;
        self.send(CaptureRequest::Enable { session: self.id })
    }

    async fn disable(&self, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        self.check_owner(&header)?;
        self.send(CaptureRequest::Disable { session: self.id })
    }

    async fn release(&self, activation_id: u32, has_position: bool, position: (f64, f64), #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        self.check_owner(&header)?;
        self.send(CaptureRequest::Release { session: self.id, activation: activation_id, position: has_position.then_some(position) })
    }

    #[zbus(name = "ConnectToEIS")]
    async fn connect_to_eis(&self, #[zbus(header)] header: Header<'_>) -> fdo::Result<OwnedFd> {
        self.check_owner(&header)?;
        let (reply, answer) = oneshot::channel();
        self.send(CaptureRequest::ConnectEis { session: self.id, reply })?;
        let fd = answer.await.map_err(|_| fdo::Error::Failed("the compositor did not answer".into()))?.map_err(fdo::Error::Failed)?;
        Ok(OwnedFd::from(fd))
    }

    async fn close(&self, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> fdo::Result<()> {
        self.check_owner(&header)?;
        close_session(conn, &self.shared, &self.compositor, self.id).await;
        Ok(())
    }

    async fn enable_clipboard(&self, options: HashMap<String, zbus::zvariant::OwnedValue>, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        let types = mime_types(&options)?;
        self.clipboard(&header, ClipboardRequest::Enable(types)).await.map(|_| ())
    }

    async fn disable_clipboard(&self, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        self.clipboard(&header, ClipboardRequest::Disable).await.map(|_| ())
    }

    async fn set_selection(&self, options: HashMap<String, zbus::zvariant::OwnedValue>, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        let types = mime_types(&options)?;
        if types.is_empty() {
            return Err(fdo::Error::InvalidArgs("mime-types is required".into()));
        }
        self.clipboard(&header, ClipboardRequest::SetSelection(types)).await.map(|_| ())
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
    pub async fn selection_owner_changed(emitter: &SignalEmitter<'_>, options: HashMap<String, zbus::zvariant::OwnedValue>) -> zbus::Result<()>;
    #[zbus(signal)]
    pub async fn selection_transfer(emitter: &SignalEmitter<'_>, mime_type: &str, serial: u32) -> zbus::Result<()>;
    #[zbus(signal)]
    pub async fn activated(emitter: &SignalEmitter<'_>, activation_id: u32, barrier_id: u32, cursor_position: (f64, f64)) -> zbus::Result<()>;
    #[zbus(signal)]
    pub async fn deactivated(emitter: &SignalEmitter<'_>, activation_id: u32, cursor_position: (f64, f64)) -> zbus::Result<()>;
    #[zbus(signal)]
    pub async fn disabled(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
    #[zbus(signal)]
    pub async fn zones_changed(emitter: &SignalEmitter<'_>, zone_set: u32) -> zbus::Result<()>;
    #[zbus(signal)]
    pub async fn closed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

/// Emit `signal` on its session (every session for zone changes).
pub async fn emit(conn: &Connection, shared: &Shared, signal: CaptureSignal) -> zbus::Result<()> {
    let ids: Vec<u32> = match &signal {
        CaptureSignal::Activated { session, .. } | CaptureSignal::Deactivated { session, .. } | CaptureSignal::Disabled { session } => vec![*session],
        CaptureSignal::ZonesChanged { .. } => lock(shared).owners.keys().copied().collect(),
    };
    for id in ids {
        let path = session_path(id);
        let emitter = SignalEmitter::new(conn, path.as_str())?;
        match &signal {
            CaptureSignal::Activated { activation, barrier, position, .. } => Session::activated(&emitter, *activation, *barrier, *position).await?,
            CaptureSignal::Deactivated { activation, position, .. } => Session::deactivated(&emitter, *activation, *position).await?,
            CaptureSignal::Disabled { .. } => Session::disabled(&emitter).await?,
            CaptureSignal::ZonesChanged { zone_set } => Session::zones_changed(&emitter, *zone_set).await?,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ZONES: [Zone; 2] = [(1000, 800, 0, 0), (1000, 800, 1000, 0)];

    #[test]
    fn clipboard_ids() {
        assert_eq!(of_clipboard_id(clipboard_id(7)), Some(7));
        assert_eq!(of_clipboard_id(7), None, "a remote desktop session's");
    }

    #[test]
    fn barriers_on_edges() {
        assert!(on_zone_edge(&ZONES, (0, 0, 0, 799)), "left edge");
        assert!(on_zone_edge(&ZONES, (2000, 100, 2000, 300)), "right edge of the second");
        assert!(on_zone_edge(&ZONES, (0, 800, 999, 800)), "bottom edge");
        assert!(!on_zone_edge(&ZONES, (500, 0, 500, 799)), "inside");
        assert!(!on_zone_edge(&ZONES, (0, 0, 10, 10)), "diagonal");
        assert!(!on_zone_edge(&ZONES, (0, 700, 0, 900)), "past the edge");
    }

    #[test]
    fn crossings() {
        let barriers = [(7, (2000, 0, 2000, 799)), (8, (0, 0, 0, 399))];
        assert_eq!(crossed(&barriers, (1995.0, 100.0), (2003.0, 104.0)), Some(7));
        assert_eq!(crossed(&barriers, (1990.0, 100.0), (1999.0, 104.0)), None, "not there yet");
        assert_eq!(crossed(&barriers, (3.0, 100.0), (-2.0, 100.0)), Some(8));
        assert_eq!(crossed(&barriers, (3.0, 500.0), (-2.0, 500.0)), None, "below the barrier");
    }
}
