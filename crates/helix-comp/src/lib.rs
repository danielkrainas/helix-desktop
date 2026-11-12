//! Helix compositor services.
//!
//! The compositor itself (Smithay, in the `helix-comp` binary) runs on the
//! main thread. Its D-Bus surface runs on a tokio runtime and is fed by
//! [`Event`]s:
//!
//! - [`idle_monitor`] — `org.gnome.Mutter.IdleMonitor`
//! - [`display_config`] — `org.gnome.Mutter.DisplayConfig`
//! - [`input_settings`] — keyboard, mouse and touchpad GSettings

pub mod a11y_keys;
pub mod a11y_mouse;
pub mod hot_corner;
pub mod magnifier;
pub mod backlight;
pub mod display_config;
pub mod edid;
pub mod tablet;
pub mod focus_tracking;
pub mod input_capture;
pub mod frames;
pub mod idle;
pub mod idle_monitor;
pub mod lid;
pub mod input_settings;
pub mod remote_desktop;
pub mod rotation;
pub mod screencast;
pub mod wm_settings;
pub mod workspaces;
pub mod xsettings;

use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use zbus::fdo::{DBusProxy, ObjectManager, RequestNameFlags, RequestNameReply};
use zbus::object_server::SignalEmitter;
use zbus::{connection, Connection};

use crate::display_config::{ApplyRequest, DisplayConfig, Layout, Monitor, Store};
use crate::idle_monitor::{IdleMonitor, Shared};

/// Something the compositor tells its D-Bus services.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Any user input.
    Activity,
    /// Whether a shown surface inhibits idling.
    IdleInhibited(bool),
    /// The output layout is now this.
    Monitors(Vec<Monitor>),
    /// The overlay key (Super alone): show or hide the overview.
    ToggleOverview,
    /// The user asked to lock the screen (Super+L).
    Lock,
    /// A screen cast stream ended on the compositor's side (monitor gone,
    /// PipeWire error).
    ScreenCastEnded(u32),
    /// The clipboard changed, for a remote desktop session sharing it.
    RemoteSelectionOwner { session: u32, mime_types: Vec<String>, is_owner: bool },
    /// An app pastes a remote desktop session's clipboard content.
    RemoteSelectionTransfer { session: u32, mime_type: String, serial: u32 },
    /// The keymap or its active layout changed (`CurrentKeymap`).
    KeymapChanged,
    /// An input capture session's signal.
    Capture(input_capture::CaptureSignal),
    /// Whether the magnifier follows keyboard focus or the text caret
    /// (handled where the D-Bus thread starts, see `focus_tracking`).
    FocusTracking(bool),
}

/// The running D-Bus services of the compositor.
pub struct CompositorServices {
    conn: Connection,
    idle: Shared,
    layout: Arc<Mutex<Layout>>,
    tasks: Vec<JoinHandle<()>>,
    casts: screencast::Shared,
    cast: Option<mpsc::UnboundedSender<screencast::CastRequest>>,
    remotes: remote_desktop::Shared,
    captures: input_capture::Shared,
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

/// How monitor configuration changes reach the compositor.
#[derive(Debug)]
pub struct MonitorControl {
    /// Where `ApplyMonitorsConfig` requests go; `None` refuses them.
    pub applier: Option<mpsc::UnboundedSender<ApplyRequest>>,
    /// Remembered configurations.
    pub store: Store,
    /// Seconds the user has to keep a persistent change (the shell asks);
    /// 0 keeps changes without asking.
    pub confirm_seconds: u32,
    /// The built-in panel's backlight (`None`: no backlight control).
    pub backlight: Option<backlight::Backlight>,
    /// Where gamma requests go (`None`: no gamma control).
    pub gamma: Option<mpsc::UnboundedSender<display_config::ColorRequest>>,
    /// Where screen cast requests go (`None`: no screen casting).
    pub cast: Option<mpsc::UnboundedSender<screencast::CastRequest>>,
}

impl Default for MonitorControl {
    fn default() -> Self {
        Self {
            applier: None,
            store: Store::default(),
            confirm_seconds: display_config::CONFIRM_SECONDS,
            backlight: None,
            gamma: None,
            cast: None,
        }
    }
}

impl CompositorServices {
    /// Serve with monitor changes refused (`ApplyMonitorsConfigAllowed` false).
    pub async fn start(builder: connection::Builder<'_>, monitors: Vec<Monitor>) -> zbus::Result<Self> {
        Self::start_with(builder, monitors, MonitorControl::default()).await
    }

    pub async fn start_with(
        builder: connection::Builder<'_>,
        monitors: Vec<Monitor>,
        control: MonitorControl,
    ) -> zbus::Result<Self> {
        let idle = Shared::new();
        let layout = Arc::new(Mutex::new(Layout::new(monitors)));
        let watch_backlight = control.backlight.is_some();
        let can_apply = control.applier.is_some();
        let casts = screencast::Shared::default();
        let remotes = remote_desktop::Shared::default();
        let captures = input_capture::Shared::default();
        let cast = control.cast.clone();
        let effects = display_config::ScreenEffects { gamma: control.gamma.clone(), grayscale: false };
        let display_config = DisplayConfig::new(
            layout.clone(),
            control.applier,
            control.store,
            control.confirm_seconds,
            control.backlight,
            control.gamma,
        );
        let conn = builder
            .serve_at(idle_monitor::MANAGER_PATH, ObjectManager)?
            .serve_at(idle_monitor::CORE_PATH, IdleMonitor::new(idle.clone()))?
            .serve_at(display_config::OBJECT_PATH, display_config)?
            .serve_at(display_config::OBJECT_PATH, effects)?
            .serve_at(
                screencast::OBJECT_PATH,
                screencast::ScreenCast {
                    shared: casts.clone(),
                    layout: layout.clone(),
                    compositor: control.cast.clone(),
                    remotes: remotes.clone(),
                },
            )?
            .serve_at(
                remote_desktop::OBJECT_PATH,
                remote_desktop::RemoteDesktop { shared: remotes.clone(), casts: casts.clone(), compositor: control.cast },
            )?
            .serve_at(screencast::OBJECT_PATH, screencast::ScreenCastState { shared: casts.clone() })?
            .serve_at(input_capture::OBJECT_PATH, input_capture::InputCapture { shared: captures.clone(), compositor: cast.clone() })?
            .build()
            .await?;
        own(&conn, idle_monitor::BUS_NAME).await?;
        own(&conn, display_config::BUS_NAME).await?;
        own(&conn, screencast::BUS_NAME).await?;
        own(&conn, remote_desktop::BUS_NAME).await?;
        own(&conn, input_capture::BUS_NAME).await?;
        tracing::info!("IdleMonitor and DisplayConfig ready");

        let mut tasks = vec![
            spawn_idle_timer(&conn, idle.clone())?,
            spawn_owner_watcher(&conn, idle.clone()).await?,
        ];
        if watch_backlight {
            tasks.push(spawn_backlight_watcher(&conn));
        }
        if can_apply {
            tasks.push(tokio::spawn(rotation::follow(conn.clone())));
            // The laptop lid turns the built-in monitor off and on.
            tasks.push(tokio::spawn(lid::follow(conn.clone())));
        }
        if let Some(cast) = &cast {
            tasks.push(spawn_cast_owner_watcher(&conn, casts.clone(), remotes.clone(), captures.clone(), cast.clone()).await?);
        }
        Ok(Self {
            conn,
            idle,
            layout,
            tasks,
            casts,
            cast,
            remotes,
            captures,
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
            Event::RemoteSelectionOwner { session, mime_types, is_owner } => {
                let path = remote_desktop::path_of(session);
                let emitter = SignalEmitter::new(&self.conn, path.as_str())?;
                remote_desktop::Session::selection_owner_changed(&emitter, remote_desktop::owner_options(mime_types, is_owner)).await
            }
            Event::RemoteSelectionTransfer { session, mime_type, serial } => {
                let path = remote_desktop::path_of(session);
                let emitter = SignalEmitter::new(&self.conn, path.as_str())?;
                remote_desktop::Session::selection_transfer(&emitter, &mime_type, serial).await
            }
            Event::KeymapChanged => {
                for id in remote_desktop::session_ids(&self.remotes) {
                    let path = remote_desktop::path_of(id);
                    if let Ok(iface) = self.conn.object_server().interface::<_, remote_desktop::Session>(path.as_str()).await {
                        iface.get().await.current_keymap_changed(iface.signal_emitter()).await?;
                    }
                }
                Ok(())
            }
            Event::FocusTracking(_) => Ok(()),
            Event::Capture(signal) => input_capture::emit(&self.conn, &self.captures, signal).await,
            Event::IdleInhibited(inhibited) => {
                self.idle.set_inhibited(inhibited);
                Ok(())
            }
            Event::Monitors(monitors) => {
                let (changed, set_changed) = {
                    let mut layout = self.layout.lock().unwrap_or_else(|e| e.into_inner());
                    let before = layout.key();
                    let changed = layout.update(monitors);
                    (changed, layout.key() != before)
                };
                if changed {
                    let iface = self
                        .conn
                        .object_server()
                        .interface::<_, DisplayConfig>(display_config::OBJECT_PATH)
                        .await?;
                    let emitter = iface.signal_emitter();
                    DisplayConfig::monitors_changed(emitter).await?;
                    iface.get().await.has_external_monitor_changed(emitter).await?;
                    iface.get().await.backlight_changed(emitter).await?;
                    if set_changed {
                        iface.get().await.restore_saved(emitter).await;
                    }
                }
                Ok(())
            }
            Event::Lock => {
                let reply = self
                    .conn
                    .call_method(
                        Some("org.gnome.ScreenSaver"),
                        "/org/gnome/ScreenSaver",
                        Some("org.gnome.ScreenSaver"),
                        "Lock",
                        &(),
                    )
                    .await;
                if let Err(err) = reply {
                    tracing::warn!(%err, "cannot ask the shell to lock");
                }
                Ok(())
            }
            Event::ScreenCastEnded(stream) => {
                let session = self.casts.lock().unwrap_or_else(|e| e.into_inner()).session_of(stream);
                if let (Some(session), Some(cast)) = (session, &self.cast) {
                    screencast::close_session(&self.conn, &self.casts, cast, session).await;
                }
                Ok(())
            }
            Event::ToggleOverview => {
                // The overview belongs to the shell; ask it over its contract.
                let toggled = async {
                    let shell = zbus::Proxy::new(&self.conn, "org.gnome.Shell", "/org/gnome/Shell", "org.gnome.Shell").await?;
                    let active: bool = shell.get_property("OverviewActive").await?;
                    shell.set_property("OverviewActive", !active).await.map_err(zbus::Error::from)
                };
                if let Err(err) = toggled.await {
                    tracing::info!(%err, "no shell to show the overview");
                }
                Ok(())
            }
        }
    }

    /// Bring back the configuration remembered for the current monitors.
    pub async fn restore_saved(&self) {
        let Ok(iface) = self
            .conn
            .object_server()
            .interface::<_, DisplayConfig>(display_config::OBJECT_PATH)
            .await
        else {
            return;
        };
        iface.get().await.restore_saved(iface.signal_emitter()).await;
    }

    /// Restore remembered monitors, then handle events until the sender
    /// side is dropped.
    pub async fn run(self, mut events: mpsc::UnboundedReceiver<Event>) {
        self.restore_saved().await;
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

/// How often the backlight is re-read for changes made elsewhere.
const BACKLIGHT_POLL: std::time::Duration = std::time::Duration::from_secs(2);

/// Publish brightness changes made outside DisplayConfig.
fn spawn_backlight_watcher(conn: &Connection) -> JoinHandle<()> {
    let conn = conn.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(BACKLIGHT_POLL).await;
            let Ok(iface) = conn
                .object_server()
                .interface::<_, DisplayConfig>(display_config::OBJECT_PATH)
                .await
            else {
                return;
            };
            let dc = iface.get().await;
            if dc.backlight_moved() {
                if let Err(err) = dc.backlight_changed(iface.signal_emitter()).await {
                    tracing::warn!(%err, "cannot publish the backlight");
                }
            }
        }
    })
}

/// End screen cast sessions whose creator left the bus.
async fn spawn_cast_owner_watcher(
    conn: &Connection,
    casts: screencast::Shared,
    remotes: remote_desktop::Shared,
    captures: input_capture::Shared,
    cast: mpsc::UnboundedSender<screencast::CastRequest>,
) -> zbus::Result<JoinHandle<()>> {
    let mut changes = DBusProxy::new(conn).await?.receive_name_owner_changed().await?;
    let conn = conn.clone();
    Ok(tokio::spawn(async move {
        while let Some(signal) = changes.next().await {
            let Ok(args) = signal.args() else { continue };
            let name = args.name().as_str();
            if name.starts_with(':') && args.new_owner().is_none() {
                let remote_sessions = remotes.lock().unwrap_or_else(|e| e.into_inner()).sessions_of(name);
                for session in remote_sessions {
                    remote_desktop::close_session(&conn, &remotes, &casts, &cast, session).await;
                }
                let sessions = casts.lock().unwrap_or_else(|e| e.into_inner()).sessions_of(name);
                for session in sessions {
                    screencast::close_session(&conn, &casts, &cast, session).await;
                }
                for session in input_capture::sessions_of(&captures, name) {
                    input_capture::close_session(&conn, &captures, &cast, session).await;
                }
            }
        }
    }))
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
