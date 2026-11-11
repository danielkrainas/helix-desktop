//! `org.freedesktop.impl.portal.Inhibit` backend.
//!
//! Contract: `helix-desktop-spec/contracts/dbus/org.freedesktop.impl.portal.Inhibit.xml`
//! (+ `.Request.xml`, `.Session.xml`, `org.freedesktop.impl.portal.md`).
//!
//! Each portal inhibition becomes an `org.gnome.SessionManager` inhibitor
//! (the flag bits are the same), held until the frontend closes the Request.
//! Inhibitors are owned by this process's bus connection, so they also go
//! away if the backend exits.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use helix_session::client::SessionManagerProxy;
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, Value};
use zbus::{fdo, interface, Connection, ObjectServer};

use crate::OBJECT_PATH;

/// `session-state` values for `StateChanged`.
pub mod session_state {
    pub const RUNNING: u32 = 1;
    pub const QUERY_END: u32 = 2;
    pub const ENDING: u32 = 3;
}

#[derive(Debug)]
struct RequestEntry {
    cookie: u32,
    owner: Option<String>,
}

/// Live requests and monitor sessions, by object path.
#[derive(Debug, Default)]
pub struct Registry {
    requests: HashMap<OwnedObjectPath, RequestEntry>,
    monitors: HashMap<OwnedObjectPath, Option<String>>,
    session_state: Option<u32>,
}

pub type SharedRegistry = Arc<Mutex<Registry>>;

fn lock(r: &SharedRegistry) -> MutexGuard<'_, Registry> {
    r.lock().unwrap_or_else(|e| e.into_inner())
}

impl Registry {
    pub fn request_count(&self) -> usize {
        self.requests.len()
    }

    pub fn monitor_count(&self) -> usize {
        self.monitors.len()
    }

    fn state(&self) -> u32 {
        self.session_state.unwrap_or(session_state::RUNNING)
    }
}

fn state_dict(state: u32) -> HashMap<&'static str, Value<'static>> {
    HashMap::from([
        // No screensaver integration yet.
        ("screensaver-active", Value::from(false)),
        ("session-state", Value::from(state)),
    ])
}

fn sender(header: &Header<'_>) -> Option<String> {
    header.sender().map(|s| s.to_string())
}

/// The exported Inhibit backend.
pub struct InhibitBackend {
    sm: SessionManagerProxy<'static>,
    registry: SharedRegistry,
}

impl InhibitBackend {
    pub fn new(sm: SessionManagerProxy<'static>, registry: SharedRegistry) -> Self {
        Self { sm, registry }
    }
}

#[interface(name = "org.freedesktop.impl.portal.Inhibit")]
impl InhibitBackend {
    #[allow(clippy::too_many_arguments)]
    async fn inhibit(
        &self,
        handle: ObjectPath<'_>,
        app_id: &str,
        _window: &str,
        flags: u32,
        options: HashMap<String, Value<'_>>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> fdo::Result<()> {
        let reason = match options.get("reason") {
            Some(Value::Str(s)) => s.as_str(),
            _ => "",
        };
        let cookie = self
            .sm
            .inhibit(app_id, 0, reason, flags)
            .await
            .map_err(|e| fdo::Error::Failed(format!("session manager refused inhibit: {e}")))?;

        let path = OwnedObjectPath::from(handle.into_owned());
        let request = InhibitRequest {
            sm: self.sm.clone(),
            registry: self.registry.clone(),
        };
        if !server.at(&path, request).await? {
            // Path already in use: undo and report.
            let _ = self.sm.uninhibit(cookie).await;
            return Err(fdo::Error::InvalidArgs(format!("{path} is already exported")));
        }
        lock(&self.registry).requests.insert(
            path.clone(),
            RequestEntry {
                cookie,
                owner: sender(&header),
            },
        );
        tracing::info!(app_id, flags, cookie, %path, "portal inhibit");
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_monitor(
        &self,
        _handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        app_id: &str,
        _window: &str,
        #[zbus(header)] header: Header<'_>,
        #[zbus(object_server)] server: &ObjectServer,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<u32> {
        let path = OwnedObjectPath::from(session_handle.into_owned());
        let session = MonitorSession {
            registry: self.registry.clone(),
        };
        if !server.at(&path, session).await? {
            return Err(fdo::Error::InvalidArgs(format!("{path} is already exported")));
        }
        let state = {
            let mut reg = lock(&self.registry);
            reg.monitors.insert(path.clone(), sender(&header));
            reg.state()
        };
        tracing::info!(app_id, %path, "portal inhibit monitor created");

        // Send the initial state once the frontend has had a chance to set
        // up its side of the session (it ignores signals for sessions it
        // does not know yet).
        let conn = conn.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if let Ok(emitter) = SignalEmitter::new(&conn, OBJECT_PATH) {
                let _ = Self::state_changed(&emitter, path.as_ref(), state_dict(state)).await;
            }
        });
        Ok(0)
    }

    async fn query_end_response(&self, session_handle: ObjectPath<'_>) {
        // There is no query-end phase yet (see gaps.md), so nothing waits on
        // this acknowledgement.
        tracing::debug!(%session_handle, "query end response");
    }

    #[zbus(signal)]
    pub async fn state_changed(
        emitter: &SignalEmitter<'_>,
        session_handle: ObjectPath<'_>,
        state: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<()>;
}

/// `org.freedesktop.impl.portal.Request` for one inhibition.
pub struct InhibitRequest {
    sm: SessionManagerProxy<'static>,
    registry: SharedRegistry,
}

#[interface(name = "org.freedesktop.impl.portal.Request")]
impl InhibitRequest {
    async fn close(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> fdo::Result<()> {
        let Some(path) = header.path() else {
            return Ok(());
        };
        let path = OwnedObjectPath::from(path.to_owned());
        close_request(&self.sm, &self.registry, server, &path).await;
        Ok(())
    }
}

/// `org.freedesktop.impl.portal.Session` for one state monitor.
pub struct MonitorSession {
    registry: SharedRegistry,
}

#[interface(name = "org.freedesktop.impl.portal.Session")]
impl MonitorSession {
    async fn close(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> fdo::Result<()> {
        if let Some(path) = header.path() {
            let path = OwnedObjectPath::from(path.to_owned());
            lock(&self.registry).monitors.remove(&path);
            let _ = server.remove::<MonitorSession, _>(&path).await;
            tracing::info!(%path, "portal inhibit monitor closed");
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

async fn close_request(
    sm: &SessionManagerProxy<'_>,
    registry: &SharedRegistry,
    server: &ObjectServer,
    path: &OwnedObjectPath,
) {
    let entry = lock(registry).requests.remove(path);
    if let Some(entry) = entry {
        if let Err(err) = sm.uninhibit(entry.cookie).await {
            tracing::warn!(%err, cookie = entry.cookie, "uninhibit failed");
        }
        tracing::info!(cookie = entry.cookie, %path, "portal inhibit released");
    }
    let _ = server.remove::<InhibitRequest, _>(path).await;
}

/// Release every request and monitor created by a peer that left the bus
/// (normally the portal frontend).
pub async fn release_owner(
    conn: &Connection,
    sm: &SessionManagerProxy<'_>,
    registry: &SharedRegistry,
    owner: &str,
) {
    let (requests, monitors): (Vec<_>, Vec<_>) = {
        let reg = lock(registry);
        (
            reg.requests
                .iter()
                .filter(|(_, e)| e.owner.as_deref() == Some(owner))
                .map(|(p, _)| p.clone())
                .collect(),
            reg.monitors
                .iter()
                .filter(|(_, o)| o.as_deref() == Some(owner))
                .map(|(p, _)| p.clone())
                .collect(),
        )
    };
    let server = conn.object_server();
    for path in &requests {
        close_request(sm, registry, server, path).await;
    }
    for path in &monitors {
        lock(registry).monitors.remove(path);
        let _ = server.remove::<MonitorSession, _>(path).await;
    }
}

/// Unexport every request and monitor (inhibitors are released along with
/// the connection).
pub async fn release_all(conn: &Connection, registry: &SharedRegistry) {
    let (requests, monitors): (Vec<_>, Vec<_>) = {
        let mut reg = lock(registry);
        (
            reg.requests.drain().map(|(p, _)| p).collect(),
            reg.monitors.drain().map(|(p, _)| p).collect(),
        )
    };
    let server = conn.object_server();
    for path in &requests {
        let _ = server.remove::<InhibitRequest, _>(path).await;
    }
    for path in &monitors {
        let _ = server.remove::<MonitorSession, _>(path).await;
    }
}

/// Record a new session state and tell every monitor.
pub async fn broadcast_state(conn: &Connection, registry: &SharedRegistry, state: u32) {
    let monitors: Vec<_> = {
        let mut reg = lock(registry);
        reg.session_state = Some(state);
        reg.monitors.keys().cloned().collect()
    };
    let Ok(emitter) = SignalEmitter::new(conn, OBJECT_PATH) else {
        return;
    };
    for path in monitors {
        if let Err(err) =
            InhibitBackend::state_changed(&emitter, path.as_ref(), state_dict(state)).await
        {
            tracing::warn!(%err, %path, "failed to send StateChanged");
        }
    }
}
