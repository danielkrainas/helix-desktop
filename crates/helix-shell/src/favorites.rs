//! Favorites and app folders (helix-desktop-spec `contracts/wayland/README.md`,
//! "Favorites and app folders").

use std::path::PathBuf;

use gio::prelude::*;

use crate::launcher::App;

const SHELL: &str = "org.gnome.shell";
const FOLDERS: &str = "org.gnome.desktop.app-folders";
const FOLDER: &str = "org.gnome.desktop.app-folders.folder";

/// An app folder: its name and its apps (ids, installed only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    pub id: String,
    pub name: String,
    pub apps: Vec<String>,
}

/// A folder's settings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FolderSpec {
    pub name: String,
    pub translate: bool,
    pub apps: Vec<String>,
    pub categories: Vec<String>,
    pub excluded: Vec<String>,
}

fn settings(schema: &str) -> Option<gio::Settings> {
    gio::SettingsSchemaSource::default()?.lookup(schema, true).map(|_| gio::Settings::new(schema))
}

fn strv(s: &gio::Settings, key: &str) -> Vec<String> {
    s.strv(key).iter().map(|v| v.to_string()).collect()
}

fn data_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![gio::glib::user_data_dir()];
    dirs.extend(gio::glib::system_data_dirs());
    dirs
}

/// The `Categories` of desktop file `id`.
pub fn categories(id: &str) -> Vec<String> {
    for dir in data_dirs() {
        if let Ok(text) = std::fs::read_to_string(dir.join("applications").join(id)) {
            return text
                .lines()
                .find_map(|l| l.strip_prefix("Categories="))
                .map(|c| c.split(';').filter(|c| !c.is_empty()).map(str::to_owned).collect())
                .unwrap_or_default();
        }
    }
    Vec::new()
}

/// The `Name` of a desktop directory entry.
fn directory_name(file: &str) -> Option<String> {
    data_dirs().into_iter().find_map(|dir| {
        let text = std::fs::read_to_string(dir.join("desktop-directories").join(file)).ok()?;
        text.lines().find_map(|l| l.strip_prefix("Name=")).map(str::to_owned)
    })
}

/// The favorites (desktop file ids, in order).
pub fn favorites() -> Vec<String> {
    settings(SHELL).filter(|s| s.settings_schema().is_some_and(|x| x.has_key("favorite-apps"))).map(|s| strv(&s, "favorite-apps")).unwrap_or_default()
}

/// Replace the favorites (desktop file ids, in order).
pub fn set_favorites(list: &[String]) {
    let Some(s) = settings(SHELL).filter(|s| s.settings_schema().is_some_and(|x| x.has_key("favorite-apps"))) else { return };
    let refs: Vec<&str> = list.iter().map(String::as_str).collect();
    let _ = s.set_strv("favorite-apps", refs.as_slice());
    gio::Settings::sync();
}

/// Add `id` to the favorites, or remove it; true when it is one now.
pub fn toggle_favorite(id: &str) -> bool {
    let Some(s) = settings(SHELL).filter(|s| s.settings_schema().is_some_and(|x| x.has_key("favorite-apps"))) else { return false };
    let mut list = strv(&s, "favorite-apps");
    let now = if let Some(i) = list.iter().position(|f| f == id) {
        list.remove(i);
        false
    } else {
        list.push(id.to_owned());
        true
    };
    let refs: Vec<&str> = list.iter().map(String::as_str).collect();
    let _ = s.set_strv("favorite-apps", refs.as_slice());
    gio::Settings::sync();
    now
}

/// Which installed apps a folder holds, sorted by name.
pub fn members(spec: &FolderSpec, apps: &[App]) -> Vec<String> {
    let mut found: Vec<&App> = apps
        .iter()
        .filter(|a| !spec.excluded.contains(&a.id))
        .filter(|a| spec.apps.contains(&a.id) || a.categories.iter().any(|c| spec.categories.contains(c)))
        .collect();
    found.sort_by_key(|a| a.name.to_lowercase());
    found.into_iter().map(|a| a.id.clone()).collect()
}

/// The folder's shown name.
pub fn folder_name(spec: &FolderSpec) -> String {
    if spec.translate {
        if let Some(name) = directory_name(&spec.name) {
            return name;
        }
    }
    spec.name.clone()
}

/// The app folders with installed apps.
pub fn folders(apps: &[App]) -> Vec<Folder> {
    let Some(parent) = settings(FOLDERS) else { return Vec::new() };
    let Some(schema) = gio::SettingsSchemaSource::default().and_then(|s| s.lookup(FOLDER, true)) else { return Vec::new() };
    strv(&parent, "folder-children")
        .into_iter()
        .filter_map(|id| {
            let path = format!("/org/gnome/desktop/app-folders/folders/{id}/");
            let s = gio::Settings::new_full(&schema, None::<&gio::SettingsBackend>, Some(path.as_str()));
            let spec = FolderSpec { name: s.string("name").to_string(), translate: s.boolean("translate"), apps: strv(&s, "apps"), categories: strv(&s, "categories"), excluded: strv(&s, "excluded-apps") };
            let apps = members(&spec, apps);
            (!apps.is_empty()).then(|| Folder { id: id.clone(), name: if spec.name.is_empty() { id } else { folder_name(&spec) }, apps })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(id: &str, name: &str, categories: &[&str]) -> App {
        App { id: id.into(), name: name.into(), search: String::new(), categories: categories.iter().map(|c| c.to_string()).collect() }
    }

    #[test]
    fn membership() {
        let apps = vec![app("z.desktop", "Zed", &["Utility"]), app("a.desktop", "Atlas", &["Utility", "System"]), app("b.desktop", "Bee", &["Game"]), app("x.desktop", "Xcess", &["Utility"])];
        let spec = FolderSpec { name: "Utilities".into(), translate: false, apps: vec!["b.desktop".into()], categories: vec!["Utility".into()], excluded: vec!["x.desktop".into()] };
        assert_eq!(members(&spec, &apps), ["a.desktop", "b.desktop", "z.desktop"]);
        assert_eq!(folder_name(&spec), "Utilities");
        let translated = FolderSpec { name: "no-such.directory".into(), translate: true, ..FolderSpec::default() };
        assert_eq!(folder_name(&translated), "no-such.directory", "falls back to the name");
    }
}
