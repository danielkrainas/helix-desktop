//! `org.gnome.ScreenSaver`: lock state and locking.
//!
//! Contract: `helix-desktop-spec/contracts/dbus/org.gnome.ScreenSaver.xml`
//! and `.md`. The lock itself is drawn by the shell UI on
//! `ext-session-lock-v1`; this object tracks state and asks the UI to lock.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use tokio::sync::mpsc;
use zbus::object_server::SignalEmitter;
use zbus::{fdo, interface};

use crate::auth::Authenticator;
use crate::UiEvent;

pub const BUS_NAME: &str = "org.gnome.ScreenSaver";
pub const OBJECT_PATH: &str = "/org/gnome/ScreenSaver";

/// Lock state shared by D-Bus and the idle watcher.
#[derive(Debug, Default)]
pub struct LockStatus {
    /// Locked, per the compositor's confirmation.
    pub locked_since: Option<Instant>,
    /// A lock was requested and is not confirmed yet.
    pub requested: bool,
}

impl LockStatus {
    pub fn locked(&self) -> bool {
        self.locked_since.is_some()
    }
}

pub type SharedStatus = Arc<Mutex<LockStatus>>;

pub fn status(shared: &SharedStatus) -> MutexGuard<'_, LockStatus> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct ScreenSaver {
    pub status: SharedStatus,
    pub ui: mpsc::UnboundedSender<UiEvent>,
    pub auth: Arc<dyn Authenticator>,
}

impl ScreenSaver {
    /// Ask the UI to lock, unless already locked or unlocking can't work.
    pub fn request_lock(&self) -> Result<(), String> {
        let mut status = status(&self.status);
        if status.locked() || status.requested {
            return Ok(());
        }
        self.auth.available()?;
        status.requested = true;
        let _ = self.ui.send(UiEvent::Lock);
        Ok(())
    }

    /// The UI reports the lock is in place (`true`), or gone / refused.
    pub async fn set_locked(&self, locked: bool, emitter: &SignalEmitter<'_>) -> zbus::Result<()> {
        let changed = {
            let mut status = status(&self.status);
            status.requested = false;
            let was = status.locked();
            status.locked_since = locked.then(|| status.locked_since.unwrap_or_else(Instant::now));
            was != locked
        };
        if changed {
            tracing::info!(locked, "screen lock changed");
            Self::active_changed(emitter, locked).await?;
        }
        Ok(())
    }
}

#[interface(name = "org.gnome.ScreenSaver")]
impl ScreenSaver {
    async fn lock(&self) -> fdo::Result<()> {
        self.request_lock().map_err(|e| {
            tracing::error!(err = %e, "refusing to lock");
            fdo::Error::Failed(format!("cannot lock: {e}"))
        })
    }

    async fn get_active(&self) -> bool {
        status(&self.status).locked()
    }

    async fn set_active(&self, value: bool) -> fdo::Result<()> {
        // Activating locks; deactivating never unlocks (only authentication).
        if value {
            self.lock().await?;
        }
        Ok(())
    }

    async fn get_active_time(&self) -> u32 {
        status(&self.status)
            .locked_since
            .map(|t| t.elapsed().as_secs() as u32)
            .unwrap_or(0)
    }

    #[zbus(signal)]
    pub async fn active_changed(emitter: &SignalEmitter<'_>, new_value: bool) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn wake_up_screen(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AuthError, Secret};

    struct Fake(bool);
    impl Authenticator for Fake {
        fn available(&self) -> Result<(), String> {
            if self.0 { Ok(()) } else { Err("no PAM service".into()) }
        }
        fn authenticate(&self, _: &str, _: &Secret) -> Result<(), AuthError> {
            Err(AuthError::Denied)
        }
    }

    fn saver(available: bool) -> (ScreenSaver, mpsc::UnboundedReceiver<UiEvent>) {
        let (ui, rx) = mpsc::unbounded_channel();
        (
            ScreenSaver { status: Default::default(), ui, auth: Arc::new(Fake(available)) },
            rx,
        )
    }

    #[test]
    fn lock_request_goes_to_ui_once() {
        let (s, mut rx) = saver(true);
        s.request_lock().unwrap();
        s.request_lock().unwrap();
        assert_eq!(rx.try_recv().ok(), Some(UiEvent::Lock));
        assert!(rx.try_recv().is_err(), "no duplicate request while pending");
    }

    #[test]
    fn refuses_without_authentication() {
        let (s, mut rx) = saver(false);
        assert!(s.request_lock().is_err());
        assert!(rx.try_recv().is_err());
        assert!(!status(&s.status).requested);
    }
}
