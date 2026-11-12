//! Autostart (helix-desktop-spec `org.gnome.SessionManager.md`,
//! "Autostart"): apps from the XDG autostart directories, with GNOME's
//! `AutostartCondition`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use tokio::process::{Child, Command};

/// When an entry runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Condition {
    Always,
    /// While a boolean GSettings key is true.
    GSettings { schema: String, key: String },
    /// Only if (or unless) a file under `$XDG_CONFIG_HOME` exists.
    IfExists(String),
    UnlessExists(String),
    /// A form Helix does not know.
    Unknown(String),
}

/// A parsed autostart file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The file name (identifies the entry across directories).
    pub id: String,
    pub path: PathBuf,
    pub exec: Vec<String>,
    pub try_exec: Option<String>,
    pub hidden: bool,
    pub enabled: bool,
    pub only_show_in: Vec<String>,
    pub not_show_in: Vec<String>,
    pub condition: Condition,
}

/// The `[Desktop Entry]` group's keys (unlocalized).
fn desktop_entry(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut in_group = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_group = line == "[Desktop Entry]";
            continue;
        }
        if !in_group || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim();
            if !k.contains('[') {
                out.entry(k.to_owned()).or_insert_with(|| v.trim().to_owned());
            }
        }
    }
    out
}

/// Split `Exec` into arguments (quoting per the Desktop Entry spec) and
/// drop field codes.
pub fn exec_args(exec: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut started = false;
    let mut chars = exec.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            '\\' if quoted => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            ' ' | '\t' if !quoted => {
                if started {
                    args.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            _ => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        args.push(cur);
    }
    args.into_iter()
        .filter(|a| !(a.len() == 2 && a.starts_with('%') && a != "%%"))
        .map(|a| a.replace("%%", "%"))
        .collect()
}

fn list(value: Option<&String>) -> Vec<String> {
    value.map(|v| v.split(';').filter(|s| !s.is_empty()).map(str::to_owned).collect()).unwrap_or_default()
}

fn condition(value: Option<&String>) -> Condition {
    let Some(value) = value.map(|v| v.trim()).filter(|v| !v.is_empty()) else { return Condition::Always };
    let words: Vec<&str> = value.split_whitespace().collect();
    match words.as_slice() {
        ["GSettings", schema, key] => Condition::GSettings { schema: (*schema).to_owned(), key: (*key).to_owned() },
        ["if-exists", file] => Condition::IfExists((*file).to_owned()),
        ["unless-exists", file] => Condition::UnlessExists((*file).to_owned()),
        _ => Condition::Unknown(value.to_owned()),
    }
}

/// Parse an autostart file.
pub fn parse(id: &str, path: &Path, text: &str) -> Option<Entry> {
    let keys = desktop_entry(text);
    if keys.get("Type").map(String::as_str) != Some("Application") {
        return None;
    }
    let exec = exec_args(keys.get("Exec")?);
    if exec.is_empty() {
        return None;
    }
    let truthy = |k: &str| keys.get(k).is_some_and(|v| v == "true");
    Some(Entry {
        id: id.to_owned(),
        path: path.to_path_buf(),
        exec,
        try_exec: keys.get("TryExec").cloned(),
        hidden: truthy("Hidden"),
        enabled: keys.get("X-GNOME-Autostart-enabled").is_none_or(|v| v != "false"),
        only_show_in: list(keys.get("OnlyShowIn")),
        not_show_in: list(keys.get("NotShowIn")),
        condition: condition(keys.get("AutostartCondition")),
    })
}

/// The autostart directories, most important first.
pub fn directories() -> Vec<PathBuf> {
    let home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    let dirs = std::env::var("XDG_CONFIG_DIRS").ok().filter(|d| !d.is_empty()).unwrap_or_else(|| "/etc/xdg".into());
    home.into_iter().chain(dirs.split(':').filter(|d| !d.is_empty()).map(PathBuf::from)).map(|d| d.join("autostart")).collect()
}

/// Every entry, earlier directories hiding later files of the same name.
pub fn entries(dirs: &[PathBuf]) -> Vec<Entry> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(read) = std::fs::read_dir(dir) else { continue };
        let mut files: Vec<PathBuf> = read.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x == "desktop")).collect();
        files.sort();
        for path in files {
            let id = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if !seen.insert(id.clone()) {
                continue;
            }
            match std::fs::read_to_string(&path).ok().and_then(|text| parse(&id, &path, &text)) {
                Some(entry) => out.push(entry),
                None => tracing::debug!(path = %path.display(), "not an autostart application"),
            }
        }
    }
    out
}

fn in_path(program: &str, path_var: &str) -> bool {
    if program.contains('/') {
        return Path::new(program).is_file();
    }
    path_var.split(':').filter(|d| !d.is_empty()).any(|d| Path::new(d).join(program).is_file())
}

impl Entry {
    /// Whether the entry applies at all (ignoring a GSettings condition's
    /// current value): `desktops` from `XDG_CURRENT_DESKTOP`.
    pub fn applies(&self, desktops: &[&str], path_var: &str, config_home: &Path) -> bool {
        if self.hidden || !self.enabled {
            return false;
        }
        if !self.only_show_in.is_empty() && !self.only_show_in.iter().any(|d| desktops.contains(&d.as_str())) {
            return false;
        }
        if self.not_show_in.iter().any(|d| desktops.contains(&d.as_str())) {
            return false;
        }
        if self.try_exec.as_ref().is_some_and(|t| !in_path(t, path_var)) {
            return false;
        }
        match &self.condition {
            Condition::Always | Condition::GSettings { .. } => true,
            Condition::IfExists(f) => config_home.join(f).exists(),
            Condition::UnlessExists(f) => !config_home.join(f).exists(),
            Condition::Unknown(c) => {
                tracing::info!(id = self.id, condition = c, "unsupported autostart condition; not starting");
                false
            }
        }
    }
}

/// A GSettings condition's value now (false without the schema or key).
fn gsettings_true(schema: &str, key: &str) -> bool {
    use gio::prelude::*;
    let Some(source) = gio::SettingsSchemaSource::default() else { return false };
    let Some(found) = source.lookup(schema, true) else { return false };
    if !found.has_key(key) || found.key(key).value_type() != *gio::glib::VariantTy::BOOLEAN {
        return false;
    }
    gio::Settings::new(schema).boolean(key)
}

/// Runs the session's autostart apps.
pub struct Autostart {
    entries: Vec<Entry>,
    env: BTreeMap<String, String>,
    running: HashMap<String, Child>,
}

impl Autostart {
    /// The entries that apply, to run with `env` added.
    pub fn new(env: BTreeMap<String, String>) -> Self {
        let desktops_var = env.get("XDG_CURRENT_DESKTOP").cloned().or_else(|| std::env::var("XDG_CURRENT_DESKTOP").ok()).unwrap_or_default();
        let desktops: Vec<&str> = desktops_var.split(':').filter(|d| !d.is_empty()).collect();
        let path_var = env.get("PATH").cloned().or_else(|| std::env::var("PATH").ok()).unwrap_or_default();
        let config_home = directories().first().and_then(|d| d.parent().map(Path::to_path_buf)).unwrap_or_default();
        let entries = entries(&directories()).into_iter().filter(|e| e.applies(&desktops, &path_var, &config_home)).collect();
        Self { entries, env, running: HashMap::new() }
    }

    /// The GSettings schemas conditions depend on.
    pub fn schemas(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .entries
            .iter()
            .filter_map(|e| match &e.condition {
                Condition::GSettings { schema, .. } => Some(schema.clone()),
                _ => None,
            })
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// Start what should run and is not running; stop conditional apps
    /// whose condition no longer holds.
    pub fn update(&mut self) {
        self.running.retain(|_, child| matches!(child.try_wait(), Ok(None)));
        for entry in &self.entries {
            let wanted = match &entry.condition {
                Condition::GSettings { schema, key } => gsettings_true(schema, key),
                _ => true,
            };
            let conditional = matches!(entry.condition, Condition::GSettings { .. });
            match (wanted, self.running.get(&entry.id)) {
                (true, None) => {
                    let spawned = Command::new(&entry.exec[0]).args(&entry.exec[1..]).envs(&self.env).spawn();
                    match spawned {
                        Ok(child) => {
                            tracing::info!(id = entry.id, pid = child.id(), "autostarted");
                            // Unconditional apps are not followed.
                            if conditional {
                                self.running.insert(entry.id.clone(), child);
                            }
                        }
                        Err(err) => tracing::warn!(id = entry.id, %err, "cannot autostart"),
                    }
                }
                (false, Some(child)) => {
                    if let Some(pid) = child.id() {
                        tracing::info!(id = entry.id, pid, "condition no longer holds; stopping");
                        // SAFETY: kill(2) on our own child.
                        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
                    }
                    self.running.remove(&entry.id);
                }
                _ => {}
            }
        }
    }

    /// Run once, then follow GSettings conditions until `stop` resolves.
    pub async fn run(mut self, stop: impl std::future::Future<Output = ()>) {
        self.update();
        let schemas = self.schemas();
        if schemas.is_empty() {
            return;
        }
        let names: Vec<&str> = schemas.iter().map(String::as_str).filter(|s| helix_settings::has_schema(s)).collect();
        let settings = helix_settings::GSettings::new(&names);
        let (mut changes, _watch) = settings.watch();
        tokio::pin!(stop);
        loop {
            tokio::select! {
                _ = &mut stop => break,
                change = changes.recv() => {
                    if change.is_none() {
                        break;
                    }
                    self.update();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_lines() {
        assert_eq!(exec_args("orca"), ["orca"]);
        assert_eq!(exec_args("app --flag %U"), ["app", "--flag"]);
        assert_eq!(exec_args(r#""/opt/my app/run" "a \"b\"" 100%%"#), ["/opt/my app/run", "a \"b\"", "100%"]);
        assert_eq!(exec_args("  sh  -c  'x' "), ["sh", "-c", "'x'"]);
    }

    #[test]
    fn parsing_and_rules() {
        let text = "[Desktop Entry]\nType=Application\nName=Orca\nName[de]=Orka\nExec=orca\nAutostartCondition=GSettings org.gnome.desktop.a11y.applications screen-reader-enabled\nOnlyShowIn=GNOME;Unity;\n[Desktop Action x]\nExec=other\n";
        let e = parse("orca-autostart.desktop", Path::new("/x"), text).unwrap();
        assert_eq!(e.exec, ["orca"]);
        assert_eq!(e.condition, Condition::GSettings { schema: "org.gnome.desktop.a11y.applications".into(), key: "screen-reader-enabled".into() });
        let home = Path::new("/nonexistent");
        assert!(e.applies(&["Helix", "GNOME"], "", home));
        assert!(!e.applies(&["XFCE"], "", home));
        let hidden = parse("h.desktop", Path::new("/x"), "[Desktop Entry]\nType=Application\nExec=a\nHidden=true\n").unwrap();
        assert!(!hidden.applies(&[], "", home));
        let off = parse("o.desktop", Path::new("/x"), "[Desktop Entry]\nType=Application\nExec=a\nX-GNOME-Autostart-enabled=false\n").unwrap();
        assert!(!off.applies(&[], "", home));
        let not = parse("n.desktop", Path::new("/x"), "[Desktop Entry]\nType=Application\nExec=a\nNotShowIn=GNOME;\n").unwrap();
        assert!(!not.applies(&["Helix", "GNOME"], "", home));
        let try_exec = parse("t.desktop", Path::new("/x"), "[Desktop Entry]\nType=Application\nExec=a\nTryExec=no-such-program-here\n").unwrap();
        assert!(!try_exec.applies(&[], "/usr/bin:/bin", home));
        let sh = parse("s.desktop", Path::new("/x"), "[Desktop Entry]\nType=Application\nExec=a\nTryExec=sh\n").unwrap();
        assert!(sh.applies(&[], "/usr/bin:/bin", home));
        let odd = parse("u.desktop", Path::new("/x"), "[Desktop Entry]\nType=Application\nExec=a\nAutostartCondition=GNOME3 if-session gnome\n").unwrap();
        assert!(!odd.applies(&[], "", home));
        assert!(parse("l.desktop", Path::new("/x"), "[Desktop Entry]\nType=Link\nURL=x\n").is_none());
    }

    #[test]
    fn directories_shadow() {
        let base = std::env::temp_dir().join(format!("helix-autostart-{}", std::process::id()));
        let (a, b) = (base.join("a"), base.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("x.desktop"), "[Desktop Entry]\nType=Application\nExec=mine\n").unwrap();
        std::fs::write(b.join("x.desktop"), "[Desktop Entry]\nType=Application\nExec=system\n").unwrap();
        std::fs::write(b.join("y.desktop"), "[Desktop Entry]\nType=Application\nExec=other\n").unwrap();
        let found = entries(&[a, b]);
        assert_eq!(found.iter().map(|e| (e.id.as_str(), e.exec[0].as_str())).collect::<Vec<_>>(), [("x.desktop", "mine"), ("y.desktop", "other")]);
        let config = base.join("cfg");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(config.join("marker"), "").unwrap();
        let e = |c: &str| parse("c.desktop", Path::new("/x"), &format!("[Desktop Entry]\nType=Application\nExec=a\nAutostartCondition={c}\n")).unwrap();
        assert!(e("if-exists marker").applies(&[], "", &config));
        assert!(!e("unless-exists marker").applies(&[], "", &config));
        std::fs::remove_dir_all(&base).unwrap();
    }
}
