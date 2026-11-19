//! "What is open in app X": a Window record's `document`
//! (helixdesktop/spec `contracts/agent-access/README.md`, "Records",
//! "Open documents"; helixdesktop/desktop#146), from what apps already
//! tell the desktop, so no app needs changes:
//!
//! 1. A media player's MPRIS `xesam:url` (`DesktopEntry` names its app),
//!    for that app's most recently used window.
//! 2. `recently-used.xbel`: the item the window's app registered most
//!    recently whose name is in the window's title (Text Editor, Document
//!    Viewer, Image Viewer and other apps that record recent files and
//!    show the file's name in the title).
//!
//! Neither reads the app's accessibility tree, which is the *Control
//! apps* level's (helixdesktop/desktop#149).

use std::collections::HashMap;
use std::time::Duration;

use zbus::zvariant::OwnedValue;
use zbus::Connection;

use super::desktop::{same_app, window_of, Installed, WindowInfo};
use super::files::{display_name, registered_by_window, Recent};

const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";
const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";

/// The shortest file name matched in a title (shorter ones match by
/// chance).
const MIN_NAME: usize = 3;

/// A media player's property.
async fn property(conn: &Connection, name: &str, iface: &str, prop: &str) -> Option<OwnedValue> {
    let reply = conn.call_method(Some(name), MPRIS_PATH, Some("org.freedesktop.DBus.Properties"), "Get", &(iface, prop)).await.ok()?;
    reply.body().deserialize::<OwnedValue>().ok()
}

/// What each running media player plays: (its desktop file id, the URL).
pub async fn players(conn: &Connection) -> Vec<(String, String)> {
    let ask = async {
        let dbus = zbus::fdo::DBusProxy::new(conn).await.ok()?;
        let names = dbus.list_names().await.ok()?;
        let mut out = Vec::new();
        for name in names.iter().filter(|n| n.as_str().starts_with(MPRIS_PREFIX)) {
            let one = async {
                let entry = property(conn, name.as_str(), "org.mpris.MediaPlayer2", "DesktopEntry").await?;
                let entry = String::try_from(entry).ok().filter(|e| !e.is_empty())?;
                let metadata = property(conn, name.as_str(), "org.mpris.MediaPlayer2.Player", "Metadata").await?;
                let metadata = HashMap::<String, OwnedValue>::try_from(metadata).ok()?;
                let url = metadata.get("xesam:url").and_then(|v| String::try_from(v.clone()).ok()).filter(|u| !u.is_empty())?;
                Some((entry, url))
            };
            if let Ok(Some(found)) = tokio::time::timeout(Duration::from_secs(1), one).await {
                out.push(found);
            }
        }
        Some(out)
    };
    tokio::time::timeout(Duration::from_secs(3), ask).await.ok().flatten().unwrap_or_default()
}

/// The documents of `windows` (most recently used first) where their apps
/// tell: from `players` (see [`players`]), else from `recent`.
pub fn documents(windows: &[&WindowInfo], installed: &[Installed], players: &[(String, String)], recent: &[Recent]) -> HashMap<u32, String> {
    let mut out = HashMap::new();
    for (entry, url) in players {
        let app = installed.iter().find(|a| same_app(&a.id, entry));
        let window = windows.iter().find(|w| same_app(&w.app_id, entry) || app.is_some_and(|a| window_of(a, w)));
        if let Some(w) = window {
            out.entry(w.id).or_insert_with(|| url.clone());
        }
    }
    for w in windows {
        if out.contains_key(&w.id) || w.title.is_empty() {
            continue;
        }
        let best = recent
            .iter()
            .filter(|item| !item.private)
            .filter(|item| {
                let name = display_name(&item.uri);
                name.chars().count() >= MIN_NAME && w.title.contains(&name)
            })
            .filter(|item| registered_by_window(installed, &w.app_id, item))
            .max_by_key(|item| item.used);
        if let Some(item) = best {
            out.insert(w.id, item.uri.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(id: u32, app: &str, title: &str) -> WindowInfo {
        WindowInfo { id, app_id: app.into(), title: title.into(), ..Default::default() }
    }

    fn item(uri: &str, used: i64, app: &str) -> Recent {
        Recent { uri: uri.into(), mime_type: String::new(), used, apps: vec![(app.into(), String::new())], private: false }
    }

    #[test]
    fn from_titles_and_players() {
        let windows = [
            win(1, "org.gnome.TextEditor", "notes.txt (~/Documents) - Text Editor"),
            win(2, "org.gnome.Papers", "report.pdf"),
            win(3, "org.gnome.Showtime", "Video"),
            win(4, "org.gnome.Showtime", "Other video"),
            win(5, "org.gnome.TextEditor", "a"),
            win(6, "org.gnome.Loupe", "photo.jpg"),
        ];
        let refs: Vec<&WindowInfo> = windows.iter().collect();
        let installed = vec![Installed { id: "org.gnome.TextEditor".into(), name: "Text Editor".into(), wm_class: None, exec: Some("gnome-text-editor".into()) }];
        let recent = vec![
            item("file:///home/u/Documents/notes.txt", 10, "Text Editor"),
            item("file:///home/u/Old/notes.txt", 5, "Text Editor"),
            item("file:///home/u/report.pdf", 7, "org.gnome.Papers"),
            item("file:///home/u/a", 9, "Text Editor"),
            // Another app's file of that name is not the viewer's.
            item("file:///home/u/photo.jpg", 9, "org.gnome.Nautilus"),
        ];
        let players = vec![("org.gnome.Showtime.desktop".to_owned(), "file:///home/u/Videos/v.mkv".to_owned())];
        let docs = documents(&refs, &installed, &players, &recent);
        assert_eq!(docs.get(&1).map(String::as_str), Some("file:///home/u/Documents/notes.txt"), "the most recent of that name");
        assert_eq!(docs.get(&2).map(String::as_str), Some("file:///home/u/report.pdf"));
        assert_eq!(docs.get(&3).map(String::as_str), Some("file:///home/u/Videos/v.mkv"), "the player's most recently used window");
        assert!(!docs.contains_key(&4));
        assert!(!docs.contains_key(&5), "names too short to match");
        assert!(!docs.contains_key(&6));
    }
}
