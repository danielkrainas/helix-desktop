//! Where extensions are, which are enabled and approved, their settings
//! and logs (helix-desktop-spec `contracts/extensions/README.md`).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};

use super::manifest::{self, Manifest, SettingValue};

pub const SCHEMA: &str = "org.helix.extensions";
/// Log lines kept in memory.
pub const LOG_LINES: usize = 200;
/// The log file is cut to its last half beyond this.
const LOG_FILE_MAX: u64 = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    System,
    User,
    Development,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::System => "system",
            Kind::User => "user",
            Kind::Development => "development",
        }
    }
}

/// An extension directory: its manifest, or why it is invalid.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub id: String,
    pub dir: PathBuf,
    pub kind: Kind,
    pub manifest: Result<Manifest, String>,
}

pub fn user_dir() -> PathBuf {
    gio::glib::user_data_dir().join("helix/extensions")
}

pub fn system_dirs() -> Vec<PathBuf> {
    gio::glib::system_data_dirs().into_iter().map(|d| d.join("helix/extensions")).collect()
}

/// Read the manifest in `dir`.
pub fn load(dir: &Path, kind: Kind) -> Found {
    let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let manifest = std::fs::read_to_string(dir.join("extension.toml"))
        .map_err(|e| format!("cannot read extension.toml: {e}"))
        .and_then(|source| manifest::parse(&source, (kind != Kind::Development).then_some(name.as_str())));
    let id = manifest.as_ref().map(|m| m.id.clone()).unwrap_or(name);
    Found { id, dir: dir.to_owned(), kind, manifest }
}

/// The extensions in `user` and then `system`; a per-user one hides a
/// system one with the same id.
pub fn discover(user: &Path, system: &[PathBuf]) -> BTreeMap<String, Found> {
    let mut found = BTreeMap::new();
    let roots = std::iter::once((user.to_owned(), Kind::User)).chain(system.iter().map(|d| (d.clone(), Kind::System)));
    for (root, kind) in roots {
        let Ok(entries) = std::fs::read_dir(&root) else { continue };
        let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.join("extension.toml").is_file()).collect();
        dirs.sort();
        for dir in dirs {
            let f = load(&dir, kind);
            found.entry(f.id.clone()).or_insert(f);
        }
    }
    found
}

/// `org.helix.extensions`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Keys {
    pub enabled: Vec<String>,
    pub disabled: Vec<String>,
    pub disable_all: bool,
    pub approved: HashMap<String, Vec<String>>,
    pub placed: Vec<String>,
}

impl Keys {
    pub fn read() -> Keys {
        use gio::prelude::*;
        if !helix_settings::has_schema(SCHEMA) {
            return Keys::default();
        }
        let s = gio::Settings::new(SCHEMA);
        let strv = |k: &str| s.strv(k).iter().map(|v| v.to_string()).collect();
        let approved = s.value("approved").get::<HashMap<String, Vec<String>>>().unwrap_or_default();
        Keys { enabled: strv("enabled"), disabled: strv("disabled"), disable_all: s.boolean("disable-all"), approved, placed: strv("placed") }
    }

    pub fn write(&self) {
        use gio::prelude::*;
        if !helix_settings::has_schema(SCHEMA) {
            tracing::warn!("{SCHEMA} is not installed; extension choices cannot be saved");
            return;
        }
        let s = gio::Settings::new(SCHEMA);
        fn refs(l: &[String]) -> Vec<&str> {
            l.iter().map(String::as_str).collect()
        }
        let _ = s.set_strv("enabled", refs(&self.enabled).as_slice());
        let _ = s.set_strv("disabled", refs(&self.disabled).as_slice());
        let _ = s.set_boolean("disable-all", self.disable_all);
        let _ = s.set_value("approved", &gio::glib::Variant::from(self.approved.clone()));
        let _ = s.set_strv("placed", refs(&self.placed).as_slice());
        gio::Settings::sync();
    }

    /// Whether `found` is turned on.
    pub fn is_enabled(&self, found: &Found) -> bool {
        if self.disable_all {
            return false;
        }
        match found.kind {
            Kind::Development => true,
            Kind::User => self.enabled.contains(&found.id),
            Kind::System => {
                let default = found.manifest.as_ref().is_ok_and(|m| m.enabled_by_default);
                self.enabled.contains(&found.id) || (default && !self.disabled.contains(&found.id))
            }
        }
    }

    /// Whether its permissions are approved.
    pub fn is_approved(&self, found: &Found) -> bool {
        let Ok(m) = &found.manifest else { return false };
        found.kind == Kind::Development || m.permissions.is_empty() || m.permissions.iter().all(|p| self.approved.get(&found.id).is_some_and(|a| a.contains(p)))
    }

    pub fn enable(&mut self, found: &Found, approve: bool) {
        self.disabled.retain(|i| *i != found.id);
        if !self.enabled.contains(&found.id) {
            self.enabled.push(found.id.clone());
        }
        if approve {
            if let Ok(m) = &found.manifest {
                self.approved.insert(found.id.clone(), m.permissions.clone());
            }
        }
    }

    pub fn disable(&mut self, found: &Found) {
        self.enabled.retain(|i| *i != found.id);
        let default = found.manifest.as_ref().is_ok_and(|m| m.enabled_by_default);
        if found.kind == Kind::System && default && !self.disabled.contains(&found.id) {
            self.disabled.push(found.id.clone());
        }
    }
}

/// The items to add to the layout the first time `m` is enabled: panel
/// items by zone index, then toggles and sliders; with what `placed`
/// gains.
pub fn first_placement(m: &Manifest, placed: &[String]) -> (Vec<(usize, String)>, Vec<String>, Vec<String>) {
    let full = |id: &str| format!("{}/{id}", m.id);
    let fresh = |id: &str| !placed.contains(&full(id));
    let panel: Vec<(usize, String)> = m.panel_items.iter().filter(|p| fresh(&p.id)).map(|p| (p.zones[0].index(), full(&p.id))).collect();
    let quick: Vec<String> = m.toggles.iter().chain(&m.sliders).filter(|q| fresh(&q.id)).map(|q| full(&q.id)).collect();
    let newly: Vec<String> = panel.iter().map(|(_, id)| id.clone()).chain(quick.iter().cloned()).collect();
    (panel, quick, newly)
}

/// `$XDG_STATE_HOME`, else `~/.local/state`.
fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| gio::glib::home_dir().join(".local/state"))
}

pub fn settings_path(id: &str) -> PathBuf {
    gio::glib::user_config_dir().join("helix/extensions").join(format!("{id}.toml"))
}

/// The settings' values: the file's where valid, else the defaults.
pub fn read_settings(m: &Manifest, path: &Path) -> BTreeMap<String, SettingValue> {
    let file: toml::Table = std::fs::read_to_string(path).ok().and_then(|s| s.parse().ok()).unwrap_or_default();
    m.settings
        .iter()
        .map(|d| (d.key.clone(), file.get(&d.key).and_then(|v| d.from_toml(v).ok()).unwrap_or_else(|| d.default.clone())))
        .collect()
}

pub fn write_settings(values: &BTreeMap<String, SettingValue>, path: &Path) -> std::io::Result<()> {
    let table: toml::Table = values.iter().map(|(k, v)| (k.clone(), v.to_toml())).collect();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, toml::to_string(&table).map_err(std::io::Error::other)?)?;
    std::fs::rename(tmp, path)
}

/// An extension's log: recent lines, and a file.
#[derive(Debug, Default)]
pub struct Log {
    pub lines: VecDeque<String>,
    pub file: Option<PathBuf>,
}

impl Log {
    pub fn for_id(id: &str) -> Log {
        Log { lines: VecDeque::new(), file: Some(state_dir().join("helix/extensions").join(format!("{id}.log"))) }
    }

    pub fn push(&mut self, line: String) {
        let line: String = line.chars().take(1000).collect();
        if let Some(path) = &self.file {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if std::fs::metadata(path).is_ok_and(|m| m.len() > LOG_FILE_MAX) {
                if let Ok(old) = std::fs::read(path) {
                    let half = &old[old.len() / 2..];
                    let start = half.iter().position(|b| *b == b'\n').map_or(0, |p| p + 1);
                    let _ = std::fs::write(path, &half[start..]);
                }
            }
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                let _ = writeln!(f, "{line}");
            }
        }
        self.lines.push_back(line);
        while self.lines.len() > LOG_LINES {
            self.lines.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, dir: &str, id: &str, extra: &str) {
        let d = root.join(dir);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("extension.toml"), format!("id = \"{id}\"\nname = \"N\"\nversion = \"1\"\napi = \"1.0\"\nexec = [\"x\"]\n{extra}")).unwrap();
    }

    #[test]
    fn discovery_keys_settings() {
        let tmp = std::env::temp_dir().join(format!("helix-exthost-{}", std::process::id()));
        let (user, system) = (tmp.join("user"), tmp.join("system"));
        write(&user, "org.a.One", "org.a.One", "");
        write(&system, "org.a.One", "org.a.One", "");
        write(&system, "org.a.Two", "org.a.Two", "enabled-by-default = true\npermissions = [\"notifications\"]\n[[contributes.panel-item]]\nid = \"p\"\nname = \"P\"\nzones = [\"center\", \"right\"]\n[[contributes.quick-toggle]]\nid = \"t\"\ntitle = \"T\"\n[settings]\nn = { type = \"int\", default = 2, max = 5, label = \"N\" }\n");
        write(&system, "org.a.Bad", "org.a.Wrong", "");
        let found = discover(&user, std::slice::from_ref(&system));
        assert_eq!(found.keys().collect::<Vec<_>>(), ["org.a.Bad", "org.a.One", "org.a.Two"]);
        assert_eq!(found["org.a.One"].kind, Kind::User);
        assert!(found["org.a.Bad"].manifest.is_err());

        let mut keys = Keys::default();
        let two = &found["org.a.Two"];
        assert!(keys.is_enabled(two), "on by default");
        assert!(!keys.is_approved(two), "needs approval");
        keys.disable(two);
        assert!(!keys.is_enabled(two) && keys.disabled == ["org.a.Two"]);
        keys.enable(two, true);
        assert!(keys.is_enabled(two) && keys.is_approved(two) && keys.disabled.is_empty());
        assert!(!keys.is_enabled(&found["org.a.One"]));
        keys.disable_all = true;
        assert!(!keys.is_enabled(two));

        let m = two.manifest.as_ref().unwrap();
        let (panel, quick, newly) = first_placement(m, &[]);
        assert_eq!(panel, [(1, "org.a.Two/p".to_owned())]);
        assert_eq!(quick, ["org.a.Two/t"]);
        let (panel, quick, _) = first_placement(m, &newly);
        assert!(panel.is_empty() && quick.is_empty(), "placed once only");

        let path = tmp.join("settings/org.a.Two.toml");
        assert_eq!(read_settings(m, &path)["n"], SettingValue::Int(2));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "n = 9\n").unwrap();
        assert_eq!(read_settings(m, &path)["n"], SettingValue::Int(2), "out of range: default");
        let mut values = read_settings(m, &path);
        values.insert("n".into(), SettingValue::Int(4));
        write_settings(&values, &path).unwrap();
        assert_eq!(read_settings(m, &path)["n"], SettingValue::Int(4));

        let mut log = Log { lines: Default::default(), file: Some(tmp.join("log/x.log")) };
        for i in 0..250 {
            log.push(format!("line {i}"));
        }
        assert_eq!(log.lines.len(), LOG_LINES);
        assert_eq!(log.lines.front().map(String::as_str), Some("line 50"));
        assert!(std::fs::read_to_string(tmp.join("log/x.log")).unwrap().contains("line 249"));
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
