//! What the session manager asks of systemd-logind (helix-desktop-spec
//! `org.freedesktop.login1.md`): power off or restart once the session has
//! ended, suspend, sleep inhibitors, and whether the session is active.

use futures_util::StreamExt;
use tokio::task::JoinHandle;
use zbus::zvariant::{OwnedFd, OwnedObjectPath};
use zbus::Connection;

use crate::state::EndSessionAction;

pub const BUS_NAME: &str = "org.freedesktop.login1";
const MANAGER_PATH: &str = "/org/freedesktop/login1";
const MANAGER_IFACE: &str = "org.freedesktop.login1.Manager";
const SESSION_IFACE: &str = "org.freedesktop.login1.Session";

/// A connection to the bus logind is on (the system bus, or a test bus).
#[derive(Clone, Debug)]
pub struct Logind {
    conn: Connection,
}

impl Logind {
    pub fn new(conn: Connection) -> Self {
        Self { conn }
    }

    /// logind on the system bus, if there is one.
    pub async fn system() -> Option<Self> {
        match Connection::system().await {
            Ok(conn) => Some(Self::new(conn)),
            Err(err) => {
                tracing::warn!(%err, "no system bus; power actions and SessionIsActive unavailable");
                None
            }
        }
    }

    async fn call<B>(&self, method: &str, body: &B) -> zbus::Result<zbus::Message>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        self.conn
            .call_method(Some(BUS_NAME), MANAGER_PATH, Some(MANAGER_IFACE), method, body)
            .await
    }

    pub async fn power_off(&self) -> zbus::Result<()> {
        self.call("PowerOff", &(false,)).await.map(drop)
    }

    pub async fn reboot(&self) -> zbus::Result<()> {
        self.call("Reboot", &(false,)).await.map(drop)
    }

    pub async fn suspend(&self) -> zbus::Result<()> {
        self.call("Suspend", &(false,)).await.map(drop)
    }

    /// `CanPowerOff`, `CanReboot`, `CanSuspend`: `yes`, `challenge`, `no`, `na`.
    pub async fn can(&self, method: &str) -> zbus::Result<String> {
        let reply = self.call(method, &()).await?;
        reply.body().deserialize::<String>()
    }

    /// Take an inhibitor; it lasts until the returned descriptor is dropped.
    pub async fn inhibit(&self, what: &str, why: &str, mode: &str) -> zbus::Result<OwnedFd> {
        let reply = self.call("Inhibit", &(what, "Helix", why, mode)).await?;
        reply.body().deserialize::<OwnedFd>()
    }

    /// The logind session this process belongs to.
    pub async fn own_session(&self) -> zbus::Result<OwnedObjectPath> {
        let reply = match std::env::var("XDG_SESSION_ID").ok().filter(|id| !id.is_empty()) {
            Some(id) => self.call("GetSession", &(id,)).await?,
            None => self.call("GetSessionByPID", &(std::process::id(),)).await?,
        };
        reply.body().deserialize::<OwnedObjectPath>()
    }

    /// Set a `/sys/class/<subsystem>/<name>` device's brightness through
    /// our session (no sysfs write access needed).
    pub async fn set_brightness(&self, subsystem: &str, name: &str, value: u32) -> zbus::Result<()> {
        let path = self.own_session().await?;
        self.conn
            .call_method(Some(BUS_NAME), path, Some(SESSION_IFACE), "SetBrightness", &(subsystem, name, value))
            .await
            .map(drop)
    }

    /// Follow the session's `Active` property: `on_change` gets the current
    /// value, then every change.
    pub async fn watch_active<F, Fut>(&self, mut on_change: F) -> zbus::Result<JoinHandle<()>>
    where
        F: FnMut(bool) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let path = self.own_session().await?;
        let proxy = zbus::Proxy::new(&self.conn, BUS_NAME, path, SESSION_IFACE).await?;
        let mut changes = proxy.receive_property_changed::<bool>("Active").await;
        let initial = proxy.get_property::<bool>("Active").await?;
        Ok(tokio::spawn(async move {
            on_change(initial).await;
            while let Some(change) = changes.next().await {
                match change.get().await {
                    Ok(active) => on_change(active).await,
                    Err(err) => tracing::warn!(%err, "cannot read logind session Active"),
                }
            }
        }))
    }
}

/// After the session ended: power off or restart if that was asked for.
pub async fn after_session(action: EndSessionAction, logind: Option<&Logind>) {
    let what = match action {
        EndSessionAction::Shutdown => "power off",
        EndSessionAction::Reboot => "restart",
        _ => return,
    };
    let Some(logind) = logind else {
        tracing::warn!("no logind; the session ended but the system will not {what}");
        return;
    };
    tracing::info!("asking logind to {what}");
    let result = if action == EndSessionAction::Shutdown {
        logind.power_off().await
    } else {
        logind.reboot().await
    };
    if let Err(err) = result {
        tracing::warn!(%err, "logind refused to {what}");
    }
}
