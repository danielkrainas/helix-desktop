//! Session save and restore (helix-desktop-spec `org.gnome.SessionManager.md`,
//! "RestoreSupported and session restore"): which app instances to restore,
//! with what reason, and which ids apps may clean up. Kept in a TOML file
//! across sessions.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// `reason` values of `RegisterRestore`.
pub mod reason {
    pub const NONE: u32 = 0;
    pub const LAUNCH: u32 = 1;
    pub const CRASHED: u32 = 2;
    pub const RESTORED: u32 = 3;
}

/// `org.gnome.desktop.session` `save-restore`: whether apps are saved at
/// log-out and restored at login (true without the key).
pub fn wanted() -> bool {
    use gio::prelude::*;
    const SCHEMA: &str = "org.gnome.desktop.session";
    let Some(source) = gio::SettingsSchemaSource::default() else { return true };
    match source.lookup(SCHEMA, true) {
        Some(schema) if schema.has_key("save-restore") => gio::Settings::new(SCHEMA).boolean("save-restore"),
        _ => true,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum State {
    /// Registered in the running session.
    Running,
    /// Registered when the session ended cleanly.
    Saved,
    /// Its app (or the session) went away without unregistering.
    Crashed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Instance {
    id: String,
    state: State,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct App {
    #[serde(default)]
    instances: Vec<Instance>,
    /// Ids the app may delete the data of.
    #[serde(default)]
    cleanup: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct File {
    /// Set while a session runs: still set at login means it crashed.
    #[serde(default)]
    session_running: bool,
    #[serde(default)]
    apps: BTreeMap<String, App>,
}

/// Restore records shared by the manager and the service (`None`: restore
/// is off).
pub type Shared = std::sync::Arc<std::sync::Mutex<Option<Restore>>>;

/// The restore records of this session.
#[derive(Debug)]
pub struct Restore {
    path: PathBuf,
    file: File,
    session: String,
    counter: u32,
    /// Instances registered this session, by the bus name that registered.
    owners: HashMap<String, Vec<(String, String)>>,
}

fn random_session() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/uuid")
        .map(|u| u.trim().replace('-', "")[..12].to_owned())
        .unwrap_or_else(|_| format!("{:x}", std::process::id()))
}

impl Restore {
    /// `$XDG_STATE_HOME/helix/restore.toml`.
    pub fn default_path() -> PathBuf {
        let state = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
            .unwrap_or_else(|| PathBuf::from("."));
        state.join("helix/restore.toml")
    }

    /// Start a session: read the records and note how the last one ended.
    pub fn open(path: PathBuf) -> Self {
        let file: File = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| toml::from_str(&text).ok())
            .unwrap_or_default();
        Self::start(path, file, random_session())
    }

    fn start(path: PathBuf, mut file: File, session: String) -> Self {
        let crashed = file.session_running;
        if crashed {
            tracing::warn!("the last session did not end cleanly; restoring its apps as after a crash");
        }
        for app in file.apps.values_mut() {
            for instance in &mut app.instances {
                if instance.state == State::Running {
                    instance.state = if crashed { State::Crashed } else { State::Saved };
                }
            }
        }
        file.session_running = true;
        let restore = Self { path, file, session, counter: 0, owners: HashMap::new() };
        restore.save();
        restore
    }

    fn save(&self) {
        let text = match toml::to_string(&self.file) {
            Ok(text) => text,
            Err(err) => {
                tracing::warn!(%err, "cannot write session restore records");
                return;
            }
        };
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(err) = std::fs::write(&self.path, text) {
            tracing::warn!(%err, path = %self.path.display(), "cannot write session restore records");
        }
    }

    /// Apps to start at login: one entry per restorable instance.
    pub fn to_relaunch(&self) -> Vec<String> {
        self.file
            .apps
            .iter()
            .flat_map(|(app, entry)| {
                entry
                    .instances
                    .iter()
                    .filter(|i| matches!(i.state, State::Saved | State::Crashed))
                    .map(move |_| app.clone())
            })
            .collect()
    }

    /// `RegisterRestore`: (reason, instance id, cleanup ids). Without
    /// `restoring`, instances left from earlier sessions are given up.
    pub fn register(&mut self, app_id: &str, owner: &str, restoring: bool) -> (u32, String, Vec<String>) {
        let entry = self.file.apps.entry(app_id.to_owned()).or_default();
        if !restoring {
            give_up(entry, |s| matches!(s, State::Saved | State::Crashed));
        }
        let restorable = entry.instances.iter_mut().find(|i| matches!(i.state, State::Saved | State::Crashed));
        let (reason, id) = match restorable {
            Some(instance) => {
                let reason = if instance.state == State::Crashed { reason::CRASHED } else { reason::RESTORED };
                instance.state = State::Running;
                (reason, instance.id.clone())
            }
            None => {
                self.counter += 1;
                let id = format!("helix-{}-{}", self.session, self.counter);
                entry.instances.push(Instance { id: id.clone(), state: State::Running });
                (reason::LAUNCH, id)
            }
        };
        let cleanup = entry.cleanup.clone();
        self.owners.entry(owner.to_owned()).or_default().push((app_id.to_owned(), id.clone()));
        self.save();
        (reason, id, cleanup)
    }

    /// `UnregisterRestore`: the instance exited cleanly.
    pub fn unregister(&mut self, app_id: &str, instance_id: &str) {
        let Some(entry) = self.file.apps.get_mut(app_id) else { return };
        let before = entry.instances.len();
        entry.instances.retain(|i| i.id != instance_id);
        if entry.instances.len() != before && !entry.cleanup.iter().any(|c| c == instance_id) {
            entry.cleanup.push(instance_id.to_owned());
        }
        for list in self.owners.values_mut() {
            list.retain(|(a, i)| !(a == app_id && i == instance_id));
        }
        self.save();
    }

    /// `DeletedInstanceIds`.
    pub fn deleted(&mut self, app_id: &str, ids: &[String]) {
        if let Some(entry) = self.file.apps.get_mut(app_id) {
            entry.cleanup.retain(|c| !ids.contains(c));
            if entry.instances.is_empty() && entry.cleanup.is_empty() {
                self.file.apps.remove(app_id);
            }
        }
        self.save();
    }

    /// A bus peer left: its registered instances crashed.
    pub fn owner_left(&mut self, owner: &str) {
        let Some(list) = self.owners.remove(owner) else { return };
        for (app_id, id) in list {
            if let Some(i) = self.file.apps.get_mut(&app_id).and_then(|e| e.instances.iter_mut().find(|i| i.id == id)) {
                if i.state == State::Running {
                    tracing::info!(app_id, id, "restorable app left without unregistering");
                    i.state = State::Crashed;
                }
            }
        }
        self.save();
    }

    /// The session ends cleanly: running instances are saved (given up
    /// without `saving`); instances offered for restore but never claimed
    /// are given up.
    pub fn end_session(&mut self, saving: bool) {
        for entry in self.file.apps.values_mut() {
            // Not claimed this session (they were offered at login).
            give_up(entry, |s| matches!(s, State::Saved | State::Crashed) || !saving);
            for i in &mut entry.instances {
                i.state = State::Saved;
            }
        }
        self.file.session_running = false;
        self.save();
    }
}

/// Drops `entry`'s instances in the states `which` picks, keeping their
/// ids for the app to clean up.
fn give_up(entry: &mut App, which: impl Fn(State) -> bool) {
    let mut given_up = Vec::new();
    entry.instances.retain(|i| {
        if which(i.state) {
            given_up.push(i.id.clone());
            false
        } else {
            true
        }
    });
    for id in given_up {
        if !entry.cleanup.contains(&id) {
            entry.cleanup.push(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("helix-restore-{}-{name}.toml", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn launch_save_restore() {
        let path = temp("clean");
        let mut r = Restore::start(path.clone(), File::default(), "s1".into());
        assert!(r.to_relaunch().is_empty());
        let (reason, id, cleanup) = r.register("org.example.Editor", ":1.5", true);
        assert_eq!((reason, id.as_str(), cleanup.len()), (reason::LAUNCH, "helix-s1-1", 0));
        let (_, closed, _) = r.register("org.example.Editor", ":1.5", true);
        r.unregister("org.example.Editor", &closed);
        r.end_session(true);

        // Next login: the instance that was open comes back.
        let mut r = Restore::open(path.clone());
        assert_eq!(r.to_relaunch(), ["org.example.Editor"]);
        let (reason, again, cleanup) = r.register("org.example.Editor", ":1.9", true);
        assert_eq!((reason, again.as_str()), (reason::RESTORED, id.as_str()));
        assert_eq!(cleanup, std::slice::from_ref(&closed), "the closed instance may be deleted");
        r.deleted("org.example.Editor", &[closed]);
        let (_, _, cleanup) = r.register("org.example.Editor", ":1.9", true);
        assert!(cleanup.is_empty());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn crashes() {
        let path = temp("crash");
        let mut r = Restore::start(path.clone(), File::default(), "s1".into());
        let (_, id, _) = r.register("org.example.Web", ":1.5", true);
        r.owner_left(":1.5");
        let (_, other, _) = r.register("org.example.Mail", ":1.6", true);
        drop(r); // the session never ended cleanly

        let mut r = Restore::open(path.clone());
        let mut relaunch = r.to_relaunch();
        relaunch.sort();
        assert_eq!(relaunch, ["org.example.Mail", "org.example.Web"]);
        assert_eq!(r.register("org.example.Web", ":2.1", true), (reason::CRASHED, id, vec![]));
        assert_eq!(r.register("org.example.Mail", ":2.2", true).0, reason::CRASHED, "the session crashed");
        let _ = other;
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn unclaimed_instances_are_given_up() {
        let path = temp("unclaimed");
        let mut r = Restore::start(path.clone(), File::default(), "s1".into());
        let (_, id, _) = r.register("org.example.Gone", ":1.5", true);
        r.end_session(true);
        let mut r = Restore::open(path.clone());
        assert_eq!(r.to_relaunch(), ["org.example.Gone"]);
        r.end_session(true); // never registered again
        let mut r = Restore::open(path.clone());
        assert!(r.to_relaunch().is_empty());
        assert_eq!(r.register("org.example.Gone", ":3.1", true).2, [id], "its data can go");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn save_restore_off() {
        let path = temp("off");
        let mut r = Restore::start(path.clone(), File::default(), "a".into());
        let (_, id, _) = r.register("org.example.Editor", ":1.5", true);
        r.end_session(false);
        let mut r = Restore::open(path.clone());
        assert!(r.to_relaunch().is_empty(), "nothing saved at log-out");
        let (reason, _, cleanup) = r.register("org.example.Editor", ":2.1", true);
        assert_eq!((reason, cleanup), (reason::LAUNCH, vec![id.clone()]));

        // Saved while on, turned off before the app comes back.
        r.end_session(true);
        let mut r = Restore::open(path.clone());
        assert_eq!(r.to_relaunch(), ["org.example.Editor"]);
        let (reason, _, cleanup) = r.register("org.example.Editor", ":3.1", false);
        assert_eq!(reason, reason::LAUNCH);
        assert_eq!(cleanup.len(), 2, "both earlier ids can go");
        let _ = std::fs::remove_file(path);
    }
}
