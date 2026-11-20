//! Desktop icons (helixdesktop/spec `contracts/gsettings/background.md`,
//! "Desktop icons"): with `org.gnome.desktop.background
//! show-desktop-icons`, the files of the desktop folder are shown on the
//! primary monitor's background.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::icons::IconSource;

pub const BACKGROUND: &str = "org.gnome.desktop.background";

/// One file shown on the desktop.
#[derive(Debug, Clone, PartialEq)]
pub struct DesktopItem {
    pub path: PathBuf,
    pub name: String,
    pub icon: IconSource,
    /// A folder (sorted first).
    pub folder: bool,
    /// A trusted, executable `.desktop` launcher of an application:
    /// opening it starts the application.
    pub launcher: bool,
}

/// Cell size and the margin around the grid (logical pixels).
pub const CELL_W: i32 = 96;
pub const CELL_H: i32 = 96;
pub const MARGIN: i32 = 12;
pub const ICON: i32 = 48;

/// The desktop folder (XDG `DESKTOP`), unless it is the home folder
/// itself (then nothing is shown).
pub fn folder() -> Option<PathBuf> {
    let dir = gio::glib::user_special_dir(gio::glib::UserDirectory::Desktop)?;
    (dir != gio::glib::home_dir()).then_some(dir)
}

/// Whether the file is marked trusted to launch: `metadata::trusted` is
/// "true", as a file manager's "Allow Launching" (or `gio set FILE
/// metadata::trusted true`) leaves it, as in GNOME.
pub fn marked_trusted(path: &Path) -> bool {
    use gio::prelude::*;
    let info = gio::File::for_path(path).query_info("metadata::trusted", gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS, gio::Cancellable::NONE);
    info.ok().and_then(|i| i.attribute_as_string("metadata::trusted")).is_some_and(|v| v == "true")
}

/// An application's desktop entry (`Type=Application`, with a `Name`):
/// its name and icon.
fn application_entry(path: &Path) -> Option<(String, Option<IconSource>)> {
    let file = gio::glib::KeyFile::new();
    file.load_from_file(path, gio::glib::KeyFileFlags::NONE).ok()?;
    const GROUP: &str = "Desktop Entry";
    let name = file.locale_string(GROUP, "Name", None).ok()?.to_string();
    let icon = file.locale_string(GROUP, "Icon", None).ok().and_then(|i| IconSource::from_string(&i));
    file.string(GROUP, "Type").is_ok_and(|t| t == "Application").then_some((name, icon))
}

/// What a `.desktop` launcher shows, name and icon, if it launches: an
/// application's entry, executable, not writable by others (as Desktop
/// Icons NG) and trusted. Others (an archive keeps the executable bit,
/// and anyone can write any name and icon) show as plain files, under
/// their own file name, as in GNOME.
fn launcher(path: &Path, trusted: &dyn Fn(&Path) -> bool) -> Option<(String, Option<IconSource>)> {
    let entry = application_entry(path)?;
    use std::os::unix::fs::PermissionsExt;
    let executable = std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0 && m.permissions().mode() & 0o002 == 0);
    (executable && trusted(path)).then_some(entry)
}

/// Whether the `.desktop` file at `path` launches its application when
/// opened: an application's entry, executable, not writable by others
/// and marked trusted (agents' `Open` follows the same rule,
/// helixdesktop/desktop#147).
pub fn trusted_launcher(path: &Path) -> bool {
    launcher(path, &marked_trusted).is_some()
}

/// Start the application of the `.desktop` file at `path` (looked at
/// with [`trusted_launcher`] first).
pub fn launch_file(path: &Path) -> Result<(), String> {
    // gio's own tool starts a desktop file's application.
    std::process::Command::new("gio").arg("launch").arg(path).spawn().map(drop).map_err(|e| e.to_string())
}

/// What a file's menu offers about launching it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Launching {
    /// "Allow Launching": it does not launch now.
    Allow,
    /// "Don't Allow Launching": it launches now.
    Disallow,
}

/// The user's ID.
pub fn uid() -> u32 {
    // SAFETY: getuid has no preconditions.
    unsafe { libc::getuid() }
}

/// What the menu of the file at `path` offers about launching it, as
/// Desktop Icons NG: only for an application's `.desktop` file right in
/// the desktop folder `dir`, a regular file (not a link) owned by user
/// `uid` that others cannot write.
pub fn launching(path: &Path, dir: &Path, uid: u32) -> Option<Launching> {
    launching_with(path, dir, uid, &marked_trusted)
}

fn launching_with(path: &Path, dir: &Path, uid: u32, trusted: &dyn Fn(&Path) -> bool) -> Option<Launching> {
    use std::os::unix::fs::MetadataExt;
    if path.parent() != Some(dir) || path.extension().is_none_or(|e| e != "desktop") {
        return None;
    }
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() || meta.uid() != uid || meta.mode() & 0o002 != 0 {
        return None;
    }
    application_entry(path)?;
    Some(if launcher(path, trusted).is_some() { Launching::Disallow } else { Launching::Allow })
}

/// Let the file at `path` launch, or stop it, as Desktop Icons NG's
/// "Allow Launching" and "Don't Allow Launching": `metadata::trusted`
/// becomes "true" or "false"; allowing also makes the file executable
/// for its owner when it is not (the executable bit is never taken away:
/// who set it is not known).
pub fn set_launching(path: &Path, allow: bool) -> Result<(), String> {
    set_launching_with(path, allow, &|path, value| {
        use gio::prelude::*;
        let file = gio::File::for_path(path);
        file.set_attribute_string("metadata::trusted", value, gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS, gio::Cancellable::NONE).map_err(|e| e.to_string())
    })
}

fn set_launching_with(path: &Path, allow: bool, mark: &dyn Fn(&Path, &str) -> Result<(), String>) -> Result<(), String> {
    mark(path, if allow { "true" } else { "false" })?;
    if allow {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?.permissions();
        if permissions.mode() & 0o100 == 0 {
            permissions.set_mode(permissions.mode() | 0o100);
            std::fs::set_permissions(path, permissions).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// The theme icon names for a file called `name` (by its guessed type).
fn type_icon(name: &str) -> Vec<String> {
    use gio::prelude::*;
    let (content_type, _) = gio::content_type_guess(Some(name), None);
    let icon = gio::content_type_get_icon(&content_type);
    let mut names: Vec<String> = icon.downcast_ref::<gio::ThemedIcon>().map(|t| t.names().iter().map(|n| n.to_string()).collect()).unwrap_or_default();
    names.push("text-x-generic".into());
    names
}

/// The items of folder `dir`: hidden (`.name`) and backup (`name~`)
/// files left out; folders first, then by name ignoring case.
pub fn list(dir: &Path) -> Vec<DesktopItem> {
    list_with(dir, &marked_trusted)
}

/// [`list`], with `trusted` telling which launchers are trusted.
fn list_with(dir: &Path, trusted: &dyn Fn(&Path) -> bool) -> Vec<DesktopItem> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut items: Vec<DesktopItem> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let file_name = entry.file_name().to_string_lossy().into_owned();
            if file_name.starts_with('.') || file_name.ends_with('~') {
                return None;
            }
            let path = entry.path();
            let folder = path.is_dir();
            if folder {
                return Some(DesktopItem { path, name: file_name, icon: IconSource::Names(vec!["folder".into()]), folder, launcher: false });
            }
            if file_name.ends_with(".desktop") {
                if let Some((name, icon)) = launcher(&path, trusted) {
                    let icon = icon.unwrap_or_else(|| IconSource::Names(vec!["application-x-executable".into()]));
                    return Some(DesktopItem { path, name, icon, folder, launcher: true });
                }
            }
            Some(DesktopItem { icon: IconSource::Names(type_icon(&file_name)), path, name: file_name, folder, launcher: false })
        })
        .collect();
    items.sort_by_key(|i| (!i.folder, i.name.to_lowercase()));
    items
}

/// Where the cells of `count` items go in an area `w` × `h`: top-left
/// corners, filling columns top to bottom from the left; items that do
/// not fit are left out.
pub fn cells(count: usize, w: i32, h: i32) -> Vec<(i32, i32)> {
    let rows = ((h - 2 * MARGIN) / CELL_H).max(0) as usize;
    let columns = ((w - 2 * MARGIN) / CELL_W).max(0) as usize;
    (0..count.min(rows * columns)).map(|i| (MARGIN + (i / rows) as i32 * CELL_W, MARGIN + (i % rows) as i32 * CELL_H)).collect()
}

/// Two presses this close (milliseconds) on one item open it.
pub const DOUBLE_CLICK_MS: u32 = 400;

/// Open `item`: start the application of a launcher, else open the file
/// with its default application.
pub fn open(item: &DesktopItem) {
    use gio::prelude::*;
    tracing::info!(path = %item.path.display(), launcher = item.launcher, "opening a desktop item");
    // Looked at again: the file may have changed since it was listed.
    let result = if item.launcher && trusted_launcher(&item.path) {
        launch_file(&item.path)
    } else {
        let uri = gio::File::for_path(&item.path).uri();
        gio::AppInfo::launch_default_for_uri(&uri, Some(&crate::app_scope::context(None))).map_err(|e| e.to_string())
    };
    if let Err(err) = result {
        tracing::warn!(%err, path = %item.path.display(), "cannot open");
    }
}

/// Whether the setting asks for desktop icons.
fn wanted() -> bool {
    let source = gio::SettingsSchemaSource::default();
    let Some(schema) = source.and_then(|s| s.lookup(BACKGROUND, true)) else { return false };
    schema.has_key("show-desktop-icons") && helix_settings::Settings::new(BACKGROUND).boolean("show-desktop-icons")
}

/// Follow the setting and the desktop folder (looked at every two
/// seconds while shown), telling the UI the items (`None`: no icons).
pub async fn follow(ui: tokio::sync::mpsc::UnboundedSender<crate::UiEvent>) {
    let schemas: Vec<&str> = [BACKGROUND].into_iter().filter(|s| helix_settings::has_schema(s)).collect();
    let watched = helix_settings::GSettings::new(&schemas);
    let (mut changes, _watch) = watched.watch();
    let mut last: Option<Option<Vec<DesktopItem>>> = None;
    loop {
        let now = if wanted() { Some(folder().map(|d| list(&d)).unwrap_or_default()) } else { None };
        if last.as_ref() != Some(&now) {
            if ui.send(crate::UiEvent::DesktopIcons(now.clone())).is_err() {
                return;
            }
            last = Some(now.clone());
        }
        let poll = now.is_some();
        tokio::select! {
            change = changes.recv() => if change.is_none() && !poll { return },
            _ = tokio::time::sleep(Duration::from_secs(2)), if poll => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing() {
        let dir = std::env::temp_dir().join(format!("helix-desktop-icons-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Projects")).unwrap();
        std::fs::write(dir.join("notes.txt"), "x").unwrap();
        std::fs::write(dir.join(".hidden"), "x").unwrap();
        std::fs::write(dir.join("old.txt~"), "x").unwrap();
        std::fs::write(dir.join("Alpha.png"), "x").unwrap();
        let entry = "[Desktop Entry]\nType=Application\nName=Calculator\nIcon=accessories-calculator\nExec=true\n";
        std::fs::write(dir.join("calc.desktop"), entry).unwrap();
        std::fs::write(dir.join("calc2.desktop"), entry).unwrap();
        std::fs::write(dir.join("invoice.desktop"), "[Desktop Entry]\nType=Application\nName=Invoice.pdf\nIcon=application-pdf\nExec=sh -c evil\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        for f in ["calc.desktop", "invoice.desktop"] {
            std::fs::set_permissions(dir.join(f), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // Trusted: only calc.desktop (and calc2.desktop, not executable).
        let trusted = |p: &Path| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("calc"));
        let items = list_with(&dir, &trusted);
        let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["Projects", "Alpha.png", "calc2.desktop", "Calculator", "invoice.desktop", "notes.txt"]);
        assert!(items[0].folder);
        assert_eq!(items[3].icon, IconSource::Names(vec!["accessories-calculator".into()]));
        let launches: Vec<&str> = items.iter().filter(|i| i.launcher).map(|i| i.name.as_str()).collect();
        assert_eq!(launches, ["Calculator"], "only the trusted executable launcher starts its app");
        // The untrusted executable one shows its file name and the file
        // type's icon, not the PDF it claims to be.
        assert_eq!(items[4].icon, IconSource::Names(type_icon("invoice.desktop")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn allow_launching() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("helix-desktop-trust-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let entry = "[Desktop Entry]\nType=Application\nName=Calculator\nExec=true\n";
        for f in ["calc.desktop", "trusted.desktop", "open.desktop", "sub/calc.desktop"] {
            std::fs::write(dir.join(f), entry).unwrap();
        }
        std::fs::write(dir.join("link.desktop.txt"), entry).unwrap();
        std::fs::write(dir.join("broken.desktop"), "not a desktop file").unwrap();
        std::fs::write(dir.join("dir.desktop"), "[Desktop Entry]\nType=Directory\nName=X\n").unwrap();
        std::os::unix::fs::symlink(dir.join("calc.desktop"), dir.join("link.desktop")).unwrap();
        let mode = |f: &str, m: u32| std::fs::set_permissions(dir.join(f), std::fs::Permissions::from_mode(m)).unwrap();
        mode("trusted.desktop", 0o755);
        mode("open.desktop", 0o757);
        let trusted = |p: &Path| p.file_name().is_some_and(|n| n != "calc.desktop");
        let me = uid();
        let offer = |f: &str| launching_with(&dir.join(f), &dir, me, &trusted);
        assert_eq!(offer("calc.desktop"), Some(Launching::Allow), "not executable, not trusted");
        assert_eq!(offer("trusted.desktop"), Some(Launching::Disallow));
        assert_eq!(offer("open.desktop"), None, "others may write it");
        assert_eq!(launching_with(&dir.join("sub/calc.desktop"), &dir, me, &trusted), None, "not right in the desktop folder");
        assert_eq!(offer("link.desktop"), None, "a link");
        assert_eq!(offer("link.desktop.txt"), None);
        assert_eq!(offer("broken.desktop"), None);
        assert_eq!(offer("dir.desktop"), None, "not an application");
        assert_eq!(launching_with(&dir.join("trusted.desktop"), &dir, me + 1, &trusted), None, "someone else's file");
        // The world-writable one does not launch even when trusted.
        let items = list_with(&dir, &trusted);
        assert!(items.iter().any(|i| i.name == "open.desktop" && !i.launcher));
        assert!(items.iter().any(|i| i.name == "Calculator" && i.launcher));

        // Allowing marks it and makes it executable for its owner;
        // disallowing only takes the mark away.
        let marks = std::cell::RefCell::new(Vec::new());
        let mark = |p: &Path, v: &str| {
            marks.borrow_mut().push(format!("{} {v}", p.file_name().unwrap().to_string_lossy()));
            Ok(())
        };
        mode("calc.desktop", 0o640);
        set_launching_with(&dir.join("calc.desktop"), true, &mark).unwrap();
        assert_eq!(std::fs::metadata(dir.join("calc.desktop")).unwrap().permissions().mode() & 0o777, 0o740);
        set_launching_with(&dir.join("calc.desktop"), false, &mark).unwrap();
        assert_eq!(std::fs::metadata(dir.join("calc.desktop")).unwrap().permissions().mode() & 0o777, 0o740);
        assert_eq!(*marks.borrow(), ["calc.desktop true", "calc.desktop false"]);
        // A failed mark leaves the mode alone.
        mode("calc.desktop", 0o644);
        assert!(set_launching_with(&dir.join("calc.desktop"), true, &|_, _| Err("no metadata".into())).is_err());
        assert_eq!(std::fs::metadata(dir.join("calc.desktop")).unwrap().permissions().mode() & 0o777, 0o644);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn grid() {
        // Two rows of 96 in 2 × 12 + 200.
        let c = cells(5, 2 * MARGIN + 2 * CELL_W, 2 * MARGIN + 200);
        assert_eq!(c, [(12, 12), (12, 108), (108, 12), (108, 108)], "columns first; what does not fit is left out");
        assert!(cells(3, 10, 10).is_empty());
    }
}
