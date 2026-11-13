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

fn folder_settings(id: &str) -> Option<gio::Settings> {
    let schema = gio::SettingsSchemaSource::default()?.lookup(FOLDER, true)?;
    let path = format!("/org/gnome/desktop/app-folders/folders/{id}/");
    Some(gio::Settings::new_full(&schema, None::<&gio::SettingsBackend>, Some(path.as_str())))
}

/// A new folder `name` holding `apps`; its id.
pub fn create_folder(name: &str, apps: &[String]) -> Option<String> {
    let parent = settings(FOLDERS)?;
    let mut children = strv(&parent, "folder-children");
    let base: String = name.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    let base = if base.is_empty() { "Folder".to_owned() } else { base };
    let mut id = base.clone();
    let mut n = 1;
    while children.contains(&id) {
        n += 1;
        id = format!("{base}{n}");
    }
    let s = folder_settings(&id)?;
    let _ = s.set_string("name", name);
    let refs: Vec<&str> = apps.iter().map(String::as_str).collect();
    let _ = s.set_strv("apps", refs.as_slice());
    children.push(id.clone());
    let refs: Vec<&str> = children.iter().map(String::as_str).collect();
    let _ = parent.set_strv("folder-children", refs.as_slice());
    gio::Settings::sync();
    Some(id)
}

/// Put app `app` into folder `id` (also when its categories had left it
/// out).
pub fn add_to_folder(id: &str, app: &str) {
    let Some(s) = folder_settings(id) else { return };
    let mut apps = strv(&s, "apps");
    if !apps.iter().any(|a| a == app) {
        apps.push(app.to_owned());
    }
    let excluded: Vec<String> = strv(&s, "excluded-apps").into_iter().filter(|a| a != app).collect();
    let refs: Vec<&str> = apps.iter().map(String::as_str).collect();
    let _ = s.set_strv("apps", refs.as_slice());
    let refs: Vec<&str> = excluded.iter().map(String::as_str).collect();
    let _ = s.set_strv("excluded-apps", refs.as_slice());
    gio::Settings::sync();
}

/// Take app `app` out of folder `id` (excluded when the folder's categories
/// would bring it back); a folder left without apps (`left` of them
/// remain) is removed.
pub fn remove_from_folder(id: &str, app: &str, left: usize) {
    let Some(s) = folder_settings(id) else { return };
    if left == 0 {
        delete_folder(id);
        return;
    }
    let apps: Vec<String> = strv(&s, "apps").into_iter().filter(|a| a != app).collect();
    let refs: Vec<&str> = apps.iter().map(String::as_str).collect();
    let _ = s.set_strv("apps", refs.as_slice());
    let categories = strv(&s, "categories");
    if categories.iter().any(|c| crate::favorites::categories(app).contains(c)) {
        let mut excluded = strv(&s, "excluded-apps");
        if !excluded.iter().any(|a| a == app) {
            excluded.push(app.to_owned());
        }
        let refs: Vec<&str> = excluded.iter().map(String::as_str).collect();
        let _ = s.set_strv("excluded-apps", refs.as_slice());
    }
    gio::Settings::sync();
}

/// Rename folder `id`.
pub fn rename_folder(id: &str, name: &str) {
    if let Some(s) = folder_settings(id) {
        let _ = s.set_string("name", name);
        let _ = s.set_boolean("translate", false);
        gio::Settings::sync();
    }
}

/// Remove folder `id` (its settings reset).
pub fn delete_folder(id: &str) {
    if let Some(parent) = settings(FOLDERS) {
        let children: Vec<String> = strv(&parent, "folder-children").into_iter().filter(|c| c != id).collect();
        let refs: Vec<&str> = children.iter().map(String::as_str).collect();
        let _ = parent.set_strv("folder-children", refs.as_slice());
    }
    if let Some(s) = folder_settings(id) {
        for key in ["name", "apps", "categories", "excluded-apps", "translate"] {
            s.reset(key);
        }
    }
    gio::Settings::sync();
}

/// A name for a folder made of `a` and `b`: a category they share (the
/// first of `a`'s that is not a generic one), else "Unnamed Folder".
pub fn folder_name_for(a: &[String], b: &[String]) -> String {
    const GENERIC: [&str; 4] = ["GTK", "GNOME", "Qt", "KDE"];
    a.iter().find(|c| b.contains(c) && !GENERIC.contains(&c.as_str())).cloned().unwrap_or_else(|| "Unnamed Folder".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(id: &str, name: &str, categories: &[&str]) -> App {
        App { id: id.into(), name: name.into(), search: String::new(), categories: categories.iter().map(|c| c.to_string()).collect() }
    }

    #[test]
    fn new_folder_names() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(folder_name_for(&s(&["GTK", "Utility", "Calculator"]), &s(&["GNOME", "GTK", "Utility"])), "Utility");
        assert_eq!(folder_name_for(&s(&["Game"]), &s(&["Office"])), "Unnamed Folder");
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
