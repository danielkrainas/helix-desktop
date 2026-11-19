//! Who is calling the shell's private D-Bus interfaces.
//!
//! Interfaces meant for the rest of the desktop (extension management,
//! the portal backend's dialogs, Bluetooth sending, key grabs) check their
//! caller here: either it owns one of a few well-known names, or at least
//! it is not inside an app sandbox (Flatpak, snap, or any other process
//! with its own root directory). A sandboxed app could otherwise reach them
//! through a name it is allowed to talk to that the same connection owns.

use std::path::{Path, PathBuf};

use zbus::fdo::{self, DBusProxy};
use zbus::message::Header;
use zbus::names::BusName;
use zbus::Connection;

/// The bus name of Helix's portal backend.
pub fn portal_name() -> String {
    std::env::var("HELIX_PORTALS_BUS_NAME").unwrap_or_else(|_| "org.freedesktop.impl.portal.desktop.helix".into())
}

/// Refuse callers other than Helix's portal backend.
pub async fn require_portal(conn: &Connection, header: &Header<'_>) -> fdo::Result<()> {
    require_owner(conn, header, &[portal_name().as_str()]).await
}

/// Whether the caller of `header` owns one of `names`.
pub async fn owns_any(conn: &Connection, header: &Header<'_>, names: &[&str]) -> bool {
    let Some(sender) = header.sender() else { return false };
    let Ok(dbus) = DBusProxy::new(conn).await else { return false };
    for name in names {
        let Ok(name) = BusName::try_from(*name) else { continue };
        if dbus.get_name_owner(name).await.is_ok_and(|owner| owner.as_str() == sender.as_str()) {
            return true;
        }
    }
    false
}

/// Refuse callers that own none of `names`.
pub async fn require_owner(conn: &Connection, header: &Header<'_>, names: &[&str]) -> fdo::Result<()> {
    if owns_any(conn, header, names).await {
        Ok(())
    } else {
        Err(fdo::Error::AccessDenied("not allowed for this caller".into()))
    }
}

/// Whether the caller of `header` is sandboxed (true when it can't be
/// told: no sender, no credentials, another user).
pub async fn sandboxed(conn: &Connection, header: &Header<'_>) -> bool {
    let Some(sender) = header.sender() else { return true };
    let Ok(dbus) = DBusProxy::new(conn).await else { return true };
    let Ok(creds) = dbus.get_connection_credentials(BusName::Unique(sender.to_owned())).await else { return true };
    // SAFETY: getuid cannot fail.
    let me = unsafe { libc::getuid() };
    if creds.unix_user_id() != Some(me) {
        return true;
    }
    let Some(pid) = creds.process_id() else { return true };
    let label = creds.linux_security_label().map(|l| String::from_utf8_lossy(l).into_owned());
    sandboxed_process(label.as_deref(), &PathBuf::from(format!("/proc/{pid}/root")), Path::new("/"))
}

/// Refuse sandboxed callers (see [`sandboxed`]).
pub async fn require_unsandboxed(conn: &Connection, header: &Header<'_>) -> fdo::Result<()> {
    if sandboxed(conn, header).await {
        Err(fdo::Error::AccessDenied("not allowed from a sandbox".into()))
    } else {
        Ok(())
    }
}

/// A process with security label `label` and root directory `root` (as
/// `/proc/PID/root`) is sandboxed when it is confined by a snap's AppArmor
/// profile, has Flatpak's `/.flatpak-info`, or its root is not `own_root`
/// (or can't be looked at).
pub fn sandboxed_process(label: Option<&str>, root: &Path, own_root: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    if label.is_some_and(|l| l.trim_end_matches('\0').starts_with("snap.")) {
        return true;
    }
    if root.join(".flatpak-info").symlink_metadata().is_ok() {
        return true;
    }
    match (std::fs::metadata(root), std::fs::metadata(own_root)) {
        (Ok(theirs), Ok(ours)) => (theirs.dev(), theirs.ino()) != (ours.dev(), ours.ino()),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandboxes() {
        let dir = std::env::temp_dir().join(format!("helix-caller-{}", std::process::id()));
        let (host, flatpak, other) = (dir.join("host"), dir.join("flatpak"), dir.join("other"));
        for d in [&host, &flatpak, &other] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(flatpak.join(".flatpak-info"), "[Application]\nname=org.example.App\n").unwrap();
        // The same root, unconfined (or confined by an ordinary profile).
        assert!(!sandboxed_process(None, &host, &host));
        assert!(!sandboxed_process(Some("unconfined"), &host, &host));
        assert!(!sandboxed_process(Some("/usr/bin/foo (enforce)"), &host, &host));
        // Snap confinement, Flatpak, another root, an unreadable root.
        assert!(sandboxed_process(Some("snap.firefox.firefox (enforce)"), &host, &host));
        assert!(sandboxed_process(None, &flatpak, &flatpak));
        assert!(sandboxed_process(None, &other, &host));
        assert!(sandboxed_process(None, &dir.join("gone"), &host));
        // This process, as /proc shows it.
        assert!(!sandboxed_process(None, &PathBuf::from(format!("/proc/{}/root", std::process::id())), Path::new("/")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
