//! `org.gnome.SessionManager.EndSessionDialog`: the shell's confirmation
//! dialog for logging out, restarting and powering off, and for installing
//! updates while restarting.
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
/// Restart and install the prepared updates.
pub const UPDATE_REBOOT: u32 = 3;
/// Restart and install the prepared system upgrade.
pub const UPGRADE_REBOOT: u32 = 4;

/// What the UI shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndDialog {
    pub kind: u32,
    pub seconds: u32,
    /// Blocking apps as (name, reason).
    pub blockers: Vec<(String, String)>,
    /// "Install pending software updates" (power off, restart): shown
    /// when updates are ready, and whether it is ticked.
    pub install: Option<bool>,
    /// The system upgrade's name (`UPGRADE_REBOOT`).
    pub upgrade: Option<String>,
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
    /// Updates were already set to install at the next boot when it opened.
    pub update_triggered: Arc<Mutex<bool>>,
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

/// The apps holding up logout now: (name, reason), from the session
/// manager's inhibitors with the logout flag (1).
pub async fn logout_blockers(conn: &Connection) -> Vec<(String, String)> {
    let Ok(reply) = conn.call_method(Some("org.gnome.SessionManager"), "/org/gnome/SessionManager", Some("org.gnome.SessionManager"), "GetInhibitors", &()).await else {
        return Vec::new();
    };
    let Ok(paths) = reply.body().deserialize::<Vec<OwnedObjectPath>>() else { return Vec::new() };
    let mut out = Vec::new();
    for path in &paths {
        let flags = conn
            .call_method(Some("org.gnome.SessionManager"), path.as_str(), Some("org.gnome.SessionManager.Inhibitor"), "GetFlags", &())
            .await
            .ok()
            .and_then(|r| r.body().deserialize::<u32>().ok())
            .unwrap_or(0);
        if flags & 1 != 0 {
            if let Some(b) = describe(conn, path).await {
                out.push(b);
            }
        }
    }
    out
}

impl EndSessionDialog {
    /// Report the user's answer (`install`: the updates check box): the
    /// matching signal, then `Closed`. Updates are scheduled first; when
    /// that fails the answer is `Canceled` and the error is returned.
    pub async fn answer(&self, choice: Choice, install: Option<bool>, emitter: &SignalEmitter<'_>) -> zbus::Result<Option<String>> {
        let Some(kind) = self.open_kind.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return Ok(None);
        };
        let _ = self.ui.send(UiEvent::EndDialog(None));
        let triggered = *self.update_triggered.lock().unwrap_or_else(|e| e.into_inner());
        let mut failed = None;
        if choice == Choice::Confirmed {
            if let Some(call) = crate::offline_updates::call_for(kind, install, triggered) {
                if let Err(err) = crate::offline_updates::make(call).await {
                    tracing::warn!(%err, "cannot schedule the updates");
                    failed = Some(err);
                }
            }
        }
        match (choice, kind) {
            _ if failed.is_some() => Self::canceled(emitter).await?,
            (Choice::Canceled, _) => Self::canceled(emitter).await?,
            (Choice::Confirmed, SHUTDOWN) => Self::confirmed_shutdown(emitter).await?,
            (Choice::Confirmed, REBOOT | UPDATE_REBOOT | UPGRADE_REBOOT) => Self::confirmed_reboot(emitter).await?,
            (Choice::Confirmed, _) => Self::confirmed_logout(emitter).await?,
        }
        Self::closed(emitter).await?;
        Ok(failed)
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
        let prepared = if matches!(r#type, SHUTDOWN | REBOOT | UPDATE_REBOOT | UPGRADE_REBOOT) {
            crate::offline_updates::prepared().await
        } else {
            Default::default()
        };
        // Nothing to install: a plain restart.
        let kind = match r#type {
            UPDATE_REBOOT if !prepared.update => REBOOT,
            UPGRADE_REBOOT if prepared.upgrade.is_none() => REBOOT,
            kind => kind,
        };
        let install = (matches!(kind, SHUTDOWN | REBOOT) && prepared.update).then_some(true);
        let upgrade = if kind == UPGRADE_REBOOT { prepared.upgrade.clone() } else { None };
        tracing::info!(kind, asked = r#type, blockers = blockers.len(), ?install, "end-session dialog");
        *self.open_kind.lock().unwrap_or_else(|e| e.into_inner()) = Some(kind);
        *self.update_triggered.lock().unwrap_or_else(|e| e.into_inner()) = prepared.update_triggered;
        let _ = self.ui.send(UiEvent::EndDialog(Some(EndDialog {
            kind,
            seconds: seconds_to_stay_open,
            blockers,
            install,
            upgrade,
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
