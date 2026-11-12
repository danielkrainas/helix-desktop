//! `org.freedesktop.impl.portal.DynamicLauncher` (helix-desktop-spec
//! `contracts/dbus/org.freedesktop.impl.portal.md`, "DynamicLauncher"):
//! the user sees a launcher's icon and name (and, for a web app, its
//! address) before an app adds it. The frontend writes the desktop file.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};

use zbus::zvariant::{ObjectPath, OwnedValue, Value};
use zbus::{fdo, interface, Connection, ObjectServer};

use crate::dialogs::{app_name, dialog, value};
use crate::screenshot::{response, with_request, Results};

/// `launcher_type` values.
pub mod launcher_type {
    pub const APPLICATION: u32 = 1;
    pub const WEBAPP: u32 = 2;
}

/// The longest name the dialog accepts (longer ones are cut).
pub const MAX_NAME: usize = 255;

/// The bytes of a serialized `GBytesIcon` (`('bytes', <ay>)`).
pub fn icon_bytes(icon: &Value<'_>) -> Option<Vec<u8>> {
    let mut v = icon;
    while let Value::Value(inner) = v {
        v = inner.as_ref();
    }
    let Value::Structure(s) = v else { return None };
    let [Value::Str(kind), data] = s.fields() else { return None };
    if kind.as_str() != "bytes" {
        return None;
    }
    let mut data = data;
    while let Value::Value(inner) = data {
        data = inner.as_ref();
    }
    Vec::<u8>::try_from(data.try_clone().ok()?).ok()
}

/// Largest icon file the user may pick.
pub const MAX_ICON: u64 = 4 << 20;

/// Whether `bytes` look like a PNG, JPEG or SVG image (what the frontend
/// accepts as a launcher icon).
pub fn image_kind(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpeg")
    } else if String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]).contains("<svg") {
        Some("svg")
    } else {
        None
    }
}

/// A picked icon file as a serialized `GBytesIcon`, if it is an image
/// that is not too large.
pub fn icon_from_file(path: &std::path::Path) -> Result<OwnedValue, String> {
    let size = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
    if size > MAX_ICON {
        return Err("the file is too large".into());
    }
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    image_kind(&bytes).ok_or("not a PNG, JPEG or SVG image")?;
    OwnedValue::try_from(Value::from(("bytes", Value::from(bytes)))).map_err(|e| e.to_string())
}

/// The name to return: the user's (trimmed, on one line, at most
/// [`MAX_NAME`] characters), else the default.
pub fn chosen_name(entered: Option<&str>, default: &str) -> String {
    let clean = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(MAX_NAME).collect::<String>();
    entered.map(clean).filter(|s| !s.is_empty()).unwrap_or_else(|| clean(default))
}

/// The dialog's message.
pub fn message(who: &str, kind: u32, editable: bool, name: &str) -> String {
    let what = if kind == launcher_type::WEBAPP { "web app" } else { "app" };
    if editable {
        format!("{who} wants to add a launcher for this {what} to your apps.")
    } else {
        format!("{who} wants to add a launcher for the {what} “{name}” to your apps.")
    }
}

static NEXT_ICON: AtomicU32 = AtomicU32::new(0);

/// A file holding the icon for the shell to show; removed on drop.
struct IconFile(std::path::PathBuf);

impl IconFile {
    fn write(bytes: &[u8]) -> Option<Self> {
        let dir = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir).join("helix-portals");
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join(format!("launcher-icon-{}-{}", std::process::id(), NEXT_ICON.fetch_add(1, Ordering::Relaxed)));
        std::fs::write(&path, bytes).ok()?;
        Some(Self(path))
    }
}

impl Drop for IconFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub struct DynamicLauncherBackend {
    pub conn: Connection,
}

#[interface(name = "org.freedesktop.impl.portal.DynamicLauncher")]
impl DynamicLauncherBackend {
    #[allow(clippy::too_many_arguments)]
    async fn prepare_install(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        handle: ObjectPath<'_>,
        app_id: String,
        _parent_window: String,
        name: String,
        icon_v: OwnedValue,
        options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<(u32, Results)> {
        let get_bool = |k: &str, d: bool| options.get(k).and_then(|v| bool::try_from(v.clone()).ok()).unwrap_or(d);
        let editable = get_bool("editable_name", true);
        let icon_editable = get_bool("editable_icon", false);
        let kind = options.get("launcher_type").and_then(|v| u32::try_from(v.clone()).ok()).unwrap_or(launcher_type::APPLICATION);
        let target = options.get("target").and_then(|v| String::try_from(v.clone()).ok()).filter(|s| !s.is_empty());
        let icon = icon_bytes(&icon_v);
        let conn = self.conn.clone();
        tracing::info!(app_id, name, kind, editable, icon = icon.as_ref().map(Vec::len), "prepare launcher install");
        Ok(with_request(server, handle, || async move {
            if kind != launcher_type::APPLICATION && kind != launcher_type::WEBAPP {
                return (response::OTHER, Results::new());
            }
            let who = if app_id.is_empty() { "An app".to_owned() } else { app_name(&app_id) };
            let default = chosen_name(None, &name);
            let mut dialog_options = HashMap::from([
                ("title".to_owned(), value(Value::from("Add to Apps"))),
                ("message".to_owned(), value(Value::from(message(&who, kind, editable, &default)))),
                ("accept".to_owned(), value(Value::from("Add"))),
                ("cancel".to_owned(), value(Value::from("Cancel"))),
            ]);
            if kind == launcher_type::WEBAPP {
                if let Some(target) = &target {
                    dialog_options.insert("description".to_owned(), value(Value::from(target.clone())));
                }
            }
            if editable {
                dialog_options.insert("entry-label".to_owned(), value(Value::from("Name")));
                dialog_options.insert("entry".to_owned(), value(Value::from(default.clone())));
            }
            if icon_editable {
                dialog_options.insert("check-label".to_owned(), value(Value::from("Choose a different icon")));
                dialog_options.insert("check".to_owned(), value(Value::from(false)));
            }
            let icon_file = icon.as_deref().and_then(IconFile::write);
            if let Some(file) = &icon_file {
                dialog_options.insert("picture".to_owned(), value(Value::from(file.0.to_string_lossy().into_owned())));
            }
            let answer = dialog(&conn, dialog_options).await;
            drop(icon_file);
            let Some(results) = answer else {
                return (response::CANCELLED, Results::new());
            };
            let entered = results.get("entry").and_then(|v| String::try_from(v.clone()).ok());
            let chosen = if editable { chosen_name(entered.as_deref(), &default) } else { default };
            tracing::info!(name = chosen, "launcher install accepted");
            let mut out = Results::new();
            out.insert("name".to_owned(), value(Value::from(chosen)));
            let other_icon = icon_editable && results.get("check").and_then(|v| bool::try_from(v.clone()).ok()).unwrap_or(false);
            let picked = match other_icon {
                true => crate::file_chooser::pick_image(&conn, "Choose an Icon").await,
                false => None,
            };
            let icon_v = match picked.map(|p| (icon_from_file(&p), p)) {
                Some((Ok(icon), path)) => {
                    tracing::info!(path = %path.display(), "launcher icon chosen");
                    icon
                }
                Some((Err(err), path)) => {
                    tracing::warn!(err, path = %path.display(), "cannot use that icon; keeping the app's");
                    icon_v
                }
                None => icon_v,
            };
            out.insert("icon".to_owned(), icon_v);
            (response::SUCCESS, out)
        })
        .await)
    }

    /// No app may add launchers without asking (see the spec).
    fn request_install_token(&self, app_id: String, _options: HashMap<String, OwnedValue>) -> u32 {
        tracing::info!(app_id, "install token refused");
        response::OTHER
    }

    #[zbus(property)]
    fn supported_launcher_types(&self) -> u32 {
        launcher_type::APPLICATION | launcher_type::WEBAPP
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::Structure;

    #[test]
    fn picked_icons() {
        assert_eq!(image_kind(b"\x89PNG\r\n\x1a\nrest"), Some("png"));
        assert_eq!(image_kind(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("jpeg"));
        assert_eq!(image_kind(b"<?xml version='1.0'?><svg xmlns='http://www.w3.org/2000/svg'/>"), Some("svg"));
        assert_eq!(image_kind(b"hello"), None);
        let dir = std::env::temp_dir().join(format!("helix-launcher-icon-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("i.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\nrest").unwrap();
        let icon = icon_from_file(&png).unwrap();
        assert_eq!(icon_bytes(&icon), Some(b"\x89PNG\r\n\x1a\nrest".to_vec()));
        std::fs::write(dir.join("t.txt"), "text").unwrap();
        assert!(icon_from_file(&dir.join("t.txt")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn icons_and_names() {
        let icon = Value::from(Structure::from(("bytes", Value::from(vec![1u8, 2, 3]))));
        assert_eq!(icon_bytes(&Value::Value(Box::new(icon))), Some(vec![1, 2, 3]));
        assert_eq!(icon_bytes(&Value::from(Structure::from(("themed", Value::from(vec!["x"]))))), None);
        assert_eq!(chosen_name(Some("  My\n App "), "Default"), "My App");
        assert_eq!(chosen_name(Some("   "), "Default"), "Default");
        assert_eq!(chosen_name(None, &"x".repeat(300)).len(), MAX_NAME);
        assert_eq!(message("Web", launcher_type::WEBAPP, false, "Maps"), "Web wants to add a launcher for the web app “Maps” to your apps.");
    }
}
