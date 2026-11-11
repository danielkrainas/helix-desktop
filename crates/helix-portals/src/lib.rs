//! Helix XDG desktop portal backends.
//!
//! Serves `org.freedesktop.impl.portal.Settings` and
//! `org.freedesktop.impl.portal.Inhibit` as
//! `org.freedesktop.impl.portal.desktop.helix`, for the xdg-desktop-portal
//! frontend to use (see `session/helix.portal`).

pub mod inhibit;
pub mod settings;

use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use helix_session::client::SessionManagerProxy;
use tokio::task::JoinHandle;
use zbus::fdo::{DBusProxy, RequestNameFlags, RequestNameReply};
use zbus::proxy::CacheProperties;
use zbus::{connection, Connection};

use crate::inhibit::{session_state, InhibitBackend, SharedRegistry};
use crate::settings::{SettingsBackend, SettingsSource};

/// Bus name advertised in `helix.portal`.
pub const BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.helix";
/// Where portal backends serve their interfaces.
pub const OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";

/// The running portal backend service.
pub struct PortalService {
    conn: Connection,
    registry: SharedRegistry,
    tasks: Vec<JoinHandle<()>>,
}

impl PortalService {
    /// Export the backends on `builder`'s bus and take [`BUS_NAME`].
    pub async fn start(
        builder: connection::Builder<'_>,
        settings: Arc<dyn SettingsSource>,
    ) -> zbus::Result<Self> {
        let conn = builder
            .serve_at(OBJECT_PATH, SettingsBackend::new(settings))?
            .build()
            .await?;

        let sm = SessionManagerProxy::builder(&conn)
            .cache_properties(CacheProperties::No)
            .build()
            .await?;
        let registry: SharedRegistry = Arc::new(Mutex::new(Default::default()));
        conn.object_server()
            .at(OBJECT_PATH, InhibitBackend::new(sm.clone(), registry.clone()))
            .await?;

        match conn
            .request_name_with_flags(BUS_NAME, RequestNameFlags::DoNotQueue.into())
            .await?
        {
            RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner => {}
            _ => return Err(zbus::Error::NameTaken),
        }
        tracing::info!(name = BUS_NAME, "portal backends ready");

        let tasks = vec![
            spawn_owner_watcher(&conn, sm.clone(), registry.clone()).await?,
            spawn_session_watcher(&conn, sm, registry.clone()).await?,
        ];
        Ok(Self {
            conn,
            registry,
            tasks,
        })
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    pub fn registry(&self) -> &SharedRegistry {
        &self.registry
    }

    /// Call when the settings source changed; emits `SettingChanged` for
    /// each changed key.
    pub async fn settings_changed(&self) -> zbus::Result<()> {
        let iface = self
            .conn
            .object_server()
            .interface::<_, SettingsBackend>(OBJECT_PATH)
            .await?;
        let backend = iface.get().await;
        backend.refresh(iface.signal_emitter()).await
    }

    /// Release everything, the name, and close the connection.
    pub async fn shutdown(self) -> zbus::Result<()> {
        for task in &self.tasks {
            task.abort();
        }
        for task in self.tasks {
            let _ = task.await;
        }
        self.conn.release_name(BUS_NAME).await?;
        // Exported objects hold SessionManager proxies, which hold the
        // connection: unregister them or the connection never drops.
        inhibit::release_all(&self.conn, &self.registry).await;
        let server = self.conn.object_server();
        server.remove::<InhibitBackend, _>(OBJECT_PATH).await?;
        server.remove::<SettingsBackend, _>(OBJECT_PATH).await?;
        self.conn.graceful_shutdown().await;
        Ok(())
    }
}

async fn spawn_owner_watcher(
    conn: &Connection,
    sm: SessionManagerProxy<'static>,
    registry: SharedRegistry,
) -> zbus::Result<JoinHandle<()>> {
    let mut changes = DBusProxy::new(conn).await?.receive_name_owner_changed().await?;
    let conn = conn.clone();
    Ok(tokio::spawn(async move {
        while let Some(signal) = changes.next().await {
            let Ok(args) = signal.args() else { continue };
            let name = args.name().as_str();
            if name.starts_with(':') && args.new_owner().is_none() {
                inhibit::release_owner(&conn, &sm, &registry, name).await;
            }
        }
    }))
}

/// Forward SessionManager lifecycle signals to inhibit monitors.
async fn spawn_session_watcher(
    conn: &Connection,
    sm: SessionManagerProxy<'static>,
    registry: SharedRegistry,
) -> zbus::Result<JoinHandle<()>> {
    let mut running = sm.receive_session_running().await?;
    let mut over = sm.receive_session_over().await?;
    let conn = conn.clone();
    Ok(tokio::spawn(async move {
        loop {
            let state = tokio::select! {
                Some(_) = running.next() => session_state::RUNNING,
                Some(_) = over.next() => session_state::ENDING,
                else => break,
            };
            inhibit::broadcast_state(&conn, &registry, state).await;
        }
    }))
}
