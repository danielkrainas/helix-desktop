//! Agent Access policy (helixdesktop/spec `contracts/agent-access/README.md`):
//! access levels, the settings, the errors, and the checks that need no
//! D-Bus: the gate before a grant (switch, remote, lock, level, feature
//! switches), the hard and user exclusions, and sensitive actions.

pub const SCHEMA: &str = "org.helixdesktop.agent-access";

/// The Settings app, whose windows agents may never use.
pub const SETTINGS_APP_ID: &str = "org.helixdesktop.Settings";

/// An access level; each includes the ones before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    ReadOnly,
    Desktop,
    Apps,
    ScreenAndInput,
}

impl Level {
    pub const ALL: [Level; 4] = [Level::ReadOnly, Level::Desktop, Level::Apps, Level::ScreenAndInput];

    /// The value in `access-level` and on D-Bus.
    pub fn id(self) -> &'static str {
        match self {
            Level::ReadOnly => "read-only",
            Level::Desktop => "desktop",
            Level::Apps => "apps",
            Level::ScreenAndInput => "screen-and-input",
        }
    }

    pub fn from_id(id: &str) -> Option<Level> {
        Level::ALL.into_iter().find(|l| l.id() == id)
    }
}

/// `remember-mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RememberMode {
    /// Ask on first use in each session.
    Ask,
    /// *Always allow* is offered and remembered grants are honoured.
    Remember,
}

/// The `org.helixdesktop.agent-access` keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub enabled: bool,
    pub level: Level,
    pub remember: RememberMode,
    pub notifications: bool,
    pub clipboard: bool,
    /// `read-text-fields`: agents get editable text fields' contents.
    pub read_text_fields: bool,
    pub confirm_sensitive: bool,
    pub allow_remote: bool,
    pub excluded_apps: Vec<String>,
    pub log_retention_days: u32,
}

impl Default for Config {
    /// The schema's defaults: Agent Access off.
    fn default() -> Self {
        Config {
            enabled: false,
            level: Level::ReadOnly,
            remember: RememberMode::Ask,
            notifications: false,
            clipboard: false,
            read_text_fields: false,
            confirm_sensitive: true,
            allow_remote: false,
            excluded_apps: Vec::new(),
            log_retention_days: 30,
        }
    }
}

impl Config {
    /// The current settings; keys an older installed schema lacks (or a
    /// missing schema) read as the defaults.
    pub fn read() -> Config {
        let s = helix_settings::Schema::open(SCHEMA);
        let d = Config::default();
        Config {
            enabled: s.get_or("enabled", d.enabled),
            level: s.get::<String>("access-level").and_then(|l| Level::from_id(&l)).unwrap_or(d.level),
            remember: match s.get::<String>("remember-mode").as_deref() {
                Some("remember") => RememberMode::Remember,
                _ => RememberMode::Ask,
            },
            notifications: s.get_or("notifications", d.notifications),
            clipboard: s.get_or("clipboard", d.clipboard),
            read_text_fields: s.get_or("read-text-fields", d.read_text_fields),
            confirm_sensitive: s.get_or("confirm-sensitive", d.confirm_sensitive),
            allow_remote: s.get_or("allow-remote", d.allow_remote),
            excluded_apps: s.strv("excluded-apps").unwrap_or_default(),
            log_retention_days: s.get_or("log-retention-days", d.log_retention_days).clamp(1, 365),
        }
    }

    /// Whether `app_id` is excluded by the user (compared without a
    /// `.desktop` suffix).
    pub fn user_excluded(&self, app_id: &str) -> bool {
        let app_id = bare_app_id(app_id);
        self.excluded_apps.iter().any(|a| bare_app_id(a) == app_id)
    }
}

fn bare_app_id(id: &str) -> &str {
    id.strip_suffix(".desktop").unwrap_or(id)
}

macro_rules! agent_errors {
    ($($variant:ident),* $(,)?) => {
        /// The errors of `org.helixdesktop.Agent1` (README.md, "Errors"),
        /// plus `org.freedesktop.DBus.Error.InvalidArgs` for bad arguments.
        #[derive(Debug)]
        pub enum AgentError {
            ZBus(zbus::Error),
            InvalidArgs(String),
            $($variant(String),)*
        }

        impl AgentError {
            /// The error's short name (the part after the prefix), for logs
            /// and tests.
            pub fn kind(&self) -> &'static str {
                match self {
                    AgentError::ZBus(_) => "ZBus",
                    AgentError::InvalidArgs(_) => "InvalidArgs",
                    $(AgentError::$variant(_) => stringify!($variant),)*
                }
            }

            fn full_name(&self) -> &'static str {
                match self {
                    AgentError::ZBus(_) => "org.freedesktop.zbus.Error",
                    AgentError::InvalidArgs(_) => "org.freedesktop.DBus.Error.InvalidArgs",
                    $(AgentError::$variant(_) => concat!("org.helixdesktop.Agent1.Error.", stringify!($variant)),)*
                }
            }

            fn message(&self) -> Option<&str> {
                match self {
                    AgentError::ZBus(_) => None,
                    AgentError::InvalidArgs(m) $(| AgentError::$variant(m))* => Some(m),
                }
            }
        }
    };
}

agent_errors!(
    Sandboxed,
    AccessOff,
    Remote,
    Locked,
    LevelTooLow,
    FeatureOff,
    NotGranted,
    DeniedByUser,
    Excluded,
    NeedsConfirmation,
    Cancelled,
    TimedOut,
    NotFound,
    NotSupported,
    Failed,
);

impl AgentError {
    /// `org.freedesktop.DBus.Error.InvalidArgs`.
    pub fn invalid_args(message: impl Into<String>) -> AgentError {
        AgentError::InvalidArgs(message.into())
    }
}

impl zbus::DBusError for AgentError {
    fn create_reply(&self, call: &zbus::message::Header<'_>) -> zbus::Result<zbus::Message> {
        let text = match self {
            AgentError::ZBus(e) => e.to_string(),
            other => other.message().unwrap_or_default().to_owned(),
        };
        zbus::Message::error(call, self.name())?.build(&(text,))
    }

    fn name(&self) -> zbus::names::ErrorName<'_> {
        zbus::names::ErrorName::from_static_str_unchecked(self.full_name())
    }

    fn description(&self) -> Option<&str> {
        self.message()
    }
}

impl From<zbus::Error> for AgentError {
    fn from(e: zbus::Error) -> Self {
        AgentError::ZBus(e)
    }
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentError::ZBus(e) => write!(f, "{e}"),
            other => write!(f, "{}: {}", other.full_name(), other.message().unwrap_or_default()),
        }
    }
}

impl std::error::Error for AgentError {}

/// A separate switch a method needs on top of its level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Switch {
    Clipboard,
    /// Notification contents (`NotificationAction`: the buttons' labels
    /// say what the notification says).
    Notifications,
}

/// What a method needs (README.md, "Methods").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Need {
    /// `None`: no grant (`GetStatus`).
    pub level: Option<Level>,
    pub switch: Option<Switch>,
    /// Always a sensitive action.
    pub sensitive: bool,
    /// Ask the user for exactly this level, not `access-level` when that
    /// is higher (`RequestAccess`).
    pub exact: bool,
}

impl Need {
    /// No grant needed.
    pub const STATUS: Need = Need { level: None, switch: None, sensitive: false, exact: false };

    pub const fn level(level: Level) -> Need {
        Need { level: Some(level), switch: None, sensitive: false, exact: false }
    }
}

/// The caller's situation, as far as the gate needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Facts {
    /// In a remote session (or the shell could not tell).
    pub remote: bool,
    /// The screen is locked or locking.
    pub locked: bool,
}

/// Steps 3–7 of the policy check: the master switch, remote sessions,
/// the lock, the level and the feature switches. `remote` is asked only
/// when the switch is on (it may mean asking logind).
pub fn gate(config: &Config, need: Need, facts: Facts) -> Result<(), AgentError> {
    let Some(level) = need.level else { return Ok(()) };
    if !config.enabled {
        return Err(AgentError::AccessOff("agent access is turned off".into()));
    }
    if facts.remote && !config.allow_remote {
        return Err(AgentError::Remote("agent access from remote sessions is turned off".into()));
    }
    if facts.locked {
        return Err(AgentError::Locked("the screen is locked".into()));
    }
    if level > config.level {
        return Err(AgentError::LevelTooLow(level.id().into()));
    }
    match need.switch {
        Some(Switch::Clipboard) if !config.clipboard => Err(AgentError::FeatureOff("clipboard".into())),
        Some(Switch::Notifications) if !config.notifications => Err(AgentError::FeatureOff("notifications".into())),
        _ => Ok(()),
    }
}

/// The level to ask the user for when a call needing `need` has no grant.
pub fn level_to_ask(config: &Config, need: Need) -> Level {
    let level = need.level.unwrap_or(Level::ReadOnly);
    if need.exact { level } else { level.max(config.level) }
}

/// Something an agent wants to read or act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target<'a> {
    /// An app (launching it, or one of its windows), by app id.
    App(&'a str),
    /// A password field (accessible role `password text`, or the
    /// `PASSWORD` state).
    PasswordField,
    /// The screen, the clipboard or input while a system prompt (polkit,
    /// keyring, network secrets, Bluetooth pairing, the consent or a
    /// sensitive-action prompt) is open.
    SystemPrompt,
    /// The shell's own UI.
    Shell,
}

/// The exclusions (README.md, "Hard exclusions"): `Excluded` with the
/// documented token, or `Ok`.
pub fn check_target(config: &Config, target: Target<'_>) -> Result<(), AgentError> {
    let token = match target {
        Target::App(app_id) if bare_app_id(app_id) == SETTINGS_APP_ID => "settings".to_owned(),
        Target::App(app_id) if config.user_excluded(app_id) => format!("app:{}", bare_app_id(app_id)),
        Target::App(_) => return Ok(()),
        Target::PasswordField => "password-field".to_owned(),
        Target::SystemPrompt => "system-prompt".to_owned(),
        Target::Shell => "shell".to_owned(),
    };
    Err(AgentError::Excluded(token))
}

/// Whether windows (and notifications, apps) of `app_id` are left out of
/// what agents see.
pub fn hidden(config: &Config, app_id: &str) -> bool {
    check_target(config, Target::App(app_id)).is_err()
}

/// What to do about a possibly sensitive call (README.md, "Sensitive
/// actions"): `Ok(false)` go on, `Ok(true)` ask the user first, or the
/// error when the user may not be asked.
pub fn sensitive_step(config: &Config, need: Need, marked: bool, interactive: bool) -> Result<bool, AgentError> {
    if !(need.sensitive || marked) || !config.confirm_sensitive || need.level.is_none() {
        return Ok(false);
    }
    if interactive {
        Ok(true)
    } else {
        Err(AgentError::NeedsConfirmation("this action needs the user's confirmation".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on() -> Config {
        Config { enabled: true, ..Config::default() }
    }

    fn kind(r: Result<(), AgentError>) -> &'static str {
        r.err().map_or("ok", |e| e.kind())
    }

    #[test]
    fn levels() {
        assert!(Level::ReadOnly < Level::Desktop && Level::Desktop < Level::Apps && Level::Apps < Level::ScreenAndInput);
        for l in Level::ALL {
            assert_eq!(Level::from_id(l.id()), Some(l));
        }
        assert_eq!(Level::from_id("all"), None);
    }

    #[test]
    fn defaults_are_off() {
        let d = Config::default();
        assert!(!d.enabled && !d.notifications && !d.clipboard && !d.allow_remote);
        assert!(d.confirm_sensitive);
        assert_eq!((d.level, d.remember, d.log_retention_days), (Level::ReadOnly, RememberMode::Ask, 30));
    }

    #[test]
    fn gate_refusals_in_order() {
        let read = Need::level(Level::ReadOnly);
        let both = Facts { remote: true, locked: true };
        // Off beats everything else.
        assert_eq!(kind(gate(&Config::default(), read, both)), "AccessOff");
        // GetStatus needs nothing.
        assert_eq!(kind(gate(&Config::default(), Need::STATUS, both)), "ok");
        // Remote before the lock; allowed remote goes on to the lock.
        assert_eq!(kind(gate(&on(), read, both)), "Remote");
        let remote_ok = Config { allow_remote: true, ..on() };
        assert_eq!(kind(gate(&remote_ok, read, both)), "Locked");
        assert_eq!(kind(gate(&remote_ok, read, Facts { remote: true, locked: false })), "ok");
        assert_eq!(kind(gate(&on(), read, Facts { remote: false, locked: true })), "Locked");
        // Above the configured level.
        assert_eq!(kind(gate(&on(), Need::level(Level::Desktop), Facts::default())), "LevelTooLow");
        let apps = Config { level: Level::Apps, ..on() };
        assert_eq!(kind(gate(&apps, Need::level(Level::Desktop), Facts::default())), "ok");
        assert_eq!(kind(gate(&apps, Need::level(Level::ScreenAndInput), Facts::default())), "LevelTooLow");
        match gate(&on(), Need::level(Level::Apps), Facts::default()) {
            Err(AgentError::LevelTooLow(m)) => assert_eq!(m, "apps", "names the level needed"),
            other => panic!("{other:?}"),
        }
        // The clipboard switch.
        let clip = Need { switch: Some(Switch::Clipboard), ..read };
        assert_eq!(kind(gate(&on(), clip, Facts::default())), "FeatureOff");
        assert_eq!(kind(gate(&Config { clipboard: true, ..on() }, clip, Facts::default())), "ok");
    }

    #[test]
    fn asks_for_the_configured_level() {
        let desktop = Config { level: Level::Desktop, ..on() };
        assert_eq!(level_to_ask(&desktop, Need::level(Level::ReadOnly)), Level::Desktop);
        assert_eq!(level_to_ask(&desktop, Need { exact: true, ..Need::level(Level::ReadOnly) }), Level::ReadOnly);
        assert_eq!(level_to_ask(&on(), Need::level(Level::ReadOnly)), Level::ReadOnly);
    }

    #[test]
    fn exclusions() {
        let config = Config { excluded_apps: vec!["org.keepassxc.KeePassXC.desktop".into(), "com.example.Bank".into()], ..on() };
        assert_eq!(kind(check_target(&config, Target::App("org.gnome.TextEditor"))), "ok");
        for (target, token) in [
            (Target::App("org.helixdesktop.Settings"), "settings"),
            (Target::App("org.helixdesktop.Settings.desktop"), "settings"),
            (Target::App("org.keepassxc.KeePassXC"), "app:org.keepassxc.KeePassXC"),
            (Target::App("com.example.Bank.desktop"), "app:com.example.Bank"),
            (Target::PasswordField, "password-field"),
            (Target::SystemPrompt, "system-prompt"),
            (Target::Shell, "shell"),
        ] {
            match check_target(&config, target) {
                Err(AgentError::Excluded(t)) => assert_eq!(t, token),
                other => panic!("{target:?}: {other:?}"),
            }
        }
        // Settings is excluded even with an empty user list.
        assert!(hidden(&on(), SETTINGS_APP_ID));
        assert!(!hidden(&on(), "org.keepassxc.KeePassXC"));
    }

    #[test]
    fn sensitive_actions() {
        let read = Need::level(Level::Desktop);
        let always = Need { sensitive: true, ..read };
        assert!(!sensitive_step(&on(), read, false, true).unwrap(), "not sensitive");
        assert!(sensitive_step(&on(), read, true, true).unwrap(), "marked by the agent");
        assert!(sensitive_step(&on(), always, false, true).unwrap(), "sensitive by itself");
        assert_eq!(sensitive_step(&on(), always, false, false).unwrap_err().kind(), "NeedsConfirmation");
        let trusting = Config { confirm_sensitive: false, ..on() };
        assert!(!sensitive_step(&trusting, always, true, false).unwrap(), "confirm-sensitive off");
    }

    #[test]
    fn error_names() {
        use zbus::DBusError;
        assert_eq!(AgentError::AccessOff(String::new()).name().as_str(), "org.helixdesktop.Agent1.Error.AccessOff");
        assert_eq!(AgentError::NeedsConfirmation(String::new()).name().as_str(), "org.helixdesktop.Agent1.Error.NeedsConfirmation");
        assert_eq!(AgentError::invalid_args("x").name().as_str(), "org.freedesktop.DBus.Error.InvalidArgs");
    }
}
