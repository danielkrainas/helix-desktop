//! Desktop icons (helix-desktop-spec `contracts/gsettings/background.md`,
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
    /// An executable `.desktop` launcher of an application: opening it
    /// starts the application.
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

/// What a `.desktop` file shows: name, icon, whether it launches.
fn launcher(path: &Path) -> Option<(String, Option<IconSource>, bool)> {
    let file = gio::glib::KeyFile::new();
    file.load_from_file(path, gio::glib::KeyFileFlags::NONE).ok()?;
    const GROUP: &str = "Desktop Entry";
    let name = file.locale_string(GROUP, "Name", None).ok()?.to_string();
    let icon = file.locale_string(GROUP, "Icon", None).ok().and_then(|i| IconSource::from_string(&i));
    let application = file.string(GROUP, "Type").is_ok_and(|t| t == "Application");
    use std::os::unix::fs::PermissionsExt;
    let executable = std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0);
    Some((name, icon, application && executable))
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
                if let Some((name, icon, launches)) = launcher(&path) {
                    let icon = icon.unwrap_or_else(|| IconSource::Names(vec!["application-x-executable".into()]));
                    return Some(DesktopItem { path, name, icon, folder, launcher: launches });
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
    let result = if item.launcher {
        // gio's own tool starts a desktop file's application.
        std::process::Command::new("gio").arg("launch").arg(&item.path).spawn().map(drop).map_err(|e| e.to_string())
    } else {
        let uri = gio::File::for_path(&item.path).uri();
        gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>).map_err(|e| e.to_string())
    };
    if let Err(err) = result {
        tracing::warn!(%err, path = %item.path.display(), "cannot open");
    }
}

/// Whether the setting asks for desktop icons.
fn wanted() -> bool {
    use gio::prelude::*;
    let source = gio::SettingsSchemaSource::default();
    let Some(schema) = source.and_then(|s| s.lookup(BACKGROUND, true)) else { return false };
    schema.has_key("show-desktop-icons") && gio::Settings::new(BACKGROUND).boolean("show-desktop-icons")
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
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.join("calc.desktop"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let items = list(&dir);
        let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["Projects", "Alpha.png", "Calculator", "Calculator", "notes.txt"]);
        assert!(items[0].folder);
        assert_eq!(items[2].icon, IconSource::Names(vec!["accessories-calculator".into()]));
        let launches: Vec<bool> = items.iter().map(|i| i.launcher).collect();
        assert_eq!(launches.iter().filter(|l| **l).count(), 1, "only the executable launcher starts its app");
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
