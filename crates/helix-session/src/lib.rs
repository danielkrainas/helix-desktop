//! Helix session manager and D-Bus services.
//!
//! Implements the session lifecycle, inhibit, and related contracts
//! from helix-desktop-spec.
//!
//! - [`state`]: transport-independent session rules (unit-tested).
//! - [`manager`]: the `org.gnome.SessionManager` D-Bus interface.
//! - [`SessionService`]: connects to a bus, owns the name, and runs until the
//!   session ends.

pub mod client;
pub mod locale;
pub mod manager;
pub mod state;

use std::future::Future;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use zbus::fdo::{DBusProxy, RequestNameFlags, RequestNameReply};
use zbus::object_server::SignalEmitter;
use zbus::{connection, Connection};

use crate::manager::{SessionInfo, SessionManager, BUS_NAME, OBJECT_PATH};
use crate::state::{EndSessionAction, Phase, SessionState};

/// Why [`SessionService::run`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEnd {
    /// A client requested Logout / Shutdown / Reboot and it was accepted.
    Requested(EndSessionAction),
    /// The caller-supplied stop future resolved (e.g. SIGTERM).
    Stopped,
}

/// A running `org.gnome.SessionManager` service.
pub struct SessionService {
    conn: Connection,
    state: Arc<Mutex<SessionState>>,
    end_rx: mpsc::UnboundedReceiver<EndSessionAction>,
    watcher: JoinHandle<()>,
}

impl SessionService {
    /// Export the manager on `builder`'s bus and take ownership of
    /// `org.gnome.SessionManager`.
    ///
    /// Fails if another process already owns the name.
    pub async fn start(builder: connection::Builder<'_>) -> zbus::Result<Self> {
        Self::start_with(builder, SessionInfo::default()).await
    }

    /// Like [`start`](Self::start), with explicit session properties.
    pub async fn start_with(
        builder: connection::Builder<'_>,
        info: SessionInfo,
    ) -> zbus::Result<Self> {
        let state = Arc::new(Mutex::new(SessionState::new()));
        let (end_tx, end_rx) = mpsc::unbounded_channel();

        let conn = builder
            .serve_at(OBJECT_PATH, SessionManager::new(state.clone(), end_tx, info))?
            .build()
            .await?;

        match conn
            .request_name_with_flags(BUS_NAME, RequestNameFlags::DoNotQueue.into())
            .await?
        {
            RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner => {}
            RequestNameReply::Exists | RequestNameReply::InQueue => {
                return Err(zbus::Error::NameTaken);
            }
        }
        tracing::info!(name = BUS_NAME, unique = ?conn.unique_name(), "owning bus name");

        let watcher = spawn_owner_watcher(&conn).await?;
        Ok(Self {
            conn,
            state,
            end_rx,
            watcher,
        })
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Serve until a client ends the session or `stop` resolves, then release
    /// the bus name and close the connection.
    pub async fn run(mut self, stop: impl Future<Output = ()>) -> zbus::Result<SessionEnd> {
        let end = tokio::select! {
            Some(action) = self.end_rx.recv() => SessionEnd::Requested(action),
            () = stop => SessionEnd::Stopped,
        };

        if end == SessionEnd::Stopped {
            // Externally terminated: still tell listeners the session is over.
            let previous = self
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .force_end();
            if previous != Phase::Ending {
                let emitter = SignalEmitter::new(&self.conn, OBJECT_PATH)?;
                SessionManager::session_over(&emitter).await?;
            }
        }

        self.watcher.abort();
        // A round trip to the bus also ensures the end-request reply has
        // been flushed before we disconnect.
        self.conn.release_name(BUS_NAME).await?;
        self.conn.object_server().remove::<SessionManager, _>(OBJECT_PATH).await?;
        self.conn.graceful_shutdown().await;
        Ok(end)
    }
}

/// Watch for peers leaving the bus so their clients and inhibitors don't leak
/// (a crashed app must not block logout forever).
async fn spawn_owner_watcher(conn: &Connection) -> zbus::Result<JoinHandle<()>> {
    let dbus = DBusProxy::new(conn).await?;
    let mut changes = dbus.receive_name_owner_changed().await?;
    let iface = conn
        .object_server()
        .interface::<_, SessionManager>(OBJECT_PATH)
        .await?;

    Ok(tokio::spawn(async move {
        while let Some(signal) = changes.next().await {
            let Ok(args) = signal.args() else { continue };
            let name = args.name().as_str();
            if !name.starts_with(':') || args.new_owner().is_some() {
                continue;
            }
            let manager = iface.get().await;
            if let Err(err) = manager.release_owner(name, iface.signal_emitter()).await {
                tracing::warn!(%err, name, "failed to announce released resources");
            }
        }
    }))
}
