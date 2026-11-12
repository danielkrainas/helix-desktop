//! Helix XDG desktop portal backends.
//!
//! Serves `org.freedesktop.impl.portal.Settings`,
//! `org.freedesktop.impl.portal.Inhibit`,
//! `org.freedesktop.impl.portal.Screenshot` and
//! `org.freedesktop.impl.portal.ScreenCast` as
//! `org.freedesktop.impl.portal.desktop.helix`, for the xdg-desktop-portal
//! frontend to use (see `session/helix.portal`).

pub mod clipboard;
pub mod dialogs;
pub mod inhibit;
pub mod lockdown;
pub mod remote_desktop;
pub mod screencast;
pub mod screenshot;
pub mod settings;
pub mod wallpaper;

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
    /// The bus name taken ([`BUS_NAME`] unless overridden).
    name: zbus::names::WellKnownName<'static>,
    registry: SharedRegistry,
    tasks: Vec<JoinHandle<()>>,
}

impl PortalService {
    /// Export the backends on `builder`'s bus and take [`BUS_NAME`].
    pub async fn start(
        builder: connection::Builder<'_>,
        settings: Arc<dyn SettingsSource>,
    ) -> zbus::Result<Self> {
        // ScreenCast and RemoteDesktop share their sessions.
        let casts = screencast::Shared::default();
        let conn = builder
            .serve_at(OBJECT_PATH, SettingsBackend::new(settings))?
            .serve_at(OBJECT_PATH, screenshot::ScreenshotBackend)?
            .serve_at(OBJECT_PATH, screencast::ScreenCastBackend { shared: casts.clone() })?
            .serve_at(OBJECT_PATH, remote_desktop::RemoteDesktopBackend { shared: casts.clone() })?
            .serve_at(OBJECT_PATH, clipboard::ClipboardBackend { shared: casts })?
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
        // Backends whose dialogs the shell shows.
        let server = conn.object_server();
        server.at(OBJECT_PATH, dialogs::AccessBackend { conn: conn.clone() }).await?;
        server.at(OBJECT_PATH, dialogs::AccountBackend { conn: conn.clone() }).await?;
        server.at(OBJECT_PATH, dialogs::AppChooserBackend { conn: conn.clone() }).await?;
        server.at(OBJECT_PATH, dialogs::BackgroundBackend { conn: conn.clone() }).await?;
        server.at(OBJECT_PATH, wallpaper::WallpaperBackend { conn: conn.clone() }).await?;
        server.at(OBJECT_PATH, lockdown::LockdownBackend::default()).await?;
        lockdown::refresh(&conn, OBJECT_PATH).await?;

        // Tests may run a second backend beside the real one.
        let name = std::env::var("HELIX_PORTALS_BUS_NAME").unwrap_or_else(|_| BUS_NAME.to_owned());
        let name = zbus::names::WellKnownName::try_from(name).map_err(zbus::Error::from)?;
        match conn
            .request_name_with_flags(name.clone(), RequestNameFlags::DoNotQueue.into())
            .await?
        {
            RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner => {}
            _ => return Err(zbus::Error::NameTaken),
        }
        tracing::info!(%name, "portal backends ready");

        let tasks = vec![
            spawn_owner_watcher(&conn, sm.clone(), registry.clone()).await?,
            spawn_session_watcher(&conn, sm.clone(), registry.clone()).await?,
            spawn_session_client(&conn, sm, registry.clone()),
            tokio::spawn(dialogs::follow_running(conn.clone(), OBJECT_PATH)),
        ];
        Ok(Self {
            conn,
            name,
            registry,
            tasks,
        })
    }

    /// Call when a lockdown setting changed (see [`lockdown::SCHEMAS`]).
    pub async fn lockdown_changed(&self) -> zbus::Result<()> {
        lockdown::refresh(&self.conn, OBJECT_PATH).await
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
        self.conn.release_name(self.name.clone()).await?;
        // Exported objects hold SessionManager proxies, which hold the
        // connection: unregister them or the connection never drops.
        inhibit::release_all(&self.conn, &self.registry).await;
        let server = self.conn.object_server();
        server.remove::<InhibitBackend, _>(OBJECT_PATH).await?;
        server.remove::<SettingsBackend, _>(OBJECT_PATH).await?;
        server.remove::<screenshot::ScreenshotBackend, _>(OBJECT_PATH).await?;
        server.remove::<screencast::ScreenCastBackend, _>(OBJECT_PATH).await?;
        server.remove::<remote_desktop::RemoteDesktopBackend, _>(OBJECT_PATH).await?;
        server.remove::<clipboard::ClipboardBackend, _>(OBJECT_PATH).await?;
        // These hold the connection too.
        server.remove::<dialogs::AccessBackend, _>(OBJECT_PATH).await?;
        server.remove::<dialogs::AccountBackend, _>(OBJECT_PATH).await?;
        server.remove::<dialogs::AppChooserBackend, _>(OBJECT_PATH).await?;
        server.remove::<dialogs::BackgroundBackend, _>(OBJECT_PATH).await?;
        server.remove::<wallpaper::WallpaperBackend, _>(OBJECT_PATH).await?;
        server.remove::<lockdown::LockdownBackend, _>(OBJECT_PATH).await?;
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

/// Register as a session client so the end-of-session protocol reaches the
/// inhibit portal's monitors (query end → state 2, end → state 3, cancel →
/// state 1), as GNOME's portal backend does.
fn spawn_session_client(
    conn: &Connection,
    sm: SessionManagerProxy<'static>,
    registry: SharedRegistry,
) -> JoinHandle<()> {
    let conn = conn.clone();
    tokio::spawn(async move {
        let path = match sm.register_client(BUS_NAME, "").await {
            Ok(path) => path,
            Err(err) => {
                tracing::info!(%err, "no session manager to register with");
                return;
            }
        };
        let rule = match zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface("org.gnome.SessionManager.ClientPrivate")
            .and_then(|b| b.path(path.clone()))
        {
            Ok(b) => b.build(),
            Err(err) => {
                tracing::warn!(%err, "bad session client match rule");
                return;
            }
        };
        let Ok(mut signals) = zbus::MessageStream::for_match_rule(rule, &conn, None).await else {
            tracing::warn!("cannot watch session client signals");
            return;
        };
        let respond = |ok: bool| {
            let conn = conn.clone();
            let path = path.clone();
            async move {
                let _ = conn
                    .call_method(
                        Some("org.gnome.SessionManager"),
                        path.as_str(),
                        Some("org.gnome.SessionManager.ClientPrivate"),
                        "EndSessionResponse",
                        &(ok, ""),
                    )
                    .await;
            }
        };
        while let Some(Ok(message)) = signals.next().await {
            match message.header().member().map(|m| m.as_str()) {
                Some("QueryEndSession") => {
                    // Answer within the protocol's one second.
                    inhibit::query_end(&conn, &registry, std::time::Duration::from_millis(800)).await;
                    respond(true).await;
                }
                Some("EndSession") => {
                    inhibit::broadcast_state(&conn, &registry, session_state::ENDING).await;
                    respond(true).await;
                }
                Some("CancelEndSession") => {
                    inhibit::broadcast_state(&conn, &registry, session_state::RUNNING).await;
                }
                _ => {}
            }
        }
    })
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
