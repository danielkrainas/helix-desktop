//! `org.gnome.SessionManager.EndSessionDialog`: the shell's confirmation
//! dialog for logging out, restarting and powering off.
//!
//! Contract: helix-desktop-spec `org.gnome.SessionManager.EndSessionDialog.xml`
//! and `.md`. The session manager calls `Open`; the user's answer goes back
//! as a signal.

use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::OwnedObjectPath;
use zbus::{interface, Connection};

use crate::UiEvent;

pub const OBJECT_PATH: &str = "/org/gnome/SessionManager/EndSessionDialog";

/// `Open` types.
pub const LOGOUT: u32 = 0;
pub const SHUTDOWN: u32 = 1;
pub const REBOOT: u32 = 2;

/// What the UI shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndDialog {
    pub kind: u32,
    pub seconds: u32,
    /// Blocking apps as (name, reason).
    pub blockers: Vec<(String, String)>,
}

/// The user's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Confirmed,
    Canceled,
}

/// `Open` types.
pub const KIND_POWER_OFF: u32 = 1;
pub const KIND_RESTART: u32 = 2;

/// An end of session already confirmed (on the lock screen), by type.
static PRECONFIRMED: Mutex<Option<u32>> = Mutex::new(None);

pub fn preconfirm(kind: u32) {
    *PRECONFIRMED.lock().unwrap_or_else(|e| e.into_inner()) = Some(kind);
}

pub fn preconfirm_clear() {
    PRECONFIRMED.lock().unwrap_or_else(|e| e.into_inner()).take();
}

pub struct EndSessionDialog {
    pub ui: mpsc::UnboundedSender<UiEvent>,
    /// Type of the open dialog, if any.
    pub open_kind: Arc<Mutex<Option<u32>>>,
}

/// A human name for an app id (its desktop file's name if installed).
fn app_name(app_id: &str) -> String {
    use gio::prelude::*;
    let wanted = format!("{}.desktop", app_id.trim_end_matches(".desktop"));
    gio::AppInfo::all()
        .into_iter()
        .find(|a| a.id().is_some_and(|id| id == wanted))
        .map(|a| a.display_name().to_string())
        .unwrap_or_else(|| app_id.to_owned())
}

async fn describe(conn: &Connection, path: &OwnedObjectPath) -> Option<(String, String)> {
    let get = |member: &'static str| async move {
        conn.call_method(
            Some("org.gnome.SessionManager"),
            path.as_str(),
            Some("org.gnome.SessionManager.Inhibitor"),
            member,
            &(),
        )
        .await
        .ok()?
        .body()
        .deserialize::<String>()
        .ok()
    };
    let app_id = get("GetAppId").await?;
    let reason = get("GetReason").await.unwrap_or_default();
    Some((app_name(&app_id), reason))
}

impl EndSessionDialog {
    /// Report the user's answer: the matching signal, then `Closed`.
    pub async fn answer(&self, choice: Choice, emitter: &SignalEmitter<'_>) -> zbus::Result<()> {
        let Some(kind) = self.open_kind.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return Ok(());
        };
        let _ = self.ui.send(UiEvent::EndDialog(None));
        match (choice, kind) {
            (Choice::Canceled, _) => Self::canceled(emitter).await?,
            (Choice::Confirmed, SHUTDOWN) => Self::confirmed_shutdown(emitter).await?,
            (Choice::Confirmed, REBOOT) => Self::confirmed_reboot(emitter).await?,
            (Choice::Confirmed, _) => Self::confirmed_logout(emitter).await?,
        }
        Self::closed(emitter).await
    }
}

#[interface(name = "org.gnome.SessionManager.EndSessionDialog")]
impl EndSessionDialog {
    async fn open(
        &self,
        r#type: u32,
        _timestamp: u32,
        seconds_to_stay_open: u32,
        inhibitor_object_paths: Vec<OwnedObjectPath>,
        #[zbus(connection)] conn: &Connection,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) {
        let preconfirmed = PRECONFIRMED.lock().unwrap_or_else(|e| e.into_inner()).take();
        if preconfirmed == Some(r#type) {
            tracing::info!(kind = r#type, "end of session confirmed on the lock screen");
            let sent = match r#type {
                KIND_RESTART => Self::confirmed_reboot(&emitter).await,
                _ => Self::confirmed_shutdown(&emitter).await,
            };
            if let Err(err) = sent {
                tracing::warn!(%err, "cannot confirm the end of the session");
            }
            return;
        }
        let mut blockers = Vec::new();
        for path in &inhibitor_object_paths {
            if let Some(b) = describe(conn, path).await {
                blockers.push(b);
            }
        }
        tracing::info!(kind = r#type, blockers = blockers.len(), "end-session dialog");
        *self.open_kind.lock().unwrap_or_else(|e| e.into_inner()) = Some(r#type);
        let _ = self.ui.send(UiEvent::EndDialog(Some(EndDialog {
            kind: r#type,
            seconds: seconds_to_stay_open,
            blockers,
        })));
    }

    async fn close(&self, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) -> zbus::fdo::Result<()> {
        if self.open_kind.lock().unwrap_or_else(|e| e.into_inner()).take().is_some() {
            let _ = self.ui.send(UiEvent::EndDialog(None));
            Self::closed(&emitter).await?;
        }
        Ok(())
    }

    #[zbus(signal)]
    pub async fn confirmed_logout(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
    #[zbus(signal)]
    pub async fn confirmed_reboot(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
    #[zbus(signal)]
    pub async fn confirmed_shutdown(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
    #[zbus(signal)]
    pub async fn canceled(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
    #[zbus(signal)]
    pub async fn closed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}
