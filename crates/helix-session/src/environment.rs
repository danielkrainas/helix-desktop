//! Publishing the session's launch environment (helix-desktop-spec
//! `org.gnome.SessionManager.md`, `Setenv`): to D-Bus activation and to the
//! systemd user manager, which forgets it again when the session ends.

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;

use zbus::Connection;

const SYSTEMD: &str = "org.freedesktop.systemd1";
const SYSTEMD_PATH: &str = "/org/freedesktop/systemd1";
const SYSTEMD_MANAGER: &str = "org.freedesktop.systemd1.Manager";

/// Variables set in the systemd user manager.
static SET_IN_SYSTEMD: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

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
    fn assignment_strings() {
        assert_eq!(assignments([("WAYLAND_DISPLAY", "wayland-1"), ("X", "")]), ["WAYLAND_DISPLAY=wayland-1", "X="]);
    }
}
