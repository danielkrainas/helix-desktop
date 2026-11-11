//! `org.gnome.SessionManager` D-Bus interface.
//!
//! Contract: `helix-desktop-spec/contracts/dbus/org.gnome.SessionManager.xml`
//! (value semantics in `org.gnome.SessionManager.md` next to it).
//!
//! This is a thin adapter: rules live in [`crate::state::SessionState`].

use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::mpsc;
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};
use zbus::{interface, DBusError};

use crate::state::{EndSessionAction, LogoutMode, OwnerCleanup, SessionError, SessionState};

/// Well-known bus name.
pub const BUS_NAME: &str = "org.gnome.SessionManager";
/// Object path of the manager.
pub const OBJECT_PATH: &str = "/org/gnome/SessionManager";

/// Static facts about the session, exposed as properties.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    /// `SessionName`.
    pub name: String,
    /// `SessionClass` (the logind session class, e.g. "user").
    pub class: String,
}

impl Default for SessionInfo {
    fn default() -> Self {
        Self {
            name: "helix".to_owned(),
            class: std::env::var("XDG_SESSION_CLASS")
                .ok()
                .filter(|c| !c.is_empty())
                .unwrap_or_else(|| "user".to_owned()),
        }
    }
}

/// Errors returned to D-Bus callers.
///
/// Error names are not part of the spec yet (see `gaps.md`); they are
/// namespaced under `org.gnome.SessionManager.Error`.
#[derive(Debug, DBusError)]
#[zbus(prefix = "org.gnome.SessionManager.Error")]
pub enum ManagerError {
    #[zbus(error)]
    ZBus(zbus::Error),
    NotInInitialization(String),
    AlreadyInitialized(String),
    NotInRunning(String),
    NotRegistered(String),
    InvalidCookie(String),
    InvalidArgs(String),
    Inhibited(String),
    General(String),
}

impl From<SessionError> for ManagerError {
    fn from(err: SessionError) -> Self {
        let msg = err.to_string();
        match err {
            SessionError::NotInInitialization => Self::NotInInitialization(msg),
            SessionError::AlreadyInitialized => Self::AlreadyInitialized(msg),
            SessionError::NotRunning => Self::NotInRunning(msg),
            SessionError::NotRegistered(_) => Self::NotRegistered(msg),
            SessionError::UnknownCookie(_) => Self::InvalidCookie(msg),
            SessionError::InvalidLogoutMode(_) => Self::InvalidArgs(msg),
            SessionError::Inhibited { .. } => Self::Inhibited(msg),
            SessionError::CookiesExhausted => Self::General(msg),
        }
    }
}

type Result<T> = std::result::Result<T, ManagerError>;

/// The exported `org.gnome.SessionManager` object.
pub struct SessionManager {
    state: Arc<Mutex<SessionState>>,
    end_tx: mpsc::UnboundedSender<EndSessionAction>,
    info: SessionInfo,
}

impl SessionManager {
    pub fn new(
        state: Arc<Mutex<SessionState>>,
        end_tx: mpsc::UnboundedSender<EndSessionAction>,
        info: SessionInfo,
    ) -> Self {
        Self { state, end_tx, info }
    }

    fn state(&self) -> MutexGuard<'_, SessionState> {
        // A poisoned lock means a panic mid-update; the state is still the
        // best information we have, so keep serving it.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    async fn end_session(
        &self,
        action: EndSessionAction,
        emitter: &SignalEmitter<'_>,
    ) -> Result<()> {
        self.state().request_end(action)?;
        self.announce_end(action, emitter).await
    }

    /// The state has already moved to `Ending`: tell listeners and the
    /// service loop.
    async fn announce_end(
        &self,
        action: EndSessionAction,
        emitter: &SignalEmitter<'_>,
    ) -> Result<()> {
        tracing::info!(?action, "ending session");
        Self::session_over(emitter).await?;
        // The receiver lives as long as the service; if it is gone the
        // process is already shutting down.
        let _ = self.end_tx.send(action);
        Ok(())
    }

    /// Leave the initialization phase (used by the launcher and by the
    /// `Initialized` method).
    pub async fn mark_initialized(&self, emitter: &SignalEmitter<'_>) -> zbus::Result<()> {
        self.state()
            .initialized()
            .map_err(|e| zbus::Error::Failure(e.to_string()))?;
        tracing::info!("session initialized; now running");
        Self::session_running(emitter).await
    }

    /// Release everything owned by a bus peer that left the bus, and announce
    /// the removals.
    pub async fn release_owner(&self, owner: &str, emitter: &SignalEmitter<'_>) -> zbus::Result<()> {
        let OwnerCleanup { clients, inhibitors } = self.state().remove_owner(owner);
        for c in &clients {
            tracing::info!(owner, app_id = %c.app_id, "client vanished from bus");
            Self::client_removed(emitter, path(&c.object_path())).await?;
        }
        for i in &inhibitors {
            tracing::info!(owner, cookie = i.cookie, "inhibitor owner vanished from bus");
            Self::inhibitor_removed(emitter, path(&i.object_path())).await?;
        }
        if !inhibitors.is_empty() {
            self.inhibited_actions_changed(emitter).await?;
        }
        Ok(())
    }
}

fn path(s: &str) -> ObjectPath<'_> {
    // Paths are built from fixed prefixes plus a number, so always valid.
    ObjectPath::try_from(s).expect("generated object path is valid")
}

fn sender(header: &Header<'_>) -> Option<String> {
    header.sender().map(|s| s.to_string())
}

#[interface(name = "org.gnome.SessionManager")]
impl SessionManager {
    // --- Initialization phase -------------------------------------------

    async fn setenv(&self, variable: &str, value: &str) -> Result<()> {
        self.state().setenv(variable, value)?;
        tracing::debug!(variable, "setenv");
        Ok(())
    }

    async fn get_locale(&self, category: i32) -> Result<String> {
        crate::locale::current(category)
            .ok_or_else(|| ManagerError::InvalidArgs(format!("unknown locale category {category}")))
    }

    async fn initialization_error(
        &self,
        message: &str,
        fatal: bool,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<()> {
        let must_end = self.state().initialization_error(fatal)?;
        if must_end {
            tracing::error!(message, "fatal session initialization error");
            self.announce_end(EndSessionAction::InitializationFailed, &emitter)
                .await?;
        } else {
            tracing::warn!(message, "session initialization error");
        }
        Ok(())
    }

    async fn initialized(&self, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) -> Result<()> {
        // Check first so callers get the specific error name.
        if self.state().phase() != crate::state::Phase::Initializing {
            return Err(SessionError::AlreadyInitialized.into());
        }
        self.mark_initialized(&emitter).await?;
        Ok(())
    }

    // --- Running phase --------------------------------------------------

    async fn register_client(
        &self,
        app_id: &str,
        _ignored: &str,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<OwnedObjectPath> {
        let client = self.state().register_client(app_id, sender(&header).as_deref());
        let p = client.object_path();
        tracing::info!(app_id, path = %p, "client registered");
        Self::client_added(&emitter, path(&p)).await?;
        Ok(OwnedObjectPath::from(path(&p).into_owned()))
    }

    async fn unregister_client(
        &self,
        client_id: ObjectPath<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<()> {
        let client = self.state().unregister_client(client_id.as_str())?;
        tracing::info!(app_id = %client.app_id, path = %client_id, "client unregistered");
        Self::client_removed(&emitter, client_id).await?;
        Ok(())
    }

    async fn inhibit(
        &self,
        app_id: &str,
        _ignored: u32,
        reason: &str,
        flags: u32,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<u32> {
        let inhibitor = self
            .state()
            .inhibit(app_id, reason, flags, sender(&header).as_deref())?;
        tracing::info!(app_id, reason, flags, cookie = inhibitor.cookie, "inhibit");
        Self::inhibitor_added(&emitter, path(&inhibitor.object_path())).await?;
        self.inhibited_actions_changed(&emitter).await?;
        Ok(inhibitor.cookie)
    }

    async fn uninhibit(
        &self,
        inhibit_cookie: u32,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<()> {
        let inhibitor = self.state().uninhibit(inhibit_cookie)?;
        tracing::info!(app_id = %inhibitor.app_id, cookie = inhibit_cookie, "uninhibit");
        Self::inhibitor_removed(&emitter, path(&inhibitor.object_path())).await?;
        self.inhibited_actions_changed(&emitter).await?;
        Ok(())
    }

    async fn is_inhibited(&self, flags: u32) -> bool {
        self.state().is_inhibited(flags)
    }

    async fn get_inhibitors(&self) -> Vec<OwnedObjectPath> {
        self.state()
            .inhibitors()
            .map(|i| OwnedObjectPath::from(path(&i.object_path()).into_owned()))
            .collect()
    }

    async fn shutdown(&self, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) -> Result<()> {
        self.end_session(EndSessionAction::Shutdown, &emitter).await
    }

    async fn reboot(&self, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) -> Result<()> {
        self.end_session(EndSessionAction::Reboot, &emitter).await
    }

    async fn logout(
        &self,
        mode: u32,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<()> {
        let mode = LogoutMode::try_from(mode)?;
        self.end_session(EndSessionAction::Logout(mode), &emitter).await
    }

    async fn is_session_running(&self) -> bool {
        self.state().is_running()
    }

    async fn can_reboot_to_firmware_setup(&self) -> bool {
        // Requires a system power backend, which Helix does not have yet.
        false
    }

    // --- Signals ----------------------------------------------------------

    #[zbus(signal)]
    pub async fn client_added(emitter: &SignalEmitter<'_>, id: ObjectPath<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn client_removed(emitter: &SignalEmitter<'_>, id: ObjectPath<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn inhibitor_added(emitter: &SignalEmitter<'_>, id: ObjectPath<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn inhibitor_removed(
        emitter: &SignalEmitter<'_>,
        id: ObjectPath<'_>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn session_running(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn session_over(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    // --- Properties -------------------------------------------------------

    #[zbus(property)]
    async fn session_name(&self) -> String {
        self.info.name.clone()
    }

    /// Deprecated in the contract; kept for introspection compatibility.
    #[zbus(property)]
    async fn renderer(&self) -> String {
        String::new()
    }

    #[zbus(property)]
    async fn session_class(&self) -> String {
        self.info.class.clone()
    }

    #[zbus(property)]
    async fn restore_supported(&self) -> bool {
        false
    }

    #[zbus(property)]
    async fn session_is_active(&self) -> bool {
        self.state().is_active()
    }

    #[zbus(property)]
    async fn session_is_locked(&self) -> bool {
        self.state().is_locked()
    }

    #[zbus(property)]
    async fn inhibited_actions(&self) -> u32 {
        self.state().inhibited_actions()
    }
}
