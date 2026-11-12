//! Helix session manager and D-Bus services.
//!
//! Implements the session lifecycle, inhibit, and related contracts
//! from helix-desktop-spec.
//!
//! - [`state`]: transport-independent session rules (unit-tested).
//! - [`manager`]: the `org.gnome.SessionManager` D-Bus interface.
//! - [`SessionService`]: connects to a bus, owns the name, and runs until the
//!   session ends.

pub mod autostart;
pub mod environment;
pub mod client;
pub mod end_session;
pub mod launcher;
pub mod locale;
pub mod logind;
pub mod manager;
pub mod objects;
pub mod restore;
pub mod state;

use std::collections::BTreeMap;
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
    lock_watcher: JoinHandle<()>,
    active_watcher: Option<JoinHandle<()>>,
    restore: restore::Shared,
}

/// How to run the service (see [`SessionService::start_with_options`]).
#[derive(Debug, Clone, Default)]
pub struct ServiceOptions {
    pub info: SessionInfo,
    pub timeouts: end_session::Timeouts,
    /// Where to reach logind; `None` disables power actions and leaves
    /// `SessionIsActive` true.
    pub logind: Option<logind::Logind>,
    /// Where session restore records live; `None` disables restore.
    pub restore: Option<std::path::PathBuf>,
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
        Self::start_with_timeouts(builder, info, end_session::Timeouts::default()).await
    }

    /// Like [`start_with`](Self::start_with), with end-of-session timeouts
    /// (tests use short ones).
    pub async fn start_with_timeouts(
        builder: connection::Builder<'_>,
        info: SessionInfo,
        timeouts: end_session::Timeouts,
    ) -> zbus::Result<Self> {
        Self::start_with_options(builder, ServiceOptions { info, timeouts, logind: None, restore: None }).await
    }

    /// Start with every option explicit.
    pub async fn start_with_options(builder: connection::Builder<'_>, options: ServiceOptions) -> zbus::Result<Self> {
        let ServiceOptions { info, timeouts, logind, restore } = options;
        let restore: restore::Shared = Arc::new(Mutex::new(restore.map(restore::Restore::open)));
        let state = Arc::new(Mutex::new(SessionState::new()));
        let (end_tx, end_rx) = mpsc::unbounded_channel();

        let conn = builder
            .serve_at(
                OBJECT_PATH,
                SessionManager::new(state.clone(), end_tx, info, timeouts, logind.clone(), restore.clone()),
            )?
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
        let lock_watcher = spawn_lock_watcher(&conn).await?;
        let active_watcher = match &logind {
            Some(logind) => spawn_active_watcher(&conn, logind).await,
            None => None,
        };
        Ok(Self {
            conn,
            state,
            end_rx,
            watcher,
            lock_watcher,
            active_watcher,
            restore,
        })
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Apps to start again at login (one entry per instance to restore).
    pub fn apps_to_restore(&self) -> Vec<String> {
        self.restore.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|r| r.to_relaunch()).unwrap_or_default()
    }

    /// Variables set through `Setenv` so far.
    pub fn environment(&self) -> BTreeMap<String, String> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .environment()
            .clone()
    }

    /// End the initialization phase, as a client calling `Initialized`
    /// would.
    pub async fn mark_initialized(&self) -> zbus::Result<()> {
        let iface = self
            .conn
            .object_server()
            .interface::<_, SessionManager>(OBJECT_PATH)
            .await?;
        let manager = iface.get().await;
        manager.mark_initialized(iface.signal_emitter()).await
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
        self.lock_watcher.abort();
        if let Some(task) = &self.active_watcher {
            task.abort();
        }
        // A round trip to the bus also ensures the end-request reply has
        // been flushed before we disconnect.
        self.conn.release_name(BUS_NAME).await?;
        self.conn.object_server().remove::<SessionManager, _>(OBJECT_PATH).await?;
        self.conn.graceful_shutdown().await;
        Ok(end)
    }
}

/// `SessionIsActive` follows logind's session `Active` property.
async fn spawn_active_watcher(conn: &Connection, logind: &logind::Logind) -> Option<JoinHandle<()>> {
    let iface = conn
        .object_server()
        .interface::<_, SessionManager>(OBJECT_PATH)
        .await
        .ok()?;
    let watch = logind
        .watch_active(move |active| {
            let iface = iface.clone();
            async move {
                let manager = iface.get().await;
                if let Err(err) = manager.set_active(active, iface.signal_emitter()).await {
                    tracing::warn!(%err, "cannot publish SessionIsActive");
                }
            }
        })
        .await;
    match watch {
        Ok(task) => Some(task),
        Err(err) => {
            tracing::warn!(%err, "cannot follow the logind session; SessionIsActive stays true");
            None
        }
    }
}

/// `SessionIsLocked` follows the screensaver's `ActiveChanged` signal
/// (helix-desktop-spec/contracts/dbus/org.gnome.ScreenSaver.md).
async fn spawn_lock_watcher(conn: &Connection) -> zbus::Result<JoinHandle<()>> {
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.gnome.ScreenSaver")?
        .member("ActiveChanged")?
        .build();
    let mut signals = zbus::MessageStream::for_match_rule(rule, conn, None).await?;
    let iface = conn
        .object_server()
        .interface::<_, SessionManager>(OBJECT_PATH)
        .await?;
    Ok(tokio::spawn(async move {
        while let Some(Ok(message)) = signals.next().await {
            let Ok(locked) = message.body().deserialize::<bool>() else { continue };
            let manager = iface.get().await;
            if let Err(err) = manager.set_locked(locked, iface.signal_emitter()).await {
                tracing::warn!(%err, "cannot publish SessionIsLocked");
            }
        }
    }))
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
