//! Offline updates for the end-session dialog (helix-desktop-spec
//! `org.gnome.SessionManager.EndSessionDialog.md`, "Updates"): what
//! PackageKit has prepared, and asking it to install that while the
//! computer restarts or powers off.
//!
//! Only on bare hardware (`HELIX_BACKEND=drm`), or with
//! `HELIX_TEST_PACKAGEKIT=1` (tests, with a fake PackageKit on a private
//! system bus): a nested session must never schedule the host's updates.

use std::collections::HashMap;

use zbus::zvariant::OwnedValue;
use zbus::Connection;

const SERVICE: &str = "org.freedesktop.PackageKit";
const PATH: &str = "/org/freedesktop/PackageKit";
const OFFLINE: &str = "org.freedesktop.PackageKit.Offline";

/// What PackageKit has ready.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Prepared {
    pub update: bool,
    /// An update is already set to be installed at the next boot.
    pub update_triggered: bool,
    /// A system upgrade, by its name and version ("Fedora 42"), if one is
    /// ready.
    pub upgrade: Option<String>,
}

/// A call to PackageKit's `Offline` interface: method and its action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Call {
    Trigger(&'static str),
    TriggerUpgrade(&'static str),
    Cancel,
}

/// Whether the dialog may look at PackageKit.
pub fn allowed() -> bool {
    std::env::var("HELIX_BACKEND").is_ok_and(|b| b == "drm") || std::env::var("HELIX_TEST_PACKAGEKIT").is_ok_and(|v| v == "1")
}

/// The upgrade's name from `PreparedUpgrade` (`name`, `version`).
pub fn upgrade_name(details: &HashMap<String, OwnedValue>) -> String {
    let get = |key: &str| details.get(key).and_then(|v| String::try_from(v.clone()).ok()).unwrap_or_default();
    let name = [get("name"), get("version")].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ");
    if name.is_empty() {
        "A new version of the system".to_owned()
    } else {
        name
    }
}

/// What PackageKit has ready; nothing when it may not be asked or does
/// not answer.
pub async fn prepared() -> Prepared {
    if !allowed() {
        return Prepared::default();
    }
    let Ok(conn) = Connection::system().await else { return Prepared::default() };
    let Ok(proxy) = zbus::fdo::PropertiesProxy::builder(&conn).destination(SERVICE).and_then(|b| b.path(PATH)) else {
        return Prepared::default();
    };
    let Ok(proxy) = proxy.build().await else { return Prepared::default() };
    let Ok(all) = proxy.get_all(zbus::names::InterfaceName::from_static_str_unchecked(OFFLINE)).await else {
        return Prepared::default();
    };
    let flag = |key: &str| all.get(key).and_then(|v| bool::try_from(v.clone()).ok()).unwrap_or(false);
    let upgrade = flag("UpgradePrepared").then(|| {
        let details: HashMap<String, OwnedValue> = all
            .get("PreparedUpgrade")
            .and_then(|v| HashMap::<String, OwnedValue>::try_from(v.clone()).ok())
            .unwrap_or_default();
        upgrade_name(&details)
    });
    Prepared { update: flag("UpdatePrepared") || flag("UpdateTriggered"), update_triggered: flag("UpdateTriggered"), upgrade }
}

/// The call confirming dialog `kind` needs: `install` is the "Install
/// pending software updates" check box (when shown).
pub fn call_for(kind: u32, install: Option<bool>, update_triggered: bool) -> Option<Call> {
    use crate::end_dialog::{REBOOT, SHUTDOWN, UPDATE_REBOOT, UPGRADE_REBOOT};
    let action = |kind| if kind == SHUTDOWN { "power-off" } else { "reboot" };
    match (kind, install) {
        (UPDATE_REBOOT, _) => Some(Call::Trigger("reboot")),
        (UPGRADE_REBOOT, _) => Some(Call::TriggerUpgrade("reboot")),
        (SHUTDOWN | REBOOT, Some(true)) => Some(Call::Trigger(action(kind))),
        (SHUTDOWN | REBOOT, Some(false)) if update_triggered => Some(Call::Cancel),
        _ => None,
    }
}

/// Make `call`.
pub async fn make(call: Call) -> Result<(), String> {
    if !allowed() {
        return Err("PackageKit may not be used here".into());
    }
    let conn = Connection::system().await.map_err(|e| e.to_string())?;
    let reply = match call {
        Call::Trigger(action) => conn.call_method(Some(SERVICE), PATH, Some(OFFLINE), "Trigger", &(action,)).await,
        Call::TriggerUpgrade(action) => conn.call_method(Some(SERVICE), PATH, Some(OFFLINE), "TriggerUpgrade", &(action,)).await,
        Call::Cancel => conn.call_method(Some(SERVICE), PATH, Some(OFFLINE), "Cancel", &()).await,
    };
    tracing::info!(?call, ok = reply.is_ok(), "offline update call");
    reply.map(|_| ()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calls() {
        assert_eq!(call_for(3, None, false), Some(Call::Trigger("reboot")));
        assert_eq!(call_for(4, None, false), Some(Call::TriggerUpgrade("reboot")));
        assert_eq!(call_for(1, Some(true), false), Some(Call::Trigger("power-off")));
        assert_eq!(call_for(2, Some(true), true), Some(Call::Trigger("reboot")));
        assert_eq!(call_for(2, Some(false), true), Some(Call::Cancel), "unticked: not at the next boot either");
        assert_eq!(call_for(2, Some(false), false), None);
        assert_eq!(call_for(1, None, true), None, "no check box: left as it is");
        assert_eq!(call_for(0, Some(true), false), None);
    }

    #[test]
    fn upgrade_names() {
        let s = |v: &str| OwnedValue::try_from(zbus::zvariant::Value::from(v)).unwrap();
        assert_eq!(upgrade_name(&HashMap::from([("name".into(), s("Fedora")), ("version".into(), s("42"))])), "Fedora 42");
        assert_eq!(upgrade_name(&HashMap::new()), "A new version of the system");
    }
}
