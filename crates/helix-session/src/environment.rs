//! Publishing the session's launch environment (helix-desktop-spec
//! `org.gnome.SessionManager.md`, `Setenv`): to D-Bus activation and to the
//! systemd user manager, which forgets it again when the session ends.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Mutex;

use zbus::Connection;

const SYSTEMD: &str = "org.freedesktop.systemd1";
const SYSTEMD_PATH: &str = "/org/freedesktop/systemd1";
const SYSTEMD_MANAGER: &str = "org.freedesktop.systemd1.Manager";

/// Variables set in the systemd user manager.
static SET_IN_SYSTEMD: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// The variables naming this login, published from the session's own
/// environment (helix-desktop-spec `org.gnome.SessionManager.md`, "The
/// login's identity").
pub const LOGIN_IDENTITY: [&str; 9] = [
    "XDG_SESSION_TYPE",
    "XDG_SESSION_ID",
    "XDG_SESSION_CLASS",
    "XDG_SEAT",
    "XDG_VTNR",
    "DESKTOP_SESSION",
    "GDMSESSION",
    "XDG_SESSION_DESKTOP",
    "XDG_CURRENT_DESKTOP",
];

/// Add to the session configuration's `[environment]` the login's identity
/// from `process` (the session's own environment), so the bus and systemd
/// stop handing out a previous desktop's values; what the configuration
/// sets already wins. `XDG_MENU_PREFIX` becomes `gnome-`.
pub fn add_login_identity(env: &mut BTreeMap<String, String>, process: impl IntoIterator<Item = (String, String)>) {
    for (name, value) in process {
        if LOGIN_IDENTITY.contains(&name.as_str()) {
            env.entry(name).or_insert(value);
        }
    }
    env.entry("XDG_MENU_PREFIX".into()).or_insert_with(|| "gnome-".into());
}

/// `NAME=value` assignments for systemd.
pub fn assignments<'a>(env: impl IntoIterator<Item = (&'a str, &'a str)>) -> Vec<String> {
    env.into_iter().map(|(k, v)| format!("{k}={v}")).collect()
}

/// Make `env` part of the launch environment: D-Bus activation and the
/// systemd user manager (when it is on this bus).
pub async fn publish(conn: &Connection, env: &HashMap<&str, &str>) {
    if env.is_empty() {
        return;
    }
    let updated = match zbus::fdo::DBusProxy::new(conn).await {
        Ok(dbus) => dbus.update_activation_environment(env.clone()).await,
        Err(err) => Err(err.into()),
    };
    if let Err(err) = updated {
        tracing::warn!(%err, "could not update the D-Bus activation environment");
    }
    let assignments = assignments(env.iter().map(|(k, v)| (*k, *v)));
    match conn.call_method(Some(SYSTEMD), SYSTEMD_PATH, Some(SYSTEMD_MANAGER), "SetEnvironment", &(assignments,)).await {
        Ok(_) => {
            let mut set = SET_IN_SYSTEMD.lock().unwrap_or_else(|e| e.into_inner());
            set.extend(env.keys().map(|k| (*k).to_owned()));
        }
        Err(err) => tracing::debug!(%err, "no systemd user manager to tell the environment"),
    }
}

/// The session ended: unset in systemd what [`publish`] set there.
pub async fn retract(conn: &Connection) {
    let names: Vec<String> = std::mem::take(&mut *SET_IN_SYSTEMD.lock().unwrap_or_else(|e| e.into_inner())).into_iter().collect();
    if names.is_empty() {
        return;
    }
    if let Err(err) = conn.call_method(Some(SYSTEMD), SYSTEMD_PATH, Some(SYSTEMD_MANAGER), "UnsetEnvironment", &(names,)).await {
        tracing::warn!(%err, "could not unset the session environment in systemd");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_identity() {
        let mut env = BTreeMap::from([("XDG_CURRENT_DESKTOP".to_owned(), "Helix:GNOME".to_owned())]);
        let process = [("XDG_SESSION_TYPE", "wayland"), ("XDG_CURRENT_DESKTOP", "XFCE"), ("DESKTOP_SESSION", "helix"), ("PATH", "/usr/bin")];
        add_login_identity(&mut env, process.map(|(k, v)| (k.to_owned(), v.to_owned())));
        let got: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(
            got,
            [("DESKTOP_SESSION", "helix"), ("XDG_CURRENT_DESKTOP", "Helix:GNOME"), ("XDG_MENU_PREFIX", "gnome-"), ("XDG_SESSION_TYPE", "wayland")],
            "the configuration wins; other variables are not published"
        );
        let mut env = BTreeMap::from([("XDG_MENU_PREFIX".to_owned(), "x-".to_owned())]);
        add_login_identity(&mut env, std::iter::empty());
        assert_eq!(env["XDG_MENU_PREFIX"], "x-");
    }

    #[test]
    fn assignment_strings() {
        assert_eq!(assignments([("WAYLAND_DISPLAY", "wayland-1"), ("X", "")]), ["WAYLAND_DISPLAY=wayland-1", "X="]);
    }
}
