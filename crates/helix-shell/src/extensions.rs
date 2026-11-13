//! `org.gnome.Shell.Extensions` (helix-desktop-spec
//! `contracts/dbus/org.gnome.Shell.Extensions.md`): Helix lists the
//! installed GNOME Shell extensions and keeps the `org.gnome.shell`
//! extension keys, so the extension tools work and a GNOME session of the
//! same user sees the same choices, but it runs no extension.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Value};

pub const SHELL_VERSION: &str = "51.0";
/// What an enabled extension reports as its error.
pub const NOT_RUN: &str = "Helix does not run GNOME Shell extensions";

pub mod kind {
    pub const SYSTEM: u8 = 1;
    pub const PER_USER: u8 = 2;
}

pub mod state {
    pub const INACTIVE: f64 = 2.0;
    pub const ERROR: f64 = 3.0;
    pub const OUT_OF_DATE: f64 = 4.0;
    pub const UNINSTALLED: f64 = 99.0;
}

/// An installed extension.
#[derive(Debug, Clone, PartialEq)]
pub struct Extension {
    pub uuid: String,
    pub path: PathBuf,
    pub kind: u8,
    pub metadata: serde_json::Map<String, serde_json::Value>,
}

/// The `org.gnome.shell` keys that decide an extension's state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Keys {
    pub enabled: Vec<String>,
    pub disabled: Vec<String>,
    pub user_disabled: bool,
    pub no_version_check: bool,
}

/// The extension in `dir` (named by its UUID), if its `metadata.json`
/// is valid: an object with `uuid` (the directory's name), `name`,
/// `description` and `shell-version`.
pub fn read(dir: &Path, kind: u8) -> Option<Extension> {
    let text = std::fs::read_to_string(dir.join("metadata.json")).ok()?;
    let serde_json::Value::Object(metadata) = serde_json::from_str(&text).ok()? else { return None };
    let uuid = metadata.get("uuid")?.as_str()?.to_owned();
    if dir.file_name()?.to_str()? != uuid || metadata.get("name")?.as_str().is_none() || metadata.get("description")?.as_str().is_none() || !metadata.get("shell-version")?.is_array() {
        return None;
    }
    Some(Extension { uuid, path: dir.to_owned(), kind, metadata })
}

/// The extensions under `user` (per-user) and `system` (each
/// `<data dir>/gnome-shell/extensions`, earlier ones first); a per-user
/// one hides a system one of the same UUID.
pub fn scan(user: &Path, system: &[PathBuf]) -> BTreeMap<String, Extension> {
    let mut found = BTreeMap::new();
    let mut add = |root: &Path, kind: u8| {
        let Ok(entries) = std::fs::read_dir(root) else { return };
        let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        dirs.sort();
        for dir in dirs {
            if let Some(ext) = read(&dir, kind) {
                found.entry(ext.uuid.clone()).or_insert(ext);
            }
        }
    };
    add(user, kind::PER_USER);
    for root in system {
        add(root, kind::SYSTEM);
    }
    found
}

fn user_root() -> PathBuf {
    gio::glib::user_data_dir().join("gnome-shell/extensions")
}

fn system_roots() -> Vec<PathBuf> {
    gio::glib::system_data_dirs().into_iter().map(|d| d.join("gnome-shell/extensions")).collect()
}

/// Whether `versions` (`shell-version`) names GNOME 51.
pub fn supports(versions: &[String]) -> bool {
    let major = SHELL_VERSION.split('.').next().unwrap_or_default();
    versions.iter().any(|v| v.split('.').next() == Some(major))
}

fn versions(ext: &Extension) -> Vec<String> {
    ext.metadata.get("shell-version").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default()
}

/// Whether `ext` is enabled by `keys`.
pub fn enabled(ext: &Extension, keys: &Keys) -> bool {
    keys.enabled.contains(&ext.uuid) && !keys.disabled.contains(&ext.uuid) && !(ext.kind == kind::PER_USER && keys.user_disabled)
}

/// Its state and error.
pub fn state_of(ext: &Extension, keys: &Keys) -> (f64, &'static str) {
    if !enabled(ext, keys) {
        (state::INACTIVE, "")
    } else if !keys.no_version_check && !supports(&versions(ext)) {
        (state::OUT_OF_DATE, "")
    } else {
        (state::ERROR, NOT_RUN)
    }
}

fn value(v: Value<'_>) -> OwnedValue {
    OwnedValue::try_from(v).expect("plain values convert")
}

pub type Info = HashMap<String, OwnedValue>;

/// Its info dictionary: the metadata's text, number, flag and text-list
/// members, then `uuid`, `type`, `state`, `path`, `error`, `hasPrefs`,
/// `hasUpdate`, `canChange` and `enabled`.
pub fn info(ext: &Extension, keys: &Keys) -> Info {
    let mut out = Info::new();
    for (key, v) in &ext.metadata {
        let converted = match v {
            serde_json::Value::String(s) => Some(Value::from(s.clone())),
            serde_json::Value::Number(n) => n.as_f64().map(Value::from),
            serde_json::Value::Bool(b) => Some(Value::from(*b)),
            serde_json::Value::Array(a) if a.iter().all(|x| x.is_string()) => Some(Value::from(a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect::<Vec<_>>())),
            _ => None,
        };
        if let Some(c) = converted {
            out.insert(key.clone(), value(c));
        }
    }
    let (state, error) = state_of(ext, keys);
    out.insert("uuid".into(), value(Value::from(ext.uuid.clone())));
    out.insert("type".into(), value(Value::from(f64::from(ext.kind))));
    out.insert("state".into(), value(Value::from(state)));
    out.insert("path".into(), value(Value::from(ext.path.to_string_lossy().into_owned())));
    out.insert("error".into(), value(Value::from(error)));
    out.insert("hasPrefs".into(), value(Value::from(ext.path.join("prefs.js").is_file())));
    out.insert("hasUpdate".into(), value(Value::from(false)));
    out.insert("canChange".into(), value(Value::from(!(ext.kind == kind::PER_USER && keys.user_disabled))));
    out.insert("enabled".into(), value(Value::from(enabled(ext, keys))));
    out
}

/// `enabled` / `disabled` after enabling (`on`) or disabling `uuid`.
pub fn switched(keys: &Keys, uuid: &str, on: bool) -> (Vec<String>, Vec<String>) {
    let mut enabled: Vec<String> = keys.enabled.iter().filter(|u| *u != uuid).cloned().collect();
    let disabled: Vec<String> = keys.disabled.iter().filter(|u| *u != uuid).cloned().collect();
    if on {
        enabled.push(uuid.to_owned());
    }
    (enabled, disabled)
}

const SCHEMA: &str = "org.gnome.shell";

fn settings() -> Option<gio::Settings> {
    let schema = gio::SettingsSchemaSource::default()?.lookup(SCHEMA, true)?;
    schema.has_key("enabled-extensions").then(|| gio::Settings::new(SCHEMA))
}

fn has(s: &gio::Settings, key: &str) -> bool {
    use gio::prelude::*;
    s.settings_schema().is_some_and(|x| x.has_key(key))
}

fn keys() -> Keys {
    use gio::prelude::*;
    let Some(s) = settings() else { return Keys::default() };
    let strv = |k: &str| if has(&s, k) { s.strv(k).iter().map(|v| v.to_string()).collect() } else { Vec::new() };
    let flag = |k: &str| has(&s, k) && s.boolean(k);
    Keys { enabled: strv("enabled-extensions"), disabled: strv("disabled-extensions"), user_disabled: flag("disable-user-extensions"), no_version_check: flag("disable-extension-version-validation") }
}

fn store(enabled: &[String], disabled: &[String]) -> bool {
    use gio::prelude::*;
    let Some(s) = settings() else { return false };
    fn refs(l: &[String]) -> Vec<&str> {
        l.iter().map(String::as_str).collect()
    }
    let mut ok = s.set_strv("enabled-extensions", refs(enabled).as_slice()).is_ok();
    if has(&s, "disabled-extensions") {
        ok &= s.set_strv("disabled-extensions", refs(disabled).as_slice()).is_ok();
    }
    gio::Settings::sync();
    ok
}

fn not_supported(what: &str) -> zbus::fdo::Error {
    zbus::fdo::Error::NotSupported(format!("{what}: {NOT_RUN}"))
}

/// The interface, on the shell's object.
pub struct Extensions;

impl Extensions {
    fn find(uuid: &str) -> Option<Extension> {
        scan(&user_root(), &system_roots()).remove(uuid)
    }

    async fn changed(emitter: &SignalEmitter<'_>, ext: &Extension, keys: &Keys) {
        let info = info(ext, keys);
        let (state, error) = state_of(ext, keys);
        let _ = Self::extension_state_changed(emitter, &ext.uuid, info).await;
        let _ = Self::extension_status_changed(emitter, &ext.uuid, state as i32, error).await;
    }

    async fn switch(uuid: &str, on: bool, emitter: &SignalEmitter<'_>) -> bool {
        let Some(ext) = Self::find(uuid) else { return false };
        let (enabled, disabled) = switched(&keys(), uuid, on);
        if !store(&enabled, &disabled) {
            return false;
        }
        tracing::info!(uuid, on, "extension switched (not run by Helix)");
        Self::changed(emitter, &ext, &keys()).await;
        true
    }
}

#[zbus::interface(name = "org.gnome.Shell.Extensions")]
impl Extensions {
    fn list_extensions(&self) -> HashMap<String, Info> {
        let keys = keys();
        scan(&user_root(), &system_roots()).into_iter().map(|(uuid, ext)| (uuid, info(&ext, &keys))).collect()
    }

    fn get_extension_info(&self, uuid: String) -> Info {
        Self::find(&uuid).map(|ext| info(&ext, &keys())).unwrap_or_default()
    }

    fn get_extension_errors(&self, uuid: String) -> Vec<String> {
        Self::find(&uuid).map(|ext| state_of(&ext, &keys()).1).filter(|e| !e.is_empty()).map(|e| vec![e.to_owned()]).unwrap_or_default()
    }

    fn install_remote_extension(&self, _uuid: String) -> zbus::fdo::Result<String> {
        Err(not_supported("installing from extensions.gnome.org"))
    }

    async fn uninstall_extension(&self, uuid: String, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) -> bool {
        let Some(ext) = Self::find(&uuid) else { return false };
        if ext.kind != kind::PER_USER {
            return false;
        }
        if let Err(err) = std::fs::remove_dir_all(&ext.path) {
            tracing::warn!(uuid, %err, "cannot uninstall the extension");
            return false;
        }
        let (enabled, disabled) = switched(&keys(), &uuid, false);
        store(&enabled, &disabled);
        tracing::info!(uuid, "extension uninstalled");
        let mut gone = info(&ext, &keys());
        gone.insert("state".into(), value(Value::from(state::UNINSTALLED)));
        let _ = Self::extension_state_changed(&emitter, &uuid, gone).await;
        let _ = Self::extension_status_changed(&emitter, &uuid, state::UNINSTALLED as i32, "").await;
        true
    }

    async fn reload_extension(&self, uuid: String, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) {
        if let Some(ext) = Self::find(&uuid) {
            Self::changed(&emitter, &ext, &keys()).await;
        }
    }

    async fn enable_extension(&self, uuid: String, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) -> bool {
        Self::switch(&uuid, true, &emitter).await
    }

    async fn disable_extension(&self, uuid: String, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) -> bool {
        Self::switch(&uuid, false, &emitter).await
    }

    fn launch_extension_prefs(&self, _uuid: String) -> zbus::fdo::Result<()> {
        Err(not_supported("extension preferences"))
    }

    fn open_extension_prefs(&self, _uuid: String, _parent_window: String, _options: HashMap<String, OwnedValue>) -> zbus::fdo::Result<()> {
        Err(not_supported("extension preferences"))
    }

    fn check_for_updates(&self) {}

    #[zbus(signal)]
    async fn extension_state_changed(emitter: &SignalEmitter<'_>, uuid: &str, state: Info) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn extension_status_changed(emitter: &SignalEmitter<'_>, uuid: &str, state: i32, error: &str) -> zbus::Result<()>;

    #[zbus(property)]
    fn shell_version(&self) -> String {
        SHELL_VERSION.into()
    }

    #[zbus(property)]
    fn user_extensions_enabled(&self) -> bool {
        !keys().user_disabled
    }

    #[zbus(property)]
    async fn set_user_extensions_enabled(&mut self, on: bool, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) {
        use gio::prelude::*;
        if let Some(s) = settings().filter(|s| has(s, "disable-user-extensions")) {
            let _ = s.set_boolean("disable-user-extensions", !on);
            gio::Settings::sync();
        }
        let keys = keys();
        for ext in scan(&user_root(), &system_roots()).values().filter(|e| e.kind == kind::PER_USER) {
            Self::changed(&emitter, ext, &keys).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, uuid: &str, json: &str) {
        let dir = root.join(uuid);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("metadata.json"), json).unwrap();
    }

    fn meta(uuid: &str, versions: &str) -> String {
        format!(r#"{{"uuid": "{uuid}", "name": "N {uuid}", "description": "D", "shell-version": [{versions}], "version": 7, "url": "https://example.org"}}"#)
    }

    #[test]
    fn scanning_and_states() {
        let tmp = std::env::temp_dir().join(format!("helix-ext-{}", std::process::id()));
        let (user, system) = (tmp.join("user"), tmp.join("system"));
        write(&user, "a@x", &meta("a@x", r#""51""#));
        write(&user, "old@x", &meta("old@x", r#""45", "46""#));
        write(&system, "a@x", &meta("a@x", r#""51""#));
        write(&system, "s@x", &meta("s@x", r#""51.1""#));
        write(&user, "wrong@x", &meta("other@x", r#""51""#));
        write(&user, "bad@x", "{not json");
        std::fs::write(user.join("a@x/prefs.js"), "").unwrap();
        let found = scan(&user, std::slice::from_ref(&system));
        assert_eq!(found.keys().collect::<Vec<_>>(), ["a@x", "old@x", "s@x"]);
        assert_eq!(found["a@x"].kind, kind::PER_USER, "the user's copy wins");
        assert_eq!(found["s@x"].kind, kind::SYSTEM);

        let keys = Keys { enabled: vec!["a@x".into(), "old@x".into(), "s@x".into()], ..Keys::default() };
        assert_eq!(state_of(&found["a@x"], &keys), (state::ERROR, NOT_RUN));
        assert_eq!(state_of(&found["old@x"], &keys).0, state::OUT_OF_DATE);
        let lax = Keys { no_version_check: true, ..keys.clone() };
        assert_eq!(state_of(&found["old@x"], &lax).0, state::ERROR);
        let off = Keys { user_disabled: true, ..keys.clone() };
        assert_eq!(state_of(&found["a@x"], &off).0, state::INACTIVE);
        assert_eq!(state_of(&found["s@x"], &off).0, state::ERROR, "system ones stay");
        let vetoed = Keys { disabled: vec!["s@x".into()], ..keys.clone() };
        assert!(!enabled(&found["s@x"], &vetoed));

        let i = info(&found["a@x"], &keys);
        assert_eq!(f64::try_from(i["version"].clone()), Ok(7.0));
        assert_eq!(String::try_from(i["url"].clone()).unwrap(), "https://example.org");
        assert_eq!(bool::try_from(i["hasPrefs"].clone()), Ok(true));
        assert_eq!(bool::try_from(i["enabled"].clone()), Ok(true));
        assert_eq!(f64::try_from(i["type"].clone()), Ok(2.0));
        assert_eq!(<Vec<String>>::try_from(i["shell-version"].clone()).unwrap(), ["51"]);

        let both = Keys { enabled: vec!["a@x".into()], disabled: vec!["b@x".into(), "a@x".into()], ..Keys::default() };
        assert_eq!(switched(&both, "a@x", true), (vec!["a@x".to_owned()], vec!["b@x".to_owned()]));
        assert_eq!(switched(&both, "a@x", false), (vec![], vec!["b@x".to_owned()]));
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
