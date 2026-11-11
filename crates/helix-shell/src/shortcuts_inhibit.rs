//! Remembered answers to "Allow APP to use all keyboard shortcuts?"
//! (helix-desktop-spec `contracts/wayland/README.md`, "Shortcut
//! inhibition"), in the xdg permission store.

use std::collections::HashMap;

use zbus::Connection;

const DEST: &str = "org.freedesktop.impl.portal.PermissionStore";
const PATH: &str = "/org/freedesktop/impl/portal/PermissionStore";
const IFACE: &str = "org.freedesktop.impl.portal.PermissionStore";
pub const TABLE: &str = "gnome";
pub const ID: &str = "shortcuts-inhibitor";

/// The stored answer for `app_id`, if any.
pub fn decision(permissions: &HashMap<String, Vec<String>>, app_id: &str) -> Option<bool> {
    match permissions.get(app_id)?.first()?.as_str() {
        "GRANTED" => Some(true),
        "DENIED" => Some(false),
        _ => None,
    }
}

/// Look up the answer for `app_id` (None: not asked yet, or no store).
pub async fn lookup(conn: &Connection, app_id: &str) -> Option<bool> {
    let reply = conn
        .call_method(Some(DEST), PATH, Some(IFACE), "Lookup", &(TABLE, ID))
        .await
        .ok()?;
    let (permissions, _data): (HashMap<String, Vec<String>>, zbus::zvariant::OwnedValue) = reply.body().deserialize().ok()?;
    decision(&permissions, app_id)
}

/// Remember the answer for `app_id`.
pub async fn store(conn: &Connection, app_id: &str, allowed: bool) -> zbus::Result<()> {
    let value = if allowed { "GRANTED" } else { "DENIED" };
    conn.call_method(Some(DEST), PATH, Some(IFACE), "SetPermission", &(TABLE, true, ID, app_id, vec![value]))
        .await
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decisions() {
        let mut p = HashMap::new();
        p.insert("a".to_owned(), vec!["GRANTED".to_owned()]);
        p.insert("b".to_owned(), vec!["DENIED".to_owned()]);
        p.insert("c".to_owned(), vec![]);
        assert_eq!(decision(&p, "a"), Some(true));
        assert_eq!(decision(&p, "b"), Some(false));
        assert_eq!(decision(&p, "c"), None);
        assert_eq!(decision(&p, "d"), None);
    }
}
