//! Helix shell.
//!
//! - [`ShellService`] serves `org.gnome.Shell` (contract:
//!   `helix-desktop-spec/contracts/dbus/org.gnome.Shell.xml` and `.md`).
//! - Requests that need UI (OSD, overview) are published as [`UiEvent`]s for
//!   the shell UI.

pub mod accel;
pub mod auth;
pub mod font;
pub mod idle;
pub mod launcher;
pub mod screensaver;
pub mod state;
pub mod text;
pub mod ui;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use zbus::fdo::{DBusProxy, RequestNameFlags, RequestNameReply};
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Value};
use zbus::{connection, interface, Connection};

use crate::state::ShellState;

pub const BUS_NAME: &str = "org.gnome.Shell";
pub const OBJECT_PATH: &str = "/org/gnome/Shell";
/// The GNOME version whose surface Helix implements (see gaps.md).
pub const SHELL_VERSION: &str = "51.0";

/// An on-screen display request.
#[derive(Debug, Clone, PartialEq)]
pub struct Osd {
    pub icon: Option<String>,
    pub label: Option<String>,
    pub level: Option<f64>,
    pub max_level: f64,
    pub connector: Option<String>,
}

impl Osd {
    fn from_params(params: &HashMap<String, OwnedValue>) -> Self {
        let string = |k: &str| match params.get(k).map(|v| &**v) {
            Some(Value::Str(s)) => Some(s.to_string()),
            _ => None,
        };
        let double = |k: &str| match params.get(k).map(|v| &**v) {
            Some(Value::F64(d)) => Some(*d),
            _ => None,
        };
        Self {
            icon: string("icon"),
            label: string("label"),
            level: double("level"),
            max_level: double("max_level").unwrap_or(1.0),
            connector: string("connector"),
        }
    }
}

/// Requests for the shell UI.
#[derive(Debug, Clone, PartialEq)]
pub enum UiEvent {
    Overview(bool),
    Osd(Osd),
    MonitorLabels(HashMap<String, i32>),
    HideMonitorLabels,
    /// Ask the compositor to deliver this accelerator for `action`.
    Bind { action: u32, accelerator: accel::Accelerator },
    Unbind { action: u32 },
    /// Lock the session now.
    Lock,
}

/// Reports from the shell UI back to the D-Bus side.
#[derive(Debug, Clone, PartialEq)]
pub enum UiRequest {
    /// The UI opened/closed the launcher itself.
    Overview(bool),
    /// A bound accelerator was pressed or released.
    Accelerator { action: u32, pressed: bool, time: u32 },
    /// The compositor refused (or cannot offer) the binding.
    GrabFailed { action: u32 },
    /// The lock is in place.
    Locked,
    /// The user unlocked.
    Unlocked,
    /// The lock was refused or taken away.
    LockGone,
}

/// The exported `org.gnome.Shell` object.
pub struct Shell {
    state: Arc<Mutex<ShellState>>,
    ui: mpsc::UnboundedSender<UiEvent>,
}

impl Shell {
    fn state(&self) -> MutexGuard<'_, ShellState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn bind_in_compositor(&self, action: u32) {
        if action == 0 {
            return;
        }
        let accelerator = self.state().grab_for(action).map(|g| g.accelerator.clone());
        if let Some(accelerator) = accelerator {
            let _ = self.ui.send(UiEvent::Bind { action, accelerator });
        }
    }

    /// Emit AcceleratorActivated/Deactivated if the grab applies in the
    /// current action mode.
    pub async fn accelerator_event(
        &self,
        action: u32,
        pressed: bool,
        time: u32,
        emitter: &SignalEmitter<'_>,
    ) -> zbus::Result<()> {
        let (applies, mode) = {
            let state = self.state();
            let mode = state.action_mode();
            let applies = state.grab_for(action).is_some_and(|g| g.mode_flags & mode != 0);
            (applies, mode)
        };
        if !applies {
            return Ok(());
        }
        let params = HashMap::from([
            ("device-id", Value::from(0u32)),
            ("timestamp", Value::from(time)),
            ("action-mode", Value::from(mode)),
            ("device-node", Value::from("")),
        ]);
        tracing::debug!(action, pressed, "accelerator");
        if pressed {
            Self::accelerator_activated(emitter, action, params).await
        } else {
            Self::accelerator_deactivated(emitter, action, params).await
        }
    }

    /// The compositor could not bind the accelerator; forget the grab.
    pub fn grab_failed(&self, action: u32) {
        if self.state().ungrab(action) {
            tracing::warn!(action, "accelerator could not be bound by the compositor; grab dropped");
        }
    }

    /// Overview state changed by the shell UI itself (no echo to the UI).
    pub async fn set_overview_from_ui(&self, active: bool, emitter: &SignalEmitter<'_>) -> zbus::Result<()> {
        if self.state().set_overview_active(active) {
            self.overview_active_changed(emitter).await?;
        }
        Ok(())
    }

    async fn show_overview(&self, emitter: &SignalEmitter<'_>) -> zbus::Result<()> {
        if self.state().set_overview_active(true) {
            let _ = self.ui.send(UiEvent::Overview(true));
            self.overview_active_changed(emitter).await?;
        }
        Ok(())
    }
}

fn sender(header: &Header<'_>) -> Option<String> {
    header.sender().map(|s| s.to_string())
}

#[interface(name = "org.gnome.Shell")]
impl Shell {
    async fn eval(&self, _script: &str) -> (bool, String) {
        // Eval is only available in unsafe mode, which Helix does not have.
        (false, String::new())
    }

    async fn focus_search(&self, #[zbus(signal_emitter)] e: SignalEmitter<'_>) -> zbus::fdo::Result<()> {
        Ok(self.show_overview(&e).await?)
    }

    #[zbus(name = "ShowOSD")]
    async fn show_osd(&self, params: HashMap<String, OwnedValue>) {
        let osd = Osd::from_params(&params);
        tracing::info!(?osd, "OSD");
        let _ = self.ui.send(UiEvent::Osd(osd));
    }

    async fn show_monitor_labels(&self, params: HashMap<String, OwnedValue>) {
        let labels = params
            .iter()
            .filter_map(|(connector, v)| match &**v {
                Value::I32(n) => Some((connector.clone(), *n)),
                Value::Value(inner) => match **inner {
                    Value::I32(n) => Some((connector.clone(), n)),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        let _ = self.ui.send(UiEvent::MonitorLabels(labels));
    }

    async fn hide_monitor_labels(&self) {
        let _ = self.ui.send(UiEvent::HideMonitorLabels);
    }

    async fn focus_app(&self, id: &str, #[zbus(signal_emitter)] e: SignalEmitter<'_>) -> zbus::fdo::Result<()> {
        tracing::info!(id, "FocusApp");
        Ok(self.show_overview(&e).await?)
    }

    async fn show_applications(&self, #[zbus(signal_emitter)] e: SignalEmitter<'_>) -> zbus::fdo::Result<()> {
        Ok(self.show_overview(&e).await?)
    }

    async fn grab_accelerator(
        &self,
        accelerator: &str,
        mode_flags: u32,
        grab_flags: u32,
        #[zbus(header)] header: Header<'_>,
    ) -> u32 {
        let action = self
            .state()
            .grab(accelerator, mode_flags, grab_flags, sender(&header).as_deref());
        tracing::info!(accelerator, action, "GrabAccelerator");
        self.bind_in_compositor(action);
        action
    }

    async fn grab_accelerators(
        &self,
        accelerators: Vec<(String, u32, u32)>,
        #[zbus(header)] header: Header<'_>,
    ) -> Vec<u32> {
        let owner = sender(&header);
        let actions: Vec<u32> = {
            let mut state = self.state();
            accelerators
                .iter()
                .map(|(accel, modes, flags)| state.grab(accel, *modes, *flags, owner.as_deref()))
                .collect()
        };
        for action in &actions {
            self.bind_in_compositor(*action);
        }
        actions
    }

    /// False if `action` was not grabbed.
    async fn ungrab_accelerator(&self, action: u32) -> bool {
        let found = self.state().ungrab(action);
        if found {
            let _ = self.ui.send(UiEvent::Unbind { action });
        }
        found
    }

    /// Releases every listed grab; false if any of them was not grabbed.
    async fn ungrab_accelerators(&self, actions: Vec<u32>) -> bool {
        let mut state = self.state();
        let mut all_found = true;
        for action in actions {
            // No short-circuit: release the rest even after a miss.
            let found = state.ungrab(action);
            if found {
                let _ = self.ui.send(UiEvent::Unbind { action });
            }
            all_found &= found;
        }
        all_found
    }

    async fn screen_transition(&self) {
        // No transition effect yet.
    }

    #[zbus(property)]
    async fn mode(&self) -> String {
        "user".to_owned()
    }

    #[zbus(property)]
    async fn overview_active(&self) -> bool {
        self.state().overview_active()
    }

    #[zbus(property)]
    async fn set_overview_active(&self, active: bool) {
        if self.state().set_overview_active(active) {
            let _ = self.ui.send(UiEvent::Overview(active));
        }
    }

    #[zbus(property)]
    async fn shell_version(&self) -> String {
        SHELL_VERSION.to_owned()
    }

    #[zbus(signal)]
    pub async fn accelerator_activated(
        emitter: &SignalEmitter<'_>,
        action: u32,
        parameters: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn accelerator_deactivated(
        emitter: &SignalEmitter<'_>,
        action: u32,
        parameters: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<()>;
}

/// The running `org.gnome.Shell` service.
pub struct ShellService {
    conn: Connection,
    state: Arc<Mutex<ShellState>>,
    watcher: JoinHandle<()>,
    auth: Arc<dyn auth::Authenticator>,
}

impl ShellService {
    /// Serve `org.gnome.Shell`; UI requests arrive on the returned receiver.
    pub async fn start(
        builder: connection::Builder<'_>,
    ) -> zbus::Result<(Self, mpsc::UnboundedReceiver<UiEvent>)> {
        let state = Arc::new(Mutex::new(ShellState::new()));
        let (ui, ui_rx) = mpsc::unbounded_channel();
        let ui_for_watcher = ui.clone();
        let auth = auth::authenticator();
        let saver = screensaver::ScreenSaver {
            status: Default::default(),
            ui: ui.clone(),
            auth: auth.clone(),
        };
        let conn = builder
            .serve_at(OBJECT_PATH, Shell { state: state.clone(), ui })?
            .serve_at(screensaver::OBJECT_PATH, saver)?
            .build()
            .await?;
        match conn
            .request_name_with_flags(BUS_NAME, RequestNameFlags::DoNotQueue.into())
            .await?
        {
            RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner => {}
            _ => return Err(zbus::Error::NameTaken),
        }
        tracing::info!(name = BUS_NAME, "shell ready");
        match conn
            .request_name_with_flags(screensaver::BUS_NAME, RequestNameFlags::DoNotQueue.into())
            .await
        {
            Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => {
                if let Err(err) = auth.available() {
                    tracing::warn!(%err, "screen locking unavailable");
                }
            }
            other => tracing::warn!(?other, "cannot own org.gnome.ScreenSaver; screen locking unavailable"),
        }

        let mut changes = DBusProxy::new(&conn).await?.receive_name_owner_changed().await?;
        let watched = state.clone();
        let watcher_ui = ui_for_watcher;
        let watcher = tokio::spawn(async move {
            while let Some(signal) = changes.next().await {
                let Ok(args) = signal.args() else { continue };
                let name = args.name().as_str();
                if name.starts_with(':') && args.new_owner().is_none() {
                    let removed = watched
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove_owner(name);
                    if !removed.is_empty() {
                        tracing::debug!(name, ?removed, "released accelerators of departed client");
                    }
                    for action in removed {
                        let _ = watcher_ui.send(UiEvent::Unbind { action });
                    }
                }
            }
        });
        Ok((Self { conn, state, watcher, auth }, ui_rx))
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// The password checker the lock screen uses.
    pub fn authenticator(&self) -> Arc<dyn auth::Authenticator> {
        self.auth.clone()
    }

    /// Lock now (e.g. the compositor restarted us while locked).
    pub async fn lock(&self) -> zbus::Result<()> {
        lock_via(&self.conn).await
    }

    /// Accelerators currently grabbed, as (action, normalized accelerator).
    pub fn grabbed(&self) -> Vec<(u32, String)> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .grabs()
            .map(|(id, g)| (*id, g.accelerator.to_string()))
            .collect()
    }

    /// Lets the UI report back (overview changes, key presses).
    pub fn ui_bridge(&self) -> UiBridge {
        UiBridge {
            conn: self.conn.clone(),
        }
    }

    pub async fn shutdown(self) -> zbus::Result<()> {
        self.watcher.abort();
        self.conn.release_name(BUS_NAME).await?;
        Ok(())
    }
}

/// See [`ShellService::ui_bridge`].
#[derive(Clone)]
pub struct UiBridge {
    conn: Connection,
}

impl UiBridge {
    pub async fn handle(&self, request: UiRequest) -> zbus::Result<()> {
        let iface = self
            .conn
            .object_server()
            .interface::<_, Shell>(OBJECT_PATH)
            .await?;
        let shell = iface.get().await;
        let emitter = iface.signal_emitter();
        match request {
            UiRequest::Overview(active) => shell.set_overview_from_ui(active, emitter).await,
            UiRequest::Accelerator { action, pressed, time } => {
                shell.accelerator_event(action, pressed, time, emitter).await
            }
            UiRequest::GrabFailed { action } => {
                shell.grab_failed(action);
                Ok(())
            }
            UiRequest::Locked | UiRequest::Unlocked | UiRequest::LockGone => {
                let saver = self
                    .conn
                    .object_server()
                    .interface::<_, screensaver::ScreenSaver>(screensaver::OBJECT_PATH)
                    .await?;
                let locked = request == UiRequest::Locked;
                let object = saver.get().await;
                let result = object.set_locked(locked, saver.signal_emitter()).await;
                result
            }
        }
    }
}

/// Ask our own ScreenSaver object to lock.
pub async fn lock_via(conn: &Connection) -> zbus::Result<()> {
    let saver = conn
        .object_server()
        .interface::<_, screensaver::ScreenSaver>(screensaver::OBJECT_PATH)
        .await?;
    let object = saver.get().await;
    let result = object
        .request_lock()
        .map_err(|e| zbus::Error::Failure(format!("cannot lock: {e}")));
    result
}
