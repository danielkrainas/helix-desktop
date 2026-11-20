//! Exit codes and messages (helixdesktop/spec
//! `contracts/agent-access/cli.md`, "Exit codes and messages"): every
//! refusal comes from the shell as a D-Bus error, and becomes one of
//! these.

use helix_i18n::{tr, trf};

const PREFIX: &str = "org.helixdesktop.Agent1.Error.";

/// The `code`s and their exit codes, in order.
pub const CODES: &[(&str, i32)] = &[
    ("failed", 1),
    ("usage", 2),
    ("no-shell", 3),
    ("access-off", 10),
    ("level-too-low", 11),
    ("not-granted", 12),
    ("denied", 13),
    ("excluded", 14),
    ("locked", 15),
    ("remote", 16),
    ("sandboxed", 17),
    ("needs-confirmation", 18),
    ("feature-off", 19),
    ("no", 20),
    ("timed-out", 21),
    ("not-found", 22),
    ("not-supported", 23),
];

/// A failure: what JSON's `error` object holds.
#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    pub code: &'static str,
    pub message: String,
    pub dbus_error: Option<String>,
    /// What standard output says instead of the error (`confirm`
    /// answered no: `{"yes": false}`, and nothing in text).
    pub output: Option<serde_json::Value>,
}

impl Failure {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), dbus_error: None, output: None }
    }

    /// `confirm` answered no: exit 20 with `{"yes": false}`.
    pub fn said_no() -> Self {
        Self { output: Some(serde_json::json!({ "yes": false })), ..Self::new("no", tr("The user said no")) }
    }

    pub fn usage(message: impl Into<String>) -> Self {
        Self::new("usage", message)
    }

    pub fn exit_code(&self) -> i32 {
        exit_code(self.code)
    }

    pub fn no_shell() -> Self {
        Self::new("no-shell", tr("Cannot reach the Helix shell (is a Helix session running?)"))
    }

    pub fn not_supported() -> Self {
        Self::new("not-supported", tr("Not supported by this Helix session"))
    }

    pub fn timed_out() -> Self {
        Self::new("timed-out", tr("Timed out"))
    }

    pub fn to_json(&self) -> serde_json::Value {
        let mut error = serde_json::json!({ "code": self.code, "exit-code": self.exit_code(), "message": self.message });
        if let Some(name) = &self.dbus_error {
            error["dbus-error"] = name.clone().into();
        }
        serde_json::json!({ "error": error })
    }
}

pub fn exit_code(code: &str) -> i32 {
    CODES.iter().find(|(c, _)| *c == code).map_or(1, |(_, e)| *e)
}

/// What each code means, for `help exit-codes`.
pub fn describe(code: &str) -> String {
    match code {
        "failed" => tr("Something else went wrong").into(),
        "usage" => tr("The command line is wrong").into(),
        "no-shell" => tr("Cannot reach the Helix shell (is a Helix session running?)").into(),
        "access-off" => tr("Agent access is turned off (Settings › Privacy & Security › Agent Access)").into(),
        "level-too-low" => tr("The access level set in Settings is too low for this").into(),
        "not-granted" => tr("The user has not allowed this agent, and could not be asked").into(),
        "denied" => tr("The user denied this agent access").into(),
        "excluded" => tr("The target is excluded from agent access").into(),
        "locked" => tr("The screen is locked; agents can't act until it is unlocked").into(),
        "remote" => tr("Agent access from remote sessions is turned off (Settings › Privacy & Security › Agent Access)").into(),
        "sandboxed" => tr("Agent access is not available to sandboxed apps").into(),
        "needs-confirmation" => tr("This action needs the user's confirmation; run it without --no-ask to ask").into(),
        "feature-off" => tr("Clipboard access is turned off (Settings › Privacy & Security › Agent Access)").into(),
        "no" => tr("The user said no, or cancelled the question").into(),
        "timed-out" => tr("Timed out").into(),
        "not-found" => tr("No such window, app or item").into(),
        "not-supported" => tr("Not supported by this Helix session").into(),
        _ => String::new(),
    }
}

/// An app's name for messages (its desktop file's), else its id.
fn app_name(id: &str) -> String {
    use gio::prelude::AppInfoExt;
    let id = id.strip_suffix(".desktop").unwrap_or(id);
    let file = format!("{id}.desktop");
    gio::AppInfo::all().into_iter().find(|app| app.id().is_some_and(|i| i == file)).map_or_else(|| id.to_owned(), |app| app.name().to_string())
}

/// The failure for a D-Bus error `name` with `message` (the shell's).
pub fn from_dbus(name: &str, message: &str) -> Failure {
    let (code, text): (&'static str, String) = match name.strip_prefix(PREFIX) {
        Some("AccessOff") => ("access-off", tr("Agent access is turned off (Settings › Privacy & Security › Agent Access)").into()),
        Some("LevelTooLow") => ("level-too-low", trf!("This needs the “{level}” access level (Settings › Privacy & Security › Agent Access)", level = crate::cli::level_name(message.trim()))),
        Some("NotGranted") => ("not-granted", tr("The user has not allowed this agent, and could not be asked").into()),
        Some("DeniedByUser") => ("denied", tr("The user denied this agent access").into()),
        Some("Excluded") => ("excluded", excluded_message(message.trim())),
        Some("Locked") => ("locked", tr("The screen is locked; agents can't act until it is unlocked").into()),
        Some("Remote") => ("remote", tr("Agent access from remote sessions is turned off (Settings › Privacy & Security › Agent Access)").into()),
        Some("Sandboxed") => ("sandboxed", tr("Agent access is not available to sandboxed apps").into()),
        Some("NeedsConfirmation") => ("needs-confirmation", tr("This action needs the user's confirmation; run it without --no-ask to ask").into()),
        Some("FeatureOff") => ("feature-off", tr("Clipboard access is turned off (Settings › Privacy & Security › Agent Access)").into()),
        Some("Cancelled") => ("no", tr("The user cancelled the question").into()),
        Some("TimedOut") => ("timed-out", tr("Timed out").into()),
        Some("NotFound") => ("not-found", trf!("No such window / app / item: {what}", what = message)),
        Some("NotSupported") => ("not-supported", tr("Not supported by this Helix session").into()),
        _ => match name {
            "org.freedesktop.DBus.Error.UnknownMethod" => ("not-supported", tr("Not supported by this Helix session").into()),
            // No shell on the bus, or a shell without Agent Access.
            "org.freedesktop.DBus.Error.ServiceUnknown"
            | "org.freedesktop.DBus.Error.NameHasNoOwner"
            | "org.freedesktop.DBus.Error.UnknownObject"
            | "org.freedesktop.DBus.Error.UnknownInterface"
            | "org.freedesktop.DBus.Error.NoReply"
            | "org.freedesktop.DBus.Error.Disconnected" => ("no-shell", tr("Cannot reach the Helix shell (is a Helix session running?)").into()),
            "org.freedesktop.DBus.Error.InvalidArgs" => ("usage", message.to_owned()),
            _ => ("failed", if message.is_empty() { name.to_owned() } else { message.to_owned() }),
        },
    };
    Failure { code, message: text, dbus_error: Some(name.to_owned()), output: None }
}

/// `Excluded`'s message by its token.
fn excluded_message(token: &str) -> String {
    match token {
        "settings" => tr("Agents can't use Settings").into(),
        "password-field" => tr("Agents can't use password fields").into(),
        "system-prompt" => tr("Agents can't act while a system prompt is open").into(),
        "shell" => tr("Agents can't act on the shell itself").into(),
        _ => match token.strip_prefix("app:") {
            Some(id) => trf!("{app} is excluded from agent access", app = app_name(id)),
            None => tr("The target is excluded from agent access").into(),
        },
    }
}

/// The failure for a zbus error.
pub fn from_zbus(err: zbus::Error) -> Failure {
    match err {
        zbus::Error::MethodError(name, message, _) => from_dbus(name.as_str(), message.as_deref().unwrap_or("")),
        zbus::Error::FDO(e) => {
            let name = zbus::DBusError::name(&*e).to_string();
            let message = zbus::DBusError::description(&*e).unwrap_or("").to_owned();
            from_dbus(&name, &message)
        }
        zbus::Error::InputOutput(_) | zbus::Error::Address(_) | zbus::Error::Handshake(_) => Failure::no_shell(),
        other => Failure::new("failed", other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes() {
        assert_eq!(from_dbus("org.helixdesktop.Agent1.Error.AccessOff", "").exit_code(), 10);
        let off = from_dbus("org.helixdesktop.Agent1.Error.AccessOff", "");
        assert_eq!(off.message, "Agent access is turned off (Settings › Privacy & Security › Agent Access)");
        assert_eq!(from_dbus("org.helixdesktop.Agent1.Error.LevelTooLow", "apps").message, "This needs the “Control apps” access level (Settings › Privacy & Security › Agent Access)");
        assert_eq!(from_dbus("org.helixdesktop.Agent1.Error.Excluded", "settings").exit_code(), 14);
        assert_eq!(from_dbus("org.helixdesktop.Agent1.Error.Excluded", "app:org.example.NoSuchApp").message, "org.example.NoSuchApp is excluded from agent access");
        assert_eq!(from_dbus("org.freedesktop.DBus.Error.UnknownMethod", "x").exit_code(), 23);
        assert_eq!(from_dbus("org.freedesktop.DBus.Error.ServiceUnknown", "x").exit_code(), 3);
        assert_eq!(from_dbus("org.helixdesktop.Agent1.Error.Failed", "it broke").message, "it broke");
        for (code, exit) in CODES {
            assert_eq!(exit_code(code), *exit);
            assert!(!describe(code).is_empty(), "{code}");
        }
    }

    #[test]
    fn json_shape() {
        let json = from_dbus("org.helixdesktop.Agent1.Error.AccessOff", "").to_json();
        assert_eq!(json["error"]["code"], "access-off");
        assert_eq!(json["error"]["exit-code"], 10);
        assert_eq!(json["error"]["dbus-error"], "org.helixdesktop.Agent1.Error.AccessOff");
        assert!(Failure::usage("bad").to_json()["error"].get("dbus-error").is_none());
    }
}
