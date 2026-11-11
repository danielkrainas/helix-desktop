//! Helix compositor services.
//!
//! The compositor itself (Smithay, in the `helix-comp` binary) runs on the
//! main thread. Its D-Bus surface runs on a tokio runtime and is fed by
//! [`Event`]s:
//!
//! - [`idle_monitor`] — `org.gnome.Mutter.IdleMonitor`
//! - [`display_config`] — `org.gnome.Mutter.DisplayConfig` (read side)

pub mod display_config;
pub mod idle;
pub mod idle_monitor;

use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use zbus::fdo::{DBusProxy, ObjectManager, RequestNameFlags, RequestNameReply};
use zbus::object_server::SignalEmitter;
use zbus::{connection, Connection};

use crate::display_config::{DisplayConfig, Layout, Monitor};
use crate::idle_monitor::{IdleMonitor, Shared};

/// Something the compositor tells its D-Bus services.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Any user input.
    Activity,
    /// The output layout is now this.
    Monitors(Vec<Monitor>),
}

/// The running D-Bus services of the compositor.
pub struct CompositorServices {
    conn: Connection,
    idle: Shared,
    layout: Arc<Mutex<Layout>>,
    tasks: Vec<JoinHandle<()>>,
}

async fn own(conn: &Connection, name: &'static str) -> zbus::Result<()> {
    match conn
        .request_name_with_flags(name, RequestNameFlags::DoNotQueue.into())
        .await?
    {
        RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner => Ok(()),
        _ => Err(zbus::Error::NameTaken),
    }
}

impl CompositorServices {
    pub async fn start(
        builder: connection::Builder<'_>,
        monitors: Vec<Monitor>,
    ) -> zbus::Result<Self> {
        let idle = Shared::new();
        let layout = Arc::new(Mutex::new(Layout::new(monitors)));
        let conn = builder
            .serve_at(idle_monitor::MANAGER_PATH, ObjectManager)?
            .serve_at(idle_monitor::CORE_PATH, IdleMonitor::new(idle.clone()))?
            .serve_at(display_config::OBJECT_PATH, DisplayConfig::new(layout.clone()))?
            .build()
            .await?;
        own(&conn, idle_monitor::BUS_NAME).await?;
        own(&conn, display_config::BUS_NAME).await?;
        tracing::info!("IdleMonitor and DisplayConfig ready");

        let tasks = vec![
            spawn_idle_timer(&conn, idle.clone())?,
            spawn_owner_watcher(&conn, idle.clone()).await?,
        ];
        Ok(Self {
            conn,
            idle,
            layout,
            tasks,
        })
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    pub async fn handle(&self, event: Event) -> zbus::Result<()> {
        match event {
            Event::Activity => {
                let emitter = SignalEmitter::new(&self.conn, idle_monitor::CORE_PATH)?;
                self.idle.activity(&emitter).await
            }
            Event::Monitors(monitors) => {
                let changed = self
                    .layout
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .update(monitors);
                if changed {
                    let iface = self
                        .conn
                        .object_server()
                        .interface::<_, DisplayConfig>(display_config::OBJECT_PATH)
                        .await?;
                    let emitter = iface.signal_emitter();
                    DisplayConfig::monitors_changed(emitter).await?;
                    iface.get().await.has_external_monitor_changed(emitter).await?;
                }
                Ok(())
            }
        }
    }

    /// Handle events until the sender side is dropped.
    pub async fn run(self, mut events: mpsc::UnboundedReceiver<Event>) {
        while let Some(event) = events.recv().await {
            if let Err(err) = self.handle(event).await {
                tracing::warn!(%err, "failed to publish compositor event");
            }
        }
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// Fire idle watches when they come due.
fn spawn_idle_timer(conn: &Connection, idle: Shared) -> zbus::Result<JoinHandle<()>> {
    let emitter = SignalEmitter::new(conn, idle_monitor::CORE_PATH)?.into_owned();
    Ok(tokio::spawn(async move {
        loop {
            let due = idle.core().next_due(idle.clock.now());
            match due {
                Some(ms) => {
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_millis(ms)) => {}
                        _ = idle.rearm.notified() => continue,
                    }
                }
                None => {
                    idle.rearm.notified().await;
                    continue;
                }
            }
            if let Err(err) = idle.poll(&emitter).await {
                tracing::warn!(%err, "failed to emit WatchFired");
            }
        }
    }))
}

/// Drop watches of clients that leave the bus.
async fn spawn_owner_watcher(conn: &Connection, idle: Shared) -> zbus::Result<JoinHandle<()>> {
    let mut changes = DBusProxy::new(conn).await?.receive_name_owner_changed().await?;
    Ok(tokio::spawn(async move {
        while let Some(signal) = changes.next().await {
            let Ok(args) = signal.args() else { continue };
            let name = args.name().as_str();
            if name.starts_with(':') && args.new_owner().is_none() {
                let removed = idle.core().remove_owner(name);
                if removed > 0 {
                    tracing::debug!(name, removed, "dropped idle watches of departed client");
                    idle.rearm.notify_one();
                }
            }
        }
    }))
}
