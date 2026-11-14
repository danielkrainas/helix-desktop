//! Who the lock screen shows (helix-desktop-spec
//! `contracts/gsettings/org.gnome.desktop.screensaver.md`, "Helix"): the
//! user's real name and picture from AccountsService
//! (`org.freedesktop.Accounts.User` `RealName`, `IconFile`, read only),
//! else the real name from the GECOS field, else the login name.

use std::collections::HashMap;
use std::ffi::CStr;
use std::path::Path;

use zbus::zvariant::{OwnedObjectPath, OwnedValue};

const ACCOUNTS: &str = "org.freedesktop.Accounts";
const ACCOUNTS_PATH: &str = "/org/freedesktop/Accounts";
const USER_IFACE: &str = "org.freedesktop.Accounts.User";
/// Pictures larger than this are not read.
const MAX_PICTURE_BYTES: u64 = 8 << 20;
/// Pictures are kept this large at most (pixels), enough for the lock
/// screen's avatar at scale 2.
const PICTURE_PX: u32 = 256;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Identity {
    /// What to call the user; never empty (falls back to the login name).
    pub name: String,
    pub picture: Option<helix_draw::Image>,
}

/// The real name in a GECOS field (its first comma-separated part).
pub fn gecos_real_name(gecos: &str) -> Option<String> {
    let name = gecos.split(',').next().unwrap_or_default().trim();
    (!name.is_empty()).then(|| name.to_owned())
}

/// The process user's GECOS real name.
fn gecos() -> Option<String> {
    // SAFETY: getpwuid returns a pointer to static storage or null; the
    // field is copied at once.
    let gecos = unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if pw.is_null() || (*pw).pw_gecos.is_null() {
            return None;
        }
        CStr::from_ptr((*pw).pw_gecos).to_string_lossy().into_owned()
    };
    gecos_real_name(&gecos)
}

/// A picture file decoded and shrunk for an avatar.
pub fn load_picture(path: &Path) -> Option<helix_draw::Image> {
    let size = std::fs::metadata(path).ok()?.len();
    if size == 0 || size > MAX_PICTURE_BYTES {
        return None;
    }
    match crate::wallpaper::decode(path) {
        Ok(image) => Some(crate::wallpaper::thumbnail(&image, PICTURE_PX, PICTURE_PX)),
        Err(err) => {
            tracing::debug!(%err, "cannot read the user's picture");
            None
        }
    }
}

/// `RealName` and `IconFile` of user `login` from AccountsService.
async fn accounts_service(system: &zbus::Connection, login: &str) -> Option<(String, String)> {
    let reply = system.call_method(Some(ACCOUNTS), ACCOUNTS_PATH, Some(ACCOUNTS), "FindUserByName", &(login,)).await.ok()?;
    let path: OwnedObjectPath = reply.body().deserialize().ok()?;
    let reply = system.call_method(Some(ACCOUNTS), path.as_str(), Some("org.freedesktop.DBus.Properties"), "GetAll", &(USER_IFACE,)).await.ok()?;
    let props: HashMap<String, OwnedValue> = reply.body().deserialize().ok()?;
    let string = |key: &str| props.get(key).and_then(|v| String::try_from(v.clone()).ok()).unwrap_or_default();
    Some((string("RealName"), string("IconFile")))
}

/// Who `login` is: AccountsService's real name and picture when it
/// answers, else GECOS, else the login name.
pub async fn read(system: Option<&zbus::Connection>, login: &str) -> Identity {
    let (real, icon) = match system {
        Some(system) => accounts_service(system, login).await.unwrap_or_default(),
        None => Default::default(),
    };
    let name = Some(real.trim().to_owned()).filter(|n| !n.is_empty()).or_else(gecos).unwrap_or_else(|| login.to_owned());
    let picture = if icon.is_empty() { None } else { tokio::task::spawn_blocking(move || load_picture(Path::new(&icon))).await.ok().flatten() };
    Identity { name, picture }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gecos_names() {
        assert_eq!(gecos_real_name("Ann Smith,,,"), Some("Ann Smith".into()));
        assert_eq!(gecos_real_name(",,,"), None);
        assert_eq!(gecos_real_name(""), None);
    }

    #[test]
    fn pictures() {
        let dir = std::env::temp_dir().join(format!("helix-identity-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A 4x2 red PNG.
        let path = dir.join("face.png");
        {
            let file = std::fs::File::create(&path).unwrap();
            let mut encoder = png::Encoder::new(file, 4, 2);
            encoder.set_color(png::ColorType::Rgba);
            encoder.write_header().unwrap().write_image_data(&[255, 0, 0, 255].repeat(8)).unwrap();
        }
        let picture = load_picture(&path).expect("decoded");
        assert_eq!((picture.width, picture.height), (4, 2));
        assert_eq!(load_picture(&dir.join("missing.png")), None);
        std::fs::write(dir.join("empty.png"), b"").unwrap();
        assert_eq!(load_picture(&dir.join("empty.png")), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Read-only, against the host's system bus; skipped without one.
    #[test]
    fn host_identity() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let system = zbus::Connection::system().await.ok();
            let login = crate::auth::current_user();
            let identity = read(system.as_ref(), &login).await;
            assert!(!identity.name.is_empty());
        });
    }
}
