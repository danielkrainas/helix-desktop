//! Helix shell.
//!
//! - [`ShellService`] serves `org.gnome.Shell` (contract:
//!   `helix-desktop-spec/contracts/dbus/org.gnome.Shell.xml` and `.md`).
//! - Requests that need UI (OSD, overview) are published as [`UiEvent`]s for
//!   the shell UI.

pub mod accel;
pub mod font;
pub mod panel;
pub mod state;

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
        action
    }

    async fn grab_accelerators(
        &self,
        accelerators: Vec<(String, u32, u32)>,
        #[zbus(header)] header: Header<'_>,
    ) -> Vec<u32> {
        let owner = sender(&header);
        let mut state = self.state();
        accelerators
            .iter()
            .map(|(accel, modes, flags)| state.grab(accel, *modes, *flags, owner.as_deref()))
            .collect()
    }

    /// False if `action` was not grabbed.
    async fn ungrab_accelerator(&self, action: u32) -> bool {
        self.state().ungrab(action)
    }

    /// Releases every listed grab; false if any of them was not grabbed.
    async fn ungrab_accelerators(&self, actions: Vec<u32>) -> bool {
        let mut state = self.state();
        let mut all_found = true;
        for action in actions {
            // No short-circuit: release the rest even after a miss.
            all_found &= state.ungrab(action);
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
}

impl ShellService {
    /// Serve `org.gnome.Shell`; UI requests arrive on the returned receiver.
    pub async fn start(
        builder: connection::Builder<'_>,
    ) -> zbus::Result<(Self, mpsc::UnboundedReceiver<UiEvent>)> {
        let state = Arc::new(Mutex::new(ShellState::new()));
        let (ui, ui_rx) = mpsc::unbounded_channel();
        let conn = builder
            .serve_at(OBJECT_PATH, Shell { state: state.clone(), ui })?
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

        let mut changes = DBusProxy::new(&conn).await?.receive_name_owner_changed().await?;
        let watched = state.clone();
        let watcher = tokio::spawn(async move {
            while let Some(signal) = changes.next().await {
                let Ok(args) = signal.args() else { continue };
                let name = args.name().as_str();
                if name.starts_with(':') && args.new_owner().is_none() {
                    let removed = watched
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove_owner(name);
                    if removed > 0 {
                        tracing::debug!(name, removed, "released accelerators of departed client");
                    }
                }
            }
        });
        Ok((Self { conn, state, watcher }, ui_rx))
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
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

    pub async fn shutdown(self) -> zbus::Result<()> {
        self.watcher.abort();
        self.conn.release_name(BUS_NAME).await?;
        Ok(())
    }
}
