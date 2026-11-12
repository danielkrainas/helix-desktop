//! `org.freedesktop.impl.portal.Wallpaper` (helix-desktop-spec
//! `contracts/dbus/org.freedesktop.impl.portal.md`, "Wallpaper"): apps
//! set the background or lock screen picture, after the user saw it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gio::prelude::*;
use zbus::zvariant::{ObjectPath, OwnedValue, Value};
use zbus::{fdo, interface, Connection, ObjectServer};

use crate::dialogs::{app_name, dialog, value};
use crate::screenshot::{response, with_request, Results};

const BACKGROUND: &str = "org.gnome.desktop.background";
const SCREENSAVER: &str = "org.gnome.desktop.screensaver";

/// Where the picture goes: (background, lock screen).
pub fn targets(set_on: Option<&str>) -> Option<(bool, bool)> {
    match set_on.unwrap_or("both") {
        "both" => Some((true, true)),
        "background" => Some((true, false)),
        "lockscreen" => Some((false, true)),
        _ => None,
    }
}

/// The copy's file name: a timestamp and the original name.
pub fn copy_name(stamp: &str, original: &Path) -> String {
    let name = original.file_name().map_or_else(|| "background".into(), |n| n.to_string_lossy().into_owned());
    format!("{stamp}-{name}")
}

fn backgrounds_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".local/share")))
        .map(|d| d.join("backgrounds"))
}

/// Copy `source` into the backgrounds folder (the app's file may go
/// away) and set it; the copy's URI.
fn apply(source: &Path, background: bool, lock_screen: bool) -> Result<String, String> {
    let dir = backgrounds_dir().ok_or("no data folder")?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let stamp = gio::glib::DateTime::now_local().and_then(|d| d.format("%Y-%m-%d-%H-%M-%S")).map_err(|e| e.to_string())?;
    let target = dir.join(copy_name(&stamp, source));
    std::fs::copy(source, &target).map_err(|e| format!("cannot copy {}: {e}", source.display()))?;
    let uri = gio::File::for_path(&target).uri().to_string();
    let set = |schema: &str, keys: &[&str]| -> Result<(), String> {
        if !helix_settings::has_schema(schema) {
            return Err(format!("{schema} is not installed"));
        }
        let settings = gio::Settings::new(schema);
        for key in keys {
            settings.set_string(key, &uri).map_err(|e| e.to_string())?;
        }
        // A picture would not show with no picture placement.
        if settings.string("picture-options") == "none" {
            settings.set_string("picture-options", "zoom").map_err(|e| e.to_string())?;
        }
        Ok(())
    };
    if background {
        set(BACKGROUND, &["picture-uri", "picture-uri-dark"])?;
    }
    if lock_screen {
        set(SCREENSAVER, &["picture-uri"])?;
    }
    gio::Settings::sync();
    Ok(uri)
}

pub struct WallpaperBackend {
    pub conn: Connection,
}

#[interface(name = "org.freedesktop.impl.portal.Wallpaper")]
impl WallpaperBackend {
    #[zbus(name = "SetWallpaperURI")]
    async fn set_wallpaper_uri(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        handle: ObjectPath<'_>,
        app_id: String,
        _parent_window: String,
        uri: String,
        options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<u32> {
        let preview = options.get("show-preview").and_then(|v| bool::try_from(v.clone()).ok()).unwrap_or(false);
        let set_on = options.get("set-on").and_then(|v| String::try_from(v.clone()).ok());
        let conn = self.conn.clone();
        tracing::info!(app_id, uri, preview, ?set_on, "set background");
        let (code, _) = with_request(server, handle, || async move {
            let Some((background, lock_screen)) = targets(set_on.as_deref()) else {
                return (response::OTHER, Results::new());
            };
            let file = gio::File::for_uri(&uri);
            let Some(path) = file.path().filter(|p| p.is_file()) else {
                tracing::warn!(uri, "not a local file");
                return (response::OTHER, Results::new());
            };
            if preview {
                let who = if app_id.is_empty() { "An app".to_owned() } else { app_name(&app_id) };
                let what = match (background, lock_screen) {
                    (true, true) => "the background and lock screen",
                    (true, false) => "the background",
                    _ => "the lock screen",
                };
                let options = HashMap::from([
                    ("title".to_owned(), value(Value::from("Set Background"))),
                    ("message".to_owned(), value(Value::from(format!("{who} wants to set this picture as {what}.")))),
                    ("picture".to_owned(), value(Value::from(path.to_string_lossy().into_owned()))),
                    ("accept".to_owned(), value(Value::from("Set"))),
                    ("cancel".to_owned(), value(Value::from("Cancel"))),
                ]);
                if dialog(&conn, options).await.is_none() {
                    return (response::CANCELLED, Results::new());
                }
            }
            match tokio::task::spawn_blocking(move || apply(&path, background, lock_screen)).await {
                Ok(Ok(uri)) => {
                    tracing::info!(uri, background, lock_screen, "background set");
                    (response::SUCCESS, Results::new())
                }
                Ok(Err(err)) => {
                    tracing::warn!(%err, "cannot set the background");
                    (response::OTHER, Results::new())
                }
                Err(_) => (response::OTHER, Results::new()),
            }
        })
        .await;
        Ok(code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn where_and_names() {
        assert_eq!(targets(None), Some((true, true)));
        assert_eq!(targets(Some("lockscreen")), Some((false, true)));
        assert_eq!(targets(Some("background")), Some((true, false)));
        assert_eq!(targets(Some("ceiling")), None);
        assert_eq!(copy_name("2026-11-12-10-00-00", Path::new("/doc/abc/My Picture.jpg")), "2026-11-12-10-00-00-My Picture.jpg");
    }
}
