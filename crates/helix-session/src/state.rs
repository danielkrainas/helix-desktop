//! Transport-independent session state.
//!
//! Everything in this module is plain Rust with no D-Bus dependency, so the
//! session lifecycle rules can be unit-tested directly. The D-Bus layer in
//! [`crate::manager`] is a thin adapter over [`SessionState`].

use std::collections::BTreeMap;
use std::fmt;

/// Object path prefix for registered clients.
///
/// The contract does not define client object paths; this follows the
/// `/org/gnome/SessionManager/<Kind><n>` shape clients already expect.
pub const CLIENT_PATH_PREFIX: &str = "/org/gnome/SessionManager/Client";

/// Object path prefix for inhibitors (returned by `GetInhibitors`).
pub const INHIBITOR_PATH_PREFIX: &str = "/org/gnome/SessionManager/Inhibitor";

/// Inhibit flag bits, as documented in
/// `helix-desktop-spec/contracts/dbus/org.gnome.SessionManager.md`.
pub mod inhibit_flags {
    /// Logout, shutdown and reboot.
    pub const LOGOUT: u32 = 1;
    pub const SWITCH_USER: u32 = 2;
    pub const SUSPEND: u32 = 4;
    /// The session being marked idle.
    pub const IDLE: u32 = 8;
    pub const AUTOMOUNT: u32 = 16;
}

/// Lifecycle phase of the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Before `Initialized` has been called. `Setenv` is only accepted here.
    Initializing,
    /// After `Initialized`; the session is up.
    Running,
    /// An end-of-session request has been accepted; the service is exiting.
    Ending,
}

/// `Logout` mode, as documented in
/// `helix-desktop-spec/contracts/dbus/org.gnome.SessionManager.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogoutMode {
    /// 0 — show a confirmation if needed.
    Normal,
    /// 1 — no confirmation.
    NoConfirmation,
    /// 2 — force.
    Force,
}

impl TryFrom<u32> for LogoutMode {
    type Error = SessionError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Normal),
            1 => Ok(Self::NoConfirmation),
            2 => Ok(Self::Force),
            other => Err(SessionError::InvalidLogoutMode(other)),
        }
    }
}

/// Why the session is ending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndSessionAction {
    Logout(LogoutMode),
    Shutdown,
    Reboot,
    /// A component reported a fatal `InitializationError`.
    InitializationFailed,
}

/// Rule violations reported back to D-Bus callers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    /// `Setenv` after the initialization phase.
    NotInInitialization,
    /// `Initialized` called when not initializing.
    AlreadyInitialized,
    /// Ending the session requires the running phase.
    NotRunning,
    /// Unknown client object path.
    NotRegistered(String),
    /// Unknown inhibit cookie.
    UnknownCookie(u32),
    /// `Logout` mode outside 0..=2.
    InvalidLogoutMode(u32),
    /// End-of-session refused because inhibitors are active.
    Inhibited { count: usize },
    /// Every cookie value is in use (practically unreachable).
    CookiesExhausted,
    /// An end-of-session sequence is already under way.
    AlreadyEnding,
}

impl EndSessionAction {
    /// Whether active logout inhibitors can block this action.
    pub fn respects_inhibitors(self) -> bool {
        !matches!(
            self,
            Self::Logout(LogoutMode::Force) | Self::InitializationFailed
        )
    }
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInInitialization => {
                write!(f, "only allowed during the initialization phase")
            }
            Self::AlreadyInitialized => write!(f, "session is already initialized"),
            Self::NotRunning => write!(f, "session is not in the running phase"),
            Self::NotRegistered(path) => write!(f, "client {path} is not registered"),
            Self::UnknownCookie(c) => write!(f, "no inhibitor with cookie {c}"),
            Self::InvalidLogoutMode(m) => {
                write!(f, "invalid logout mode {m} (expected 0, 1 or 2)")
            }
            Self::Inhibited { count } => {
                write!(f, "session end is inhibited by {count} active inhibitor(s)")
            }
            Self::CookiesExhausted => write!(f, "no free inhibit cookies"),
            Self::AlreadyEnding => write!(f, "the session is already ending"),
        }
    }
}

impl std::error::Error for SessionError {}

/// A client registered through `RegisterClient`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Client {
    pub id: u32,
    pub app_id: String,
    /// Unique bus name of the caller that registered it, if known.
    pub owner: Option<String>,
}

impl Client {
    pub fn object_path(&self) -> String {
        format!("{CLIENT_PATH_PREFIX}{}", self.id)
    }
}

/// An inhibitor created through `Inhibit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inhibitor {
    pub cookie: u32,
    pub app_id: String,
    pub reason: String,
    pub flags: u32,
    /// Unique bus name of the caller that created it, if known.
    pub owner: Option<String>,
}

impl Inhibitor {
    pub fn object_path(&self) -> String {
        format!("{INHIBITOR_PATH_PREFIX}{}", self.cookie)
    }
}

/// What was removed when a bus peer disappeared.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct OwnerCleanup {
    pub clients: Vec<Client>,
    pub inhibitors: Vec<Inhibitor>,
}

impl OwnerCleanup {
    pub fn is_empty(&self) -> bool {
        self.clients.is_empty() && self.inhibitors.is_empty()
    }
}

/// The session manager's state.
#[derive(Debug)]
pub struct SessionState {
    phase: Phase,
    env: BTreeMap<String, String>,
    clients: BTreeMap<u32, Client>,
    next_client_id: u32,
    inhibitors: BTreeMap<u32, Inhibitor>,
    next_cookie: u32,
    active: bool,
    locked: bool,
    /// An end-of-session sequence (confirm/query/end) is running.
    ending: bool,
}

impl Default for SessionState {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionState {
    pub fn new() -> Self {
        Self {
            phase: Phase::Initializing,
            env: BTreeMap::new(),
            clients: BTreeMap::new(),
            next_client_id: 1,
            inhibitors: BTreeMap::new(),
            next_cookie: 1,
            // No logind / screensaver integration yet: the session is
            // considered active and unlocked.
            active: true,
            locked: false,
            ending: false,
        }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn is_running(&self) -> bool {
        self.phase == Phase::Running
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn is_locked(&self) -> bool {
        self.locked
    }

    pub fn set_active(&mut self, active: bool) {
        self.active = active;
    }

    pub fn set_locked(&mut self, locked: bool) {
        self.locked = locked;
    }

    /// Environment collected via `Setenv`, to be applied to session children.
    pub fn environment(&self) -> &BTreeMap<String, String> {
        &self.env
    }

    pub fn setenv(&mut self, variable: &str, value: &str) -> Result<(), SessionError> {
        if self.phase != Phase::Initializing {
            return Err(SessionError::NotInInitialization);
        }
        self.env.insert(variable.to_owned(), value.to_owned());
        Ok(())
    }

    /// Move from initializing to running.
    pub fn initialized(&mut self) -> Result<(), SessionError> {
        if self.phase != Phase::Initializing {
            return Err(SessionError::AlreadyInitialized);
        }
        self.phase = Phase::Running;
        Ok(())
    }

    pub fn register_client(&mut self, app_id: &str, owner: Option<&str>) -> Client {
        let id = self.next_client_id;
        self.next_client_id = self.next_client_id.wrapping_add(1).max(1);
        let client = Client {
            id,
            app_id: app_id.to_owned(),
            owner: owner.map(str::to_owned),
        };
        self.clients.insert(id, client.clone());
        client
    }

    pub fn unregister_client(&mut self, path: &str) -> Result<Client, SessionError> {
        let id = path
            .strip_prefix(CLIENT_PATH_PREFIX)
            .and_then(|n| n.parse::<u32>().ok())
            .ok_or_else(|| SessionError::NotRegistered(path.to_owned()))?;
        self.clients
            .remove(&id)
            .ok_or_else(|| SessionError::NotRegistered(path.to_owned()))
    }

    pub fn clients(&self) -> impl Iterator<Item = &Client> {
        self.clients.values()
    }

    pub fn inhibit(
        &mut self,
        app_id: &str,
        reason: &str,
        flags: u32,
        owner: Option<&str>,
    ) -> Result<Inhibitor, SessionError> {
        let cookie = self.allocate_cookie()?;
        let inhibitor = Inhibitor {
            cookie,
            app_id: app_id.to_owned(),
            reason: reason.to_owned(),
            flags,
            owner: owner.map(str::to_owned),
        };
        self.inhibitors.insert(cookie, inhibitor.clone());
        Ok(inhibitor)
    }

    pub fn uninhibit(&mut self, cookie: u32) -> Result<Inhibitor, SessionError> {
        self.inhibitors
            .remove(&cookie)
            .ok_or(SessionError::UnknownCookie(cookie))
    }

    /// True if any active inhibitor shares a bit with `flags`.
    pub fn is_inhibited(&self, flags: u32) -> bool {
        self.inhibitors.values().any(|i| i.flags & flags != 0)
    }

    /// Bitwise OR of all active inhibitors' flags (`InhibitedActions`).
    pub fn inhibited_actions(&self) -> u32 {
        self.inhibitors.values().fold(0, |acc, i| acc | i.flags)
    }

    pub fn inhibitors(&self) -> impl Iterator<Item = &Inhibitor> {
        self.inhibitors.values()
    }

    /// Accept or refuse a request to end the session.
    ///
    /// Inhibitors with the logout bit block every action except a forced
    /// logout. There is no confirmation UI yet, so "normal" and "no
    /// confirmation" behave the same. On success the session moves to
    /// [`Phase::Ending`].
    pub fn request_end(&mut self, action: EndSessionAction) -> Result<(), SessionError> {
        if self.phase != Phase::Running {
            return Err(SessionError::NotRunning);
        }
        if action.respects_inhibitors() {
            let count = self
                .inhibitors
                .values()
                .filter(|i| i.flags & inhibit_flags::LOGOUT != 0)
                .count();
            if count > 0 {
                return Err(SessionError::Inhibited { count });
            }
        }
        self.phase = Phase::Ending;
        Ok(())
    }

    /// Start an end-of-session sequence (it may still be cancelled).
    pub fn begin_end(&mut self) -> Result<(), SessionError> {
        if self.phase != Phase::Running {
            return Err(SessionError::NotRunning);
        }
        if self.ending {
            return Err(SessionError::AlreadyEnding);
        }
        self.ending = true;
        Ok(())
    }

    /// The sequence was cancelled; the session keeps running.
    pub fn cancel_end(&mut self) {
        self.ending = false;
    }

    /// The sequence completed: the session is over.
    pub fn commit_end(&mut self) {
        self.phase = Phase::Ending;
    }

    /// Inhibitors that block ending the session (logout flag).
    pub fn blocking_inhibitors(&self) -> Vec<Inhibitor> {
        self.inhibitors
            .values()
            .filter(|i| i.flags & inhibit_flags::LOGOUT != 0)
            .cloned()
            .collect()
    }

    /// The client registered by a bus peer, if any.
    pub fn client_of(&self, owner: Option<&str>) -> Option<&Client> {
        let owner = owner?;
        self.clients.values().find(|c| c.owner.as_deref() == Some(owner))
    }

    /// A session component failed to start (`InitializationError`).
    ///
    /// Returns `true` if the session must now end (`fatal`), in which case it
    /// has moved to [`Phase::Ending`].
    pub fn initialization_error(&mut self, fatal: bool) -> Result<bool, SessionError> {
        if self.phase != Phase::Initializing {
            return Err(SessionError::NotInInitialization);
        }
        if fatal {
            self.phase = Phase::Ending;
        }
        Ok(fatal)
    }

    /// Move to [`Phase::Ending`] unconditionally (external termination such
    /// as SIGTERM, which inhibitors cannot block). Returns the previous phase.
    pub fn force_end(&mut self) -> Phase {
        std::mem::replace(&mut self.phase, Phase::Ending)
    }

    /// Drop every client and inhibitor owned by a bus name that disappeared.
    pub fn remove_owner(&mut self, owner: &str) -> OwnerCleanup {
        let owned = |o: &Option<String>| o.as_deref() == Some(owner);
        let mut cleanup = OwnerCleanup::default();
        self.clients.retain(|_, c| {
            if owned(&c.owner) {
                cleanup.clients.push(c.clone());
                false
            } else {
                true
            }
        });
        self.inhibitors.retain(|_, i| {
            if owned(&i.owner) {
                cleanup.inhibitors.push(i.clone());
                false
            } else {
                true
            }
        });
        cleanup
    }

    /// Next unused, non-zero cookie. Zero is never handed out so callers can
    /// keep using it as a "no inhibitor" sentinel.
    fn allocate_cookie(&mut self) -> Result<u32, SessionError> {
        if self.inhibitors.len() >= (u32::MAX - 1) as usize {
            return Err(SessionError::CookiesExhausted);
        }
        loop {
            let candidate = self.next_cookie;
            self.next_cookie = self.next_cookie.wrapping_add(1).max(1);
            if candidate != 0 && !self.inhibitors.contains_key(&candidate) {
                return Ok(candidate);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running() -> SessionState {
        let mut s = SessionState::new();
        s.initialized().unwrap();
        s
    }

    #[test]
    fn inhibit_cookie_lifecycle() {
        let mut s = running();
        assert!(!s.is_inhibited(u32::MAX));
        assert_eq!(s.inhibited_actions(), 0);

        let a = s.inhibit("org.example.A", "saving", 0b0001, Some(":1.10")).unwrap();
        let b = s.inhibit("org.example.B", "playing", 0b1000, Some(":1.11")).unwrap();
        assert_ne!(a.cookie, 0);
        assert_ne!(b.cookie, 0);
        assert_ne!(a.cookie, b.cookie);

        assert!(s.is_inhibited(0b0001));
        assert!(s.is_inhibited(0b1000));
        assert!(s.is_inhibited(0b1001));
        assert!(!s.is_inhibited(0b0100));
        assert_eq!(s.inhibited_actions(), 0b1001);

        assert_eq!(s.uninhibit(a.cookie).unwrap().app_id, "org.example.A");
        assert!(!s.is_inhibited(0b0001));
        assert_eq!(s.inhibited_actions(), 0b1000);

        // A cookie can only be released once.
        assert_eq!(
            s.uninhibit(a.cookie),
            Err(SessionError::UnknownCookie(a.cookie))
        );

        s.uninhibit(b.cookie).unwrap();
        assert_eq!(s.inhibited_actions(), 0);
        assert_eq!(s.inhibitors().count(), 0);
    }

    #[test]
    fn cookies_are_not_reused_while_live_and_skip_zero() {
        let mut s = running();
        s.next_cookie = u32::MAX;
        let a = s.inhibit("a", "", 1, None).unwrap();
        assert_eq!(a.cookie, u32::MAX);
        // Wraps past zero.
        let b = s.inhibit("b", "", 1, None).unwrap();
        assert_eq!(b.cookie, 1);
        // Wraps again onto a live cookie: it must be skipped.
        s.next_cookie = u32::MAX;
        let c = s.inhibit("c", "", 1, None).unwrap();
        assert_eq!(c.cookie, 2);
    }

    #[test]
    fn client_registration() {
        let mut s = running();
        let a = s.register_client("org.example.A", Some(":1.5"));
        let b = s.register_client("org.example.B", Some(":1.6"));
        assert_eq!(a.object_path(), "/org/gnome/SessionManager/Client1");
        assert_eq!(b.object_path(), "/org/gnome/SessionManager/Client2");
        assert_eq!(s.clients().count(), 2);

        let removed = s.unregister_client(&a.object_path()).unwrap();
        assert_eq!(removed.app_id, "org.example.A");
        assert_eq!(s.clients().count(), 1);

        assert!(matches!(
            s.unregister_client(&a.object_path()),
            Err(SessionError::NotRegistered(_))
        ));
        assert!(matches!(
            s.unregister_client("/org/gnome/SessionManager/Inhibitor2"),
            Err(SessionError::NotRegistered(_))
        ));
    }

    #[test]
    fn vanished_owner_releases_its_clients_and_inhibitors() {
        let mut s = running();
        s.register_client("gone", Some(":1.7"));
        let kept = s.register_client("kept", Some(":1.8"));
        s.inhibit("gone", "", 1, Some(":1.7")).unwrap();
        let kept_inh = s.inhibit("kept", "", 2, Some(":1.8")).unwrap();

        let cleanup = s.remove_owner(":1.7");
        assert_eq!(cleanup.clients.len(), 1);
        assert_eq!(cleanup.inhibitors.len(), 1);
        assert_eq!(s.clients().cloned().collect::<Vec<_>>(), vec![kept]);
        assert_eq!(s.inhibitors().cloned().collect::<Vec<_>>(), vec![kept_inh]);
        assert!(s.remove_owner(":1.7").is_empty());
    }

    #[test]
    fn setenv_only_during_initialization() {
        let mut s = SessionState::new();
        assert!(!s.is_running());
        s.setenv("FOO", "bar").unwrap();
        assert_eq!(s.environment().get("FOO").map(String::as_str), Some("bar"));
        s.initialized().unwrap();
        assert!(s.is_running());
        assert_eq!(s.setenv("FOO", "baz"), Err(SessionError::NotInInitialization));
        assert_eq!(s.initialized(), Err(SessionError::AlreadyInitialized));
    }

    #[test]
    fn logout_modes() {
        assert_eq!(LogoutMode::try_from(0), Ok(LogoutMode::Normal));
        assert_eq!(LogoutMode::try_from(1), Ok(LogoutMode::NoConfirmation));
        assert_eq!(LogoutMode::try_from(2), Ok(LogoutMode::Force));
        assert_eq!(
            LogoutMode::try_from(3),
            Err(SessionError::InvalidLogoutMode(3))
        );
    }

    #[test]
    fn end_session_refused_while_logout_inhibited() {
        let mut s = SessionState::new();
        assert_eq!(s.request_end(EndSessionAction::Shutdown), Err(SessionError::NotRunning));
        s.initialized().unwrap();

        let i = s
            .inhibit("a", "busy", inhibit_flags::LOGOUT | inhibit_flags::IDLE, None)
            .unwrap();
        for action in [
            EndSessionAction::Logout(LogoutMode::Normal),
            EndSessionAction::Logout(LogoutMode::NoConfirmation),
            EndSessionAction::Shutdown,
            EndSessionAction::Reboot,
        ] {
            assert_eq!(s.request_end(action), Err(SessionError::Inhibited { count: 1 }));
            assert!(s.is_running());
        }

        s.uninhibit(i.cookie).unwrap();
        s.request_end(EndSessionAction::Logout(LogoutMode::Normal)).unwrap();
        assert_eq!(s.phase(), Phase::Ending);
        assert!(!s.is_running());
        assert_eq!(s.request_end(EndSessionAction::Reboot), Err(SessionError::NotRunning));
    }

    #[test]
    fn non_logout_inhibitors_do_not_block_ending() {
        let mut s = running();
        s.inhibit("player", "video", inhibit_flags::IDLE | inhibit_flags::SUSPEND, None)
            .unwrap();
        s.request_end(EndSessionAction::Shutdown).unwrap();
        assert_eq!(s.phase(), Phase::Ending);
    }

    #[test]
    fn force_logout_ignores_inhibitors() {
        let mut s = running();
        s.inhibit("editor", "unsaved", inhibit_flags::LOGOUT, None).unwrap();
        s.request_end(EndSessionAction::Logout(LogoutMode::Force)).unwrap();
        assert_eq!(s.phase(), Phase::Ending);
    }

    #[test]
    fn end_sequence_bookkeeping() {
        let mut s = SessionState::new();
        assert_eq!(s.begin_end(), Err(SessionError::NotRunning));
        s.initialized().unwrap();
        s.begin_end().unwrap();
        assert_eq!(s.begin_end(), Err(SessionError::AlreadyEnding), "one at a time");
        s.cancel_end();
        assert!(s.is_running(), "cancelled: still running");
        s.begin_end().unwrap();
        s.commit_end();
        assert_eq!(s.phase(), Phase::Ending);

        let mut s = running();
        s.inhibit("idle", "", inhibit_flags::IDLE, None).unwrap();
        let blocker = s.inhibit("editor", "unsaved", inhibit_flags::LOGOUT, Some(":1.4")).unwrap();
        assert_eq!(s.blocking_inhibitors(), vec![blocker]);
        let c = s.register_client("editor", Some(":1.4"));
        assert_eq!(s.client_of(Some(":1.4")), Some(&c));
        assert_eq!(s.client_of(Some(":1.5")), None);
    }

    #[test]
    fn initialization_error() {
        let mut s = SessionState::new();
        assert_eq!(s.initialization_error(false), Ok(false));
        assert_eq!(s.phase(), Phase::Initializing);
        assert_eq!(s.initialization_error(true), Ok(true));
        assert_eq!(s.phase(), Phase::Ending);

        let mut s = running();
        assert_eq!(s.initialization_error(true), Err(SessionError::NotInInitialization));
    }
}
