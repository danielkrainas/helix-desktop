//! Agent Access grants (helixdesktop/spec `contracts/agent-access/README.md`,
//! "Grants"): session grants and denials in memory, remembered (*Always
//! allow*) grants in `$XDG_CONFIG_HOME/helix/agent-grants.toml`, read once
//! when the shell starts and written whenever they change.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::identity::Agent;
use super::policy::{Config, Level, RememberMode};

/// The file format's version.
const VERSION: i64 = 1;

/// What the store says about a call needing a level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allowed,
    Denied,
    /// No grant covers it: ask the user.
    Ask,
}

/// The user's answer in the consent prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Deny,
    Session,
    Always,
    /// Nobody could be asked, or the prompt went away unanswered.
    Unanswered,
}

/// A remembered grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remembered {
    pub level: Level,
    pub granted: i64,
}

/// One agent the store knows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Entry {
    pub executable: String,
    pub name: String,
    pub identified: bool,
    /// This session's grant: level and when.
    pub session: Option<(Level, i64)>,
    pub always: Option<Remembered>,
    /// Denied this session.
    pub denied: bool,
    /// Its last allowed call this session (Unix seconds), 0 for none.
    pub last_active: i64,
    /// Stopped from the indicator: its remembered grant is not honoured
    /// for the rest of the session (it is asked again).
    pub stopped: bool,
}

impl Entry {
    /// The level this agent's grants give, before `access-level`.
    fn granted(&self, remember: RememberMode) -> Option<Level> {
        let always = self.always.as_ref().filter(|_| remember == RememberMode::Remember && !self.stopped).map(|a| a.level);
        self.session.map(|(l, _)| l).max(always)
    }

    fn is_empty(&self) -> bool {
        self.session.is_none() && self.always.is_none() && !self.denied
    }
}

/// All grants.
#[derive(Debug, Default)]
pub struct Grants {
    agents: BTreeMap<String, Entry>,
    /// Where remembered grants live; `None` keeps them in memory only.
    path: Option<PathBuf>,
    /// Entries of the file this version does not understand, kept as they
    /// were.
    unknown: Vec<toml::Table>,
}

/// `$XDG_CONFIG_HOME/helix/agent-grants.toml`.
pub fn default_path() -> PathBuf {
    gio::glib::user_config_dir().join("helix/agent-grants.toml")
}

impl Grants {
    /// The grants remembered in `path` (none if it is missing or broken).
    pub fn load(path: Option<PathBuf>) -> Grants {
        let mut grants = Grants { path, ..Default::default() };
        let Some(path) = &grants.path else { return grants };
        let Ok(text) = std::fs::read_to_string(path) else { return grants };
        let table: toml::Table = match text.parse() {
            Ok(t) => t,
            Err(err) => {
                tracing::warn!(%err, path = %path.display(), "cannot read remembered agent grants; ignoring them");
                return grants;
            }
        };
        for item in table.get("agent").and_then(|a| a.as_array()).into_iter().flatten() {
            let Some(t) = item.as_table() else { continue };
            let s = |k: &str| t.get(k).and_then(|v| v.as_str());
            match (s("id"), s("level").and_then(Level::from_id)) {
                (Some(id), Some(level)) if !id.is_empty() => {
                    let entry = grants.agents.entry(id.to_owned()).or_default();
                    entry.executable = s("executable").unwrap_or(id).to_owned();
                    entry.name = s("name").unwrap_or_default().to_owned();
                    entry.identified = true;
                    entry.always = Some(Remembered { level, granted: t.get("granted").and_then(|v| v.as_integer()).unwrap_or(0) });
                }
                _ => grants.unknown.push(t.clone()),
            }
        }
        grants
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        let mut agents: Vec<toml::Value> = self
            .agents
            .iter()
            .filter_map(|(id, e)| {
                let a = e.always.as_ref()?;
                let mut t = toml::Table::new();
                t.insert("id".into(), id.clone().into());
                t.insert("executable".into(), e.executable.clone().into());
                t.insert("name".into(), e.name.clone().into());
                t.insert("level".into(), a.level.id().into());
                t.insert("granted".into(), a.granted.into());
                Some(t.into())
            })
            .collect();
        agents.extend(self.unknown.iter().cloned().map(toml::Value::from));
        let mut doc = toml::Table::new();
        doc.insert("version".into(), VERSION.into());
        doc.insert("agent".into(), agents.into());
        let text = toml::to_string(&doc).unwrap_or_default();
        if let Err(err) = write_private(path, &text) {
            tracing::warn!(%err, path = %path.display(), "cannot write remembered agent grants");
        }
    }

    /// What a call from `agent` needing `level` may do under `config`.
    pub fn decide(&self, agent: &str, level: Level, config: &Config) -> Decision {
        let Some(entry) = self.agents.get(agent) else { return Decision::Ask };
        if entry.denied {
            return Decision::Denied;
        }
        match entry.granted(config.remember) {
            Some(granted) if granted >= level && config.level >= level => Decision::Allowed,
            _ => Decision::Ask,
        }
    }

    /// The level `agent` may use now (its grant, capped by `access-level`).
    pub fn effective(&self, agent: &str, config: &Config) -> Option<Level> {
        let entry = self.agents.get(agent).filter(|e| !e.denied)?;
        entry.granted(config.remember).map(|l| l.min(config.level))
    }

    /// `none`, `session`, `always` or `denied`, as `GetStatus` says.
    pub fn kind(&self, agent: &str, config: &Config) -> &'static str {
        match self.agents.get(agent) {
            Some(e) if e.denied => "denied",
            Some(e) if e.session.is_some() => "session",
            Some(e) if e.always.is_some() && config.remember == RememberMode::Remember && !e.stopped => "always",
            _ => "none",
        }
    }

    /// Record the user's answer to a prompt for `level`. *Always allow*
    /// for an unidentified agent counts as this session only.
    pub fn answer(&mut self, agent: &Agent, level: Level, answer: Answer, now: i64) {
        if answer == Answer::Unanswered {
            return;
        }
        let entry = self.agents.entry(agent.id.clone()).or_default();
        entry.executable = agent.executable.clone();
        if !agent.name.is_empty() {
            entry.name = agent.name.clone();
        }
        entry.identified = agent.identified;
        match answer {
            Answer::Deny => {
                entry.denied = true;
                entry.session = None;
            }
            Answer::Session => {
                entry.denied = false;
                entry.stopped = false;
                entry.session = Some((level, now));
            }
            Answer::Always => {
                entry.denied = false;
                entry.stopped = false;
                entry.session = Some((level, now));
                if agent.identified {
                    entry.always = Some(Remembered { level, granted: now });
                    self.save();
                }
            }
            Answer::Unanswered => {}
        }
    }

    /// Note an allowed call (and the display name it came with).
    pub fn touch(&mut self, agent: &Agent, now: i64) {
        if let Some(entry) = self.agents.get_mut(&agent.id) {
            entry.last_active = now;
            if !agent.name.is_empty() {
                entry.name = agent.name.clone();
            }
        }
    }

    /// End `agent`'s session grant and denial. Whether anything changed.
    pub fn revoke(&mut self, agent: &str) -> bool {
        let Some(entry) = self.agents.get_mut(agent) else { return false };
        let changed = entry.session.is_some() || entry.denied || entry.stopped;
        entry.session = None;
        entry.denied = false;
        entry.stopped = false;
        if entry.is_empty() {
            self.agents.remove(agent);
        }
        changed
    }

    /// *Stop* in the indicator: end `agent`'s session grant, and do not
    /// honour its remembered grant for the rest of the session, so that its
    /// next call asks the user again. Whether anything changed.
    pub fn stop(&mut self, agent: &str) -> bool {
        let Some(entry) = self.agents.get_mut(agent) else { return false };
        let changed = entry.session.is_some() || (entry.always.is_some() && !entry.stopped);
        entry.session = None;
        entry.stopped = entry.always.is_some();
        entry.last_active = 0;
        if entry.is_empty() {
            self.agents.remove(agent);
        }
        changed
    }

    /// Revoke, and forget the remembered grant. Whether anything changed.
    pub fn forget(&mut self, agent: &str) -> bool {
        let Some(entry) = self.agents.remove(agent) else { return false };
        if entry.always.is_some() {
            self.save();
        }
        true
    }

    /// Revoke every agent (Stop all, or the master switch turned off).
    pub fn revoke_all(&mut self) -> bool {
        let ids: Vec<String> = self.agents.keys().cloned().collect();
        let mut changed = false;
        for id in ids {
            changed |= self.revoke(&id);
        }
        changed
    }

    /// Every agent with a grant or denial, by id.
    pub fn list(&self) -> impl Iterator<Item = (&String, &Entry)> {
        self.agents.iter()
    }
}

/// Write `text` to `path` (mode 0600) through a temporary file and a rename.
pub fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("toml.tmp");
    let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(id: &str, identified: bool) -> Agent {
        Agent { id: id.into(), executable: id.into(), name: "Example".into(), identified, pid: 2, caller_pid: 3, ssh: false }
    }

    fn config(level: Level, remember: RememberMode) -> Config {
        Config { enabled: true, level, remember, ..Config::default() }
    }

    #[test]
    fn session_grants_and_denials() {
        let c = config(Level::Desktop, RememberMode::Ask);
        let mut g = Grants::load(None);
        let a = agent("/opt/a", true);
        assert_eq!(g.decide(&a.id, Level::ReadOnly, &c), Decision::Ask);
        g.answer(&a, Level::ReadOnly, Answer::Session, 100);
        assert_eq!(g.decide(&a.id, Level::ReadOnly, &c), Decision::Allowed);
        assert_eq!(g.decide(&a.id, Level::Desktop, &c), Decision::Ask, "the grant was for read only");
        assert_eq!(g.kind(&a.id, &c), "session");
        // Unanswered changes nothing.
        g.answer(&a, Level::Desktop, Answer::Unanswered, 101);
        assert_eq!(g.effective(&a.id, &c), Some(Level::ReadOnly));
        // A denial holds until revoked.
        g.answer(&a, Level::Desktop, Answer::Deny, 102);
        assert_eq!(g.decide(&a.id, Level::ReadOnly, &c), Decision::Denied);
        assert_eq!(g.kind(&a.id, &c), "denied");
        assert_eq!(g.effective(&a.id, &c), None);
        assert!(g.revoke(&a.id));
        assert_eq!(g.decide(&a.id, Level::ReadOnly, &c), Decision::Ask);
        assert!(!g.revoke(&a.id), "nothing left");
        assert_eq!(g.list().count(), 0);
    }

    #[test]
    fn stop_asks_again() {
        let c = config(Level::Desktop, RememberMode::Remember);
        let mut g = Grants::load(None);
        let a = agent("/opt/a", true);
        g.answer(&a, Level::ReadOnly, Answer::Always, 100);
        g.touch(&a, 101);
        assert!(g.stop(&a.id));
        // The remembered grant waits until the user answers again.
        assert_eq!(g.decide(&a.id, Level::ReadOnly, &c), Decision::Ask);
        assert_eq!(g.kind(&a.id, &c), "none");
        assert_eq!(g.list().next().map(|(_, e)| e.last_active), Some(0), "no longer active");
        assert!(!g.stop(&a.id), "already stopped");
        g.answer(&a, Level::ReadOnly, Answer::Session, 102);
        assert_eq!(g.decide(&a.id, Level::ReadOnly, &c), Decision::Allowed);
        // A session grant alone just ends.
        let b = agent("/opt/b", true);
        g.answer(&b, Level::ReadOnly, Answer::Session, 103);
        assert!(g.stop(&b.id));
        assert_eq!(g.decide(&b.id, Level::ReadOnly, &c), Decision::Ask);
        assert!(g.list().all(|(id, _)| id != &b.id), "nothing left of it");
    }

    #[test]
    fn grants_are_capped_by_the_setting() {
        let mut g = Grants::load(None);
        let a = agent("/opt/a", true);
        g.answer(&a, Level::Apps, Answer::Session, 1);
        let lower = config(Level::ReadOnly, RememberMode::Ask);
        assert_eq!(g.decide(&a.id, Level::Desktop, &lower), Decision::Ask, "lowering the setting narrows the grant");
        assert_eq!(g.effective(&a.id, &lower), Some(Level::ReadOnly));
        // Raising the setting widens no grant.
        g.answer(&a, Level::ReadOnly, Answer::Session, 2);
        let higher = config(Level::ScreenAndInput, RememberMode::Ask);
        assert_eq!(g.decide(&a.id, Level::Desktop, &higher), Decision::Ask);
        assert_eq!(g.effective(&a.id, &higher), Some(Level::ReadOnly));
    }

    #[test]
    fn remembered_grants_persist() {
        let dir = std::env::temp_dir().join(format!("helix-agent-grants-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("helix/agent-grants.toml");
        let remember = config(Level::Desktop, RememberMode::Remember);
        let ask = config(Level::Desktop, RememberMode::Ask);
        let a = agent("/usr/bin/python3 /home/me/a.py", true);
        let u = agent("/usr/bin/helix-agent", false);
        {
            let mut g = Grants::load(Some(path.clone()));
            g.answer(&a, Level::Desktop, Answer::Always, 1795132800);
            g.answer(&u, Level::ReadOnly, Answer::Always, 5);
            assert_eq!(g.kind(&u.id, &remember), "session", "an unidentified agent is not remembered");
        }
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("version = 1"), "{text}");
        assert!(!text.contains("helix-agent"), "{text}");

        let mut g = Grants::load(Some(path.clone()));
        assert_eq!(g.decide(&a.id, Level::Desktop, &remember), Decision::Allowed);
        assert_eq!(g.kind(&a.id, &remember), "always");
        // Under "ask" remembered grants are ignored, but kept.
        assert_eq!(g.decide(&a.id, Level::ReadOnly, &ask), Decision::Ask);
        assert_eq!(g.list().count(), 1);
        // Revoking keeps the remembered grant; forgetting removes it.
        g.answer(&a, Level::Desktop, Answer::Session, 7);
        assert!(g.revoke(&a.id));
        assert_eq!(g.decide(&a.id, Level::Desktop, &remember), Decision::Allowed);
        assert!(g.forget(&a.id));
        assert_eq!(g.decide(&a.id, Level::Desktop, &remember), Decision::Ask);
        assert_eq!(Grants::load(Some(path.clone())).list().count(), 0, "forgotten in the file too");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keeps_entries_it_does_not_understand() {
        let dir = std::env::temp_dir().join(format!("helix-agent-grants-unknown-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent-grants.toml");
        std::fs::write(&path, "version = 2\n[[agent]]\nid = \"/opt/new\"\nlevel = \"everything\"\n\n[[agent]]\nid = \"/opt/a\"\nlevel = \"apps\"\ngranted = 9\n").unwrap();
        let mut g = Grants::load(Some(path.clone()));
        assert_eq!(g.list().map(|(id, _)| id.as_str()).collect::<Vec<_>>(), ["/opt/a"]);
        assert_eq!(g.decide("/opt/new", Level::ReadOnly, &config(Level::Apps, RememberMode::Remember)), Decision::Ask);
        g.answer(&agent("/opt/b", true), Level::ReadOnly, Answer::Always, 10);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("everything") && text.contains("/opt/a") && text.contains("/opt/b"), "{text}");
        // A broken file is ignored.
        std::fs::write(&path, "not toml [").unwrap();
        assert_eq!(Grants::load(Some(path)).list().count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
