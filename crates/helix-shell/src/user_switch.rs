//! Switching to another user (helix-desktop-spec
//! `contracts/gsettings/org.gnome.desktop.lockdown.md`, "User switching"):
//! the display manager shows its login screen while this session stays,
//! locked.
//!
//! Only on bare hardware (`HELIX_BACKEND=drm`): a nested session must not
//! drive the host's display manager. `HELIX_TEST_USER_SWITCH=1` looks for
//! the display manager on the session bus instead (tests with a fake).

use zbus::zvariant::OwnedObjectPath;
use zbus::Connection;

const GDM: &str = "org.gnome.DisplayManager";
const GDM_PATH: &str = "/org/gnome/DisplayManager/LocalDisplayFactory";
const GDM_IFACE: &str = "org.gnome.DisplayManager.LocalDisplayFactory";
const LIGHTDM: &str = "org.freedesktop.DisplayManager";
const LIGHTDM_SEAT: &str = "org.freedesktop.DisplayManager.Seat";
const ACCOUNTS: &str = "org.freedesktop.Accounts";

/// A display manager that can show its login screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Manager {
    /// GDM: `LocalDisplayFactory.CreateTransientDisplay`.
    Gdm,
    /// LightDM: the session's seat (`XDG_SEAT_PATH`), `SwitchToGreeter`.
    LightDm(OwnedObjectPath),
}

/// The bus to find the display manager on, when switching may be offered.
pub async fn bus() -> Option<Connection> {
    if std::env::var("HELIX_TEST_USER_SWITCH").is_ok_and(|v| v == "1") {
        return Connection::session().await.ok();
    }
    if std::env::var("HELIX_BACKEND").is_ok_and(|b| b == "drm") {
        return Connection::system().await.ok();
    }
    None
}

async fn has_owner(conn: &Connection, name: &str) -> bool {
    let Ok(dbus) = zbus::fdo::DBusProxy::new(conn).await else { return false };
    let Ok(name) = zbus::names::BusName::try_from(name) else { return false };
    dbus.name_has_owner(name).await.unwrap_or(false)
}

/// The display manager to switch through, if one runs.
pub async fn manager(conn: &Connection) -> Option<Manager> {
    if has_owner(conn, GDM).await {
        return Some(Manager::Gdm);
    }
    let seat = std::env::var("XDG_SEAT_PATH").ok().and_then(|p| OwnedObjectPath::try_from(p).ok())?;
    has_owner(conn, LIGHTDM).await.then_some(Manager::LightDm(seat))
}

/// Whether anyone else could log in: AccountsService knows more than one
/// user (true without AccountsService).
pub async fn other_users(conn: &Connection) -> bool {
    let reply = conn.call_method(Some(ACCOUNTS), "/org/freedesktop/Accounts", Some(ACCOUNTS), "ListCachedUsers", &()).await;
    match reply.and_then(|r| r.body().deserialize::<Vec<OwnedObjectPath>>()) {
        Ok(users) => users.len() > 1,
        Err(_) => !has_owner(conn, ACCOUNTS).await,
    }
}

/// Whether "Switch User…" can be offered (before the lockdown key).
pub async fn available(conn: &Connection) -> Option<Manager> {
    let manager = manager(conn).await?;
    other_users(conn).await.then_some(manager)
}

/// Asks the display manager for its login screen.
pub async fn switch(conn: &Connection, manager: &Manager) -> zbus::Result<()> {
    match manager {
        Manager::Gdm => conn.call_method(Some(GDM), GDM_PATH, Some(GDM_IFACE), "CreateTransientDisplay", &()).await.map(|_| ()),
        Manager::LightDm(seat) => conn.call_method(Some(LIGHTDM), seat.as_str(), Some(LIGHTDM_SEAT), "SwitchToGreeter", &()).await.map(|_| ()),
    }
}

/// Whether switching is allowed by `org.gnome.desktop.lockdown`.
pub fn allowed() -> bool {
    !crate::locked_down("disable-user-switching")
}
