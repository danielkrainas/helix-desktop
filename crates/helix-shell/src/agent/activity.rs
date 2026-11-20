//! The activity log (helixdesktop/spec `contracts/agent-access/README.md`,
//! "Activity log"; helixdesktop/desktop#144): one JSON object per line in
//! `$XDG_STATE_HOME/helix/agent-activity/YYYY-MM-DD.jsonl`, which Settings'
//! *View Activity* and `helix-agent log` read directly.
//!
//! Every line is one `write` to a file opened with `O_APPEND` and
//! `O_NOFOLLOW` (a symlink is never followed), so lines of concurrent
//! calls do not mix and a crash leaves at most one torn line, which the
//! next write ends first and readers skip. Identical refusals within a
//! minute are coalesced into one line with a count, refusals stop at
//! [`REFUSED_CAP`] bytes a day and everything at [`DAY_CAP`], so an agent
//! hammering a refused method cannot fill the disk. Day files older than
//! `log-retention-days` are removed when the shell starts, when the day
//! changes and when the setting changes.
//!
//! What goes in is only what [`Entry`] holds: ids, method names, targets
//! (app ids, window ids, file URIs) and the kinds and lengths of what an
//! agent sent ([`Entry::detail`]); never text, clipboard data,
//! notification contents, field values or search queries.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use super::policy::{AgentError, Level};

/// A day file's size after which refusals (anything not allowed) are no
/// longer written that day, so allowed calls still are.
pub const REFUSED_CAP: u64 = 4 << 20;
/// A day file's size after which nothing more is written that day.
pub const DAY_CAP: u64 = 16 << 20;
/// Identical refusals within this many seconds of one that was written
/// are counted, and written as one line when the time is up.
pub const COALESCE_SECS: i64 = 60;
/// Coalesced refusals kept in memory at most; beyond it they are written
/// out at once.
const MAX_PENDING: usize = 256;
/// The longest target kept (agent-given app ids, folders); longer ones
/// are cut.
const MAX_TARGET: usize = 256;

/// Methods never recorded (README.md, "Activity log").
pub const UNLOGGED: &[&str] = &["GetStatus"];

/// Pseudo-methods: what the user did rather than a D-Bus call. They do
/// not clash with `Agent1` method names.
pub const CONSENT_PROMPT: &str = "ConsentPrompt";
pub const SENSITIVE_PROMPT: &str = "SensitivePrompt";
pub const STOP: &str = "Stop";
/// An agent's reminder came due and was shown (or dropped, access being
/// off), or the user cancelled it.
pub const REMINDER: &str = "Reminder";
/// Written once when a day file reaches a cap.
pub const LOG_FULL: &str = "LogFull";

/// How a call ended, as the policy sees it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum Outcome {
    /// Let through (it may still have failed: [`Entry::error`]).
    #[default]
    Allowed,
    /// Refused, with the error's short name (`AccessOff`, `Excluded`, …).
    Refused(String),
    /// The user dismissed a prompt or question, or stopped the agent.
    Cancelled,
}

impl Outcome {
    /// `allowed`, `refused:<Error>` or `cancelled`.
    pub fn text(&self) -> String {
        match self {
            Outcome::Allowed => "allowed".to_owned(),
            Outcome::Refused(why) => format!("refused:{why}"),
            Outcome::Cancelled => "cancelled".to_owned(),
        }
    }
}

/// Error names that are the policy's refusals (README.md, "Errors"; and
/// `AccessDenied` for Settings' interface).
const REFUSALS: &[&str] = &["Sandboxed", "AccessOff", "Remote", "Locked", "LevelTooLow", "FeatureOff", "NotGranted", "DeniedByUser", "Excluded", "NeedsConfirmation", "AccessDenied"];

/// The outcome of a call that ended in `err`, and the error to record for
/// an allowed call that failed (`TimedOut`, `NotFound`, `InvalidArgs`, …).
pub fn outcome_of(err: &AgentError) -> (Outcome, Option<&'static str>) {
    let kind = err.kind();
    if REFUSALS.contains(&kind) {
        (Outcome::Refused(kind.to_owned()), None)
    } else if kind == "Cancelled" {
        (Outcome::Cancelled, None)
    } else {
        (Outcome::Allowed, Some(kind))
    }
}

/// Why a call was cancelled or refused, as a token of a fixed vocabulary
/// (README.md, "Activity log", `reason`), when the shell knows more than
/// the error's name: which exclusion (`Excluded`; an app the user
/// excluded is just `app`, never its id), what stopped an input
/// (`Cancelled`: `user-input`, `cooldown`, `stopped`), which switch was
/// off (`FeatureOff`). Never anything else of the error's message.
pub fn reason_of(err: &AgentError) -> Option<&'static str> {
    match err {
        AgentError::Excluded(token) => match token.as_str() {
            "password-field" => Some("password-field"),
            "system-prompt" => Some("system-prompt"),
            "shell" => Some("shell"),
            "settings" => Some("settings"),
            t if t.starts_with("app:") => Some("app"),
            _ => None,
        },
        AgentError::Cancelled(why) => match why.as_str() {
            "user-input" => Some("user-input"),
            "cooldown" => Some("cooldown"),
            "stopped" => Some("stopped"),
            _ => None,
        },
        AgentError::FeatureOff(switch) => match switch.as_str() {
            "clipboard" => Some("clipboard"),
            _ => None,
        },
        _ => None,
    }
}

/// A detail's value: a kind (a short token) or a length.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Detail {
    Kind(&'static str),
    Count(u64),
}

/// What makes refusals identical: `agent`, `method`, `target`,
/// `outcome` and `reason`.
type Key = (String, String, String, String, Option<&'static str>);

/// One line of the log.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Entry {
    /// When the call arrived (Unix seconds).
    pub time: i64,
    /// The agent's id ("" when it could not be identified).
    pub agent: String,
    /// Its self-declared display name, or "".
    pub name: String,
    pub method: String,
    /// What it acted on: an app id, `window:<id> (<app id>)`, a file URI;
    /// "" for none.
    pub target: String,
    /// The level the call needed.
    pub level: Option<Level>,
    pub outcome: Outcome,
    /// For a cancelled or refused call: why, as a token ([`reason_of`]).
    pub reason: Option<&'static str>,
    /// For an allowed call that failed: the error's short name.
    pub error: Option<String>,
    /// Kinds and lengths of what the agent sent (`query-length`, …) or
    /// what the user chose (`grant`); never contents.
    pub detail: Vec<(&'static str, Detail)>,
    /// Coalesced refusals: how many calls the line stands for (≥ 2), and
    /// the last one's time.
    pub count: u64,
    pub until: i64,
}

/// `s` as a JSON string.
fn quote(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_owned())
}

/// `text` without control characters, at most [`MAX_TARGET`] characters.
pub fn clean_target(text: &str) -> String {
    let clean: String = text.chars().filter(|c| !c.is_control()).collect();
    if clean.chars().count() <= MAX_TARGET {
        clean
    } else {
        let mut cut: String = clean.chars().take(MAX_TARGET - 1).collect();
        cut.push('…');
        cut
    }
}

impl Entry {
    /// The line (without its newline): `time`, `agent`, `name`, `method`,
    /// then `target`, `level` when there are some, `outcome`, then
    /// `reason`, `error`, the details, `count` and `until` when there are some.
    pub fn line(&self) -> String {
        let mut out = format!("{{\"time\":{},\"agent\":{},\"name\":{},\"method\":{}", self.time, quote(&self.agent), quote(&self.name), quote(&self.method));
        if !self.target.is_empty() {
            out += &format!(",\"target\":{}", quote(&clean_target(&self.target)));
        }
        if let Some(level) = self.level {
            out += &format!(",\"level\":{}", quote(level.id()));
        }
        out += &format!(",\"outcome\":{}", quote(&self.outcome.text()));
        if let Some(reason) = self.reason.filter(|_| self.outcome != Outcome::Allowed) {
            out += &format!(",\"reason\":{}", quote(reason));
        }
        if let Some(error) = &self.error {
            out += &format!(",\"error\":{}", quote(error));
        }
        for (key, value) in &self.detail {
            match value {
                Detail::Kind(kind) => out += &format!(",{}:{}", quote(key), quote(kind)),
                Detail::Count(n) => out += &format!(",{}:{n}", quote(key)),
            }
        }
        if self.count > 1 {
            out += &format!(",\"count\":{},\"until\":{}", self.count, self.until);
        }
        out.push('}');
        out
    }

    /// What makes refusals "identical" for coalescing.
    fn key(&self) -> Key {
        (self.agent.clone(), self.method.clone(), self.target.clone(), self.outcome.text(), self.reason)
    }
}

/// The local date of `time` as `YYYY-MM-DD`.
pub fn day_of(time: i64) -> String {
    gio::glib::DateTime::from_unix_local(time).ok().and_then(|d| d.format("%Y-%m-%d").ok()).map(|s| s.to_string()).unwrap_or_else(|| "1970-01-01".to_owned())
}

/// The oldest day kept at `now` when `days` days are kept (today counts
/// as one).
pub fn oldest_kept(now: i64, days: u32) -> String {
    let back = i32::try_from(days.clamp(1, 365)).unwrap_or(1) - 1;
    gio::glib::DateTime::from_unix_local(now)
        .ok()
        .and_then(|d| d.add_days(-back).ok())
        .and_then(|d| d.format("%Y-%m-%d").ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| day_of(now - i64::from(back) * 86400))
}

/// The day of a day file's name (`YYYY-MM-DD.jsonl`).
pub fn day_file(name: &str) -> Option<&str> {
    let day = name.strip_suffix(".jsonl")?;
    let b = day.as_bytes();
    let ok = b.len() == 10 && b.iter().enumerate().all(|(i, c)| if i == 4 || i == 7 { *c == b'-' } else { c.is_ascii_digit() });
    ok.then_some(day)
}

/// Make sure `dir` is our own real directory with mode 0700, creating it
/// if needed. A symlink (or another user's directory) is refused.
fn private_dir(dir: &Path) -> std::io::Result<()> {
    let refuse = |why: &str| std::io::Error::new(std::io::ErrorKind::PermissionDenied, format!("{}: {why}", dir.display()));
    match std::fs::symlink_metadata(dir) {
        Ok(meta) => {
            if !meta.file_type().is_dir() {
                return Err(refuse("not a directory (a symlink is not followed)"));
            }
            // SAFETY: getuid cannot fail.
            if meta.uid() != unsafe { libc::getuid() } {
                return Err(refuse("owned by another user"));
            }
            if meta.mode() & 0o077 != 0 {
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = dir.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::DirBuilder::new().mode(0o700).create(dir)
        }
        Err(e) => Err(e),
    }
}

/// Append `line` to `path` in one write, never following a symlink; mode
/// 0600. What `cap` says about the file's size decides first.
fn append(path: &Path, line: &str, cap: impl FnOnce(u64) -> Option<String>) -> std::io::Result<bool> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.file_type().is_file() {
        return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, format!("{}: not a regular file", path.display())));
    }
    if meta.mode() & 0o077 != 0 {
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    let size = meta.len();
    let line = match cap(size) {
        None => line.to_owned(),
        Some(marker) if marker.is_empty() => return Ok(false),
        Some(marker) => marker,
    };
    // A line torn by a crash is ended first, in the same write.
    let mut last = [0u8; 1];
    let torn = size > 0 && file.read_at(&mut last, size - 1).is_ok() && last[0] != b'\n';
    let text = if torn { format!("\n{line}\n") } else { format!("{line}\n") };
    (&file).write_all(text.as_bytes())?;
    Ok(true)
}

#[derive(Debug)]
struct Pending {
    /// The line that was written; the coalesced ones are like it.
    entry: Entry,
    /// When that line's call was.
    since: i64,
    /// Calls counted since, and the first and last one's time.
    count: u64,
    first: i64,
    last: i64,
}

impl Pending {
    /// The line standing for the counted calls, if any.
    fn summary(&self) -> Option<Entry> {
        match self.count {
            0 => None,
            // One counted call is an ordinary line.
            1 => Some(Entry { time: self.first, ..self.entry.clone() }),
            n => Some(Entry { time: self.first, count: n, until: self.last, ..self.entry.clone() }),
        }
    }
}

#[derive(Debug, Default)]
struct State {
    retention: u32,
    /// The day pruning last ran for.
    pruned: String,
    pending: HashMap<Key, Pending>,
    /// Days whose file got its marker this run: (day, which cap).
    marked: HashSet<(String, &'static str)>,
    /// A refusal to write was reported to the shell's log.
    warned: bool,
}

/// The log: where it is and the coalescing state.
#[derive(Debug)]
pub struct Log {
    /// `None`: nothing is written (tests).
    dir: Option<PathBuf>,
    state: Mutex<State>,
}

impl Log {
    pub fn new(dir: Option<PathBuf>) -> Log {
        Log { dir, state: Mutex::new(State { retention: 30, ..State::default() }) }
    }

    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Record `entry`: allowed calls at once, refusals coalesced.
    pub fn record(&self, entry: Entry) {
        if UNLOGGED.contains(&entry.method.as_str()) {
            return;
        }
        let mut state = self.state();
        if entry.outcome != Outcome::Allowed {
            let key = entry.key();
            if let Some(p) = state.pending.get_mut(&key) {
                if entry.time - p.since < COALESCE_SECS && entry.time >= p.since {
                    if p.count == 0 {
                        (p.first, p.last) = (entry.time, entry.time);
                    }
                    p.count += 1;
                    p.first = p.first.min(entry.time);
                    p.last = p.last.max(entry.time);
                    return;
                }
                let p = state.pending.remove(&key).expect("just found");
                if let Some(summary) = p.summary() {
                    self.write(&mut state, &summary);
                }
            }
            if state.pending.len() >= MAX_PENDING {
                self.flush_all(&mut state);
            }
            state.pending.insert(key, Pending { entry: entry.clone(), since: entry.time, count: 0, first: 0, last: 0 });
        }
        self.write(&mut state, &entry);
    }

    /// Write the coalesced refusals whose minute is up at `now`.
    pub fn flush(&self, now: i64) {
        let mut state = self.state();
        let done: Vec<_> = state.pending.iter().filter(|(_, p)| now - p.since >= COALESCE_SECS || now < p.since).map(|(k, _)| k.clone()).collect();
        for key in done {
            if let Some(summary) = state.pending.remove(&key).and_then(|p| p.summary()) {
                self.write(&mut state, &summary);
            }
        }
    }

    fn flush_all(&self, state: &mut State) {
        let pending: Vec<Pending> = state.pending.drain().map(|(_, p)| p).collect();
        for p in pending {
            if let Some(summary) = p.summary() {
                self.write(state, &summary);
            }
        }
    }

    fn write(&self, state: &mut State, entry: &Entry) {
        let Some(dir) = &self.dir else { return };
        let day = day_of(entry.time);
        let refusal = entry.outcome != Outcome::Allowed;
        let line = entry.line();
        let marker = |which: &'static str| Entry { time: entry.time, method: LOG_FULL.to_owned(), outcome: Outcome::Refused(LOG_FULL.to_owned()), detail: vec![("dropped", Detail::Kind(which))], ..Entry::default() }.line();
        let mut marked = None;
        let result = private_dir(dir).and_then(|()| {
            append(&dir.join(format!("{day}.jsonl")), &line, |size| {
                let (cap, which) = if refusal { (REFUSED_CAP, "refusals") } else { (DAY_CAP, "all") };
                if size + (line.len() as u64) < cap {
                    return None;
                }
                // Over: once per day and cap a marker (allowed past the
                // cap), else nothing.
                if size < DAY_CAP + 4096 && !state.marked.contains(&(day.clone(), which)) {
                    marked = Some(which);
                    Some(marker(which))
                } else {
                    Some(String::new())
                }
            })
        });
        if let Some(which) = marked {
            state.marked.insert((day, which));
            tracing::warn!(dropped = which, "the agent activity log is full for today");
        }
        match result {
            Ok(_) => state.warned = false,
            Err(err) if !state.warned => {
                state.warned = true;
                tracing::warn!(%err, "cannot write the agent activity log");
            }
            Err(_) => {}
        }
    }

    /// Keep `days` days from now on; prune when it changed.
    pub fn set_retention(&self, days: u32, now: i64) {
        let changed = {
            let mut state = self.state();
            let changed = state.retention != days || state.pruned.is_empty();
            state.retention = days;
            changed
        };
        if changed {
            self.prune(now);
        }
    }

    /// Prune when the day changed since the last time.
    pub fn tick(&self, now: i64) {
        self.flush(now);
        let today = day_of(now);
        if self.state().pruned != today {
            self.prune(now);
        }
    }

    /// Remove the day files older than the retention.
    pub fn prune(&self, now: i64) {
        let oldest = {
            let mut state = self.state();
            state.pruned = day_of(now);
            oldest_kept(now, state.retention)
        };
        let Some(dir) = &self.dir else { return };
        if !std::fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_dir()) {
            return;
        }
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let name = entry.file_name();
            let Some(day) = name.to_str().and_then(day_file) else { continue };
            if day < oldest.as_str() {
                // Never follows a symlink: it is the link that goes.
                if let Err(err) = std::fs::remove_file(entry.path()) {
                    tracing::warn!(%err, file = %entry.path().display(), "cannot remove an old activity log file");
                }
            }
        }
    }

    /// Remove every day file (`ClearLog`) and forget what was coalesced.
    pub fn clear(&self) {
        let mut state = self.state();
        state.pending.clear();
        state.marked.clear();
        let Some(dir) = &self.dir else { return };
        if !std::fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_dir()) {
            return;
        }
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            if entry.path().extension().is_some_and(|e| e == "jsonl") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("helix-activity-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn lines(dir: &Path) -> Vec<serde_json::Value> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).collect();
        files.sort();
        files.iter().flat_map(|f| std::fs::read_to_string(f).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect::<Vec<_>>()).collect()
    }

    fn refused(time: i64, why: &str) -> Entry {
        Entry { time, agent: "/usr/bin/a".into(), method: "ListWindows".into(), level: Some(Level::ReadOnly), outcome: Outcome::Refused(why.into()), ..Entry::default() }
    }

    #[test]
    fn line_format() {
        let e = Entry {
            time: 1795082400,
            agent: "/usr/bin/node /home/me/agent.js".into(),
            name: "Example \"Agent\"".into(),
            method: "SearchFiles".into(),
            target: "file:///home/me/Documents".into(),
            level: Some(Level::ReadOnly),
            outcome: Outcome::Allowed,
            error: Some("TimedOut".into()),
            detail: vec![("query-length", Detail::Count(12))],
            ..Entry::default()
        };
        assert_eq!(
            e.line(),
            r#"{"time":1795082400,"agent":"/usr/bin/node /home/me/agent.js","name":"Example \"Agent\"","method":"SearchFiles","target":"file:///home/me/Documents","level":"read-only","outcome":"allowed","error":"TimedOut","query-length":12}"#
        );
        // What Settings' reader (helix-apps `agents::Entry::from_json`)
        // and `helix-agent log` take: an object, `time` an integer, the
        // rest strings; absent keys are empty.
        let v: serde_json::Value = serde_json::from_str(&e.line()).unwrap();
        assert_eq!(v["time"].as_i64(), Some(1795082400));
        for key in ["agent", "name", "method", "target", "level", "outcome"] {
            assert!(v[key].is_string(), "{key}");
        }
        let r = refused(5, "AccessOff").line();
        assert_eq!(r, r#"{"time":5,"agent":"/usr/bin/a","name":"","method":"ListWindows","level":"read-only","outcome":"refused:AccessOff"}"#);
        let c = Entry { time: 5, method: STOP.into(), outcome: Outcome::Cancelled, count: 3, until: 9, ..Entry::default() }.line();
        assert_eq!(c, r#"{"time":5,"agent":"","name":"","method":"Stop","outcome":"cancelled","count":3,"until":9}"#);
        let g = Entry { time: 5, method: CONSENT_PROMPT.into(), outcome: Outcome::Allowed, detail: vec![("grant", Detail::Kind("always"))], ..Entry::default() }.line();
        assert!(g.ends_with(r#""outcome":"allowed","grant":"always"}"#), "{g}");
    }

    #[test]
    fn outcomes_of_errors() {
        assert_eq!(outcome_of(&AgentError::AccessOff(String::new())), (Outcome::Refused("AccessOff".into()), None));
        assert_eq!(outcome_of(&AgentError::Excluded("settings".into())), (Outcome::Refused("Excluded".into()), None));
        assert_eq!(outcome_of(&AgentError::DeniedByUser(String::new())), (Outcome::Refused("DeniedByUser".into()), None));
        assert_eq!(outcome_of(&AgentError::Cancelled(String::new())), (Outcome::Cancelled, None));
        assert_eq!(outcome_of(&AgentError::TimedOut(String::new())), (Outcome::Allowed, Some("TimedOut")));
        assert_eq!(outcome_of(&AgentError::invalid_args("x")), (Outcome::Allowed, Some("InvalidArgs")));
    }

    #[test]
    fn reasons_of_errors() {
        for token in ["password-field", "system-prompt", "shell", "settings"] {
            assert_eq!(reason_of(&AgentError::Excluded(token.into())), Some(token));
        }
        // An excluded app's id stays out of the reason.
        assert_eq!(reason_of(&AgentError::Excluded("app:org.keepassxc.KeePassXC".into())), Some("app"));
        assert_eq!(reason_of(&AgentError::Excluded("something new".into())), None);
        for token in ["user-input", "cooldown", "stopped"] {
            assert_eq!(reason_of(&AgentError::Cancelled(token.into())), Some(token));
        }
        assert_eq!(reason_of(&AgentError::Cancelled("the user cancelled the question".into())), None);
        assert_eq!(reason_of(&AgentError::FeatureOff("clipboard".into())), Some("clipboard"));
        for err in [AgentError::AccessOff("agent access is turned off".into()), AgentError::Locked("the screen is locked".into()), AgentError::LevelTooLow("apps".into()), AgentError::Failed("user-input".into()), AgentError::NotFound("stopped".into())] {
            assert_eq!(reason_of(&err), None, "{err:?}");
        }
        // Written after the outcome; only for a call not allowed.
        let e = Entry { time: 5, method: "Input".into(), outcome: Outcome::Cancelled, reason: Some("user-input"), detail: vec![("done-count", Detail::Count(2))], ..Entry::default() };
        assert_eq!(e.line(), r#"{"time":5,"agent":"","name":"","method":"Input","outcome":"cancelled","reason":"user-input","done-count":2}"#);
        let e = Entry { outcome: Outcome::Refused("Excluded".into()), reason: Some("app"), ..refused(5, "Excluded") };
        assert!(e.line().ends_with(r#""outcome":"refused:Excluded","reason":"app"}"#), "{}", e.line());
        let e = Entry { outcome: Outcome::Allowed, reason: Some("app"), ..refused(5, "Excluded") };
        assert!(!e.line().contains("reason"));
    }

    #[test]
    fn targets_are_cleaned() {
        assert_eq!(clean_target("org.gnome.Maps\n\u{1b}[31m"), "org.gnome.Maps[31m");
        let long = "x".repeat(1000);
        assert_eq!(clean_target(&long).chars().count(), MAX_TARGET);
        let e = Entry { target: format!("a\nb{long}"), ..Entry::default() };
        assert!(!e.line().contains("\\n"));
        assert_eq!(e.line().lines().count(), 1);
    }

    #[test]
    fn days_and_names() {
        assert_eq!(day_file("2026-11-19.jsonl"), Some("2026-11-19"));
        for bad in ["2026-11-19.json", "notes.jsonl", "2026-1-019.jsonl", "../2026-11-19.jsonl", "2026-11-19.jsonl.tmp"] {
            assert_eq!(day_file(bad), None, "{bad}");
        }
        let now = 1795082400; // 2026-11-18/19 depending on the zone
        assert_eq!(oldest_kept(now, 1), day_of(now));
        assert!(oldest_kept(now, 30) < day_of(now));
        assert_eq!(oldest_kept(now, 2), day_of(now - 86400));
    }

    #[test]
    fn files_are_private_and_appended() {
        let dir = tmp("private");
        let log = Log::new(Some(dir.clone()));
        let e = Entry { time: crate::agent::now(), agent: "/a".into(), method: "GetState".into(), ..Entry::default() };
        log.record(e.clone());
        log.record(e.clone());
        assert_eq!(std::fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
        let file = dir.join(format!("{}.jsonl", day_of(e.time)));
        assert_eq!(std::fs::metadata(&file).unwrap().mode() & 0o777, 0o600);
        assert_eq!(lines(&dir).len(), 2, "allowed calls are not coalesced");
        // A torn last line (a crash mid-write) is ended first.
        std::fs::OpenOptions::new().append(true).open(&file).unwrap().write_all(b"{\"time\":1,\"ag").unwrap();
        log.record(e.clone());
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.ends_with(&format!("{{\"time\":1,\"ag\n{}\n", e.line())), "{text}");
        // Wider modes are narrowed.
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        log.record(e.clone());
        assert_eq!(std::fs::metadata(&file).unwrap().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn symlinks_are_not_followed() {
        let base = tmp("symlink");
        std::fs::create_dir_all(&base).unwrap();
        let target = base.join("elsewhere.txt");
        std::fs::write(&target, "precious\n").unwrap();
        let dir = base.join("agent-activity");
        std::fs::create_dir(&dir).unwrap();
        let e = Entry { time: crate::agent::now(), agent: "/a".into(), method: "GetState".into(), ..Entry::default() };
        std::os::unix::fs::symlink(&target, dir.join(format!("{}.jsonl", day_of(e.time)))).unwrap();
        let log = Log::new(Some(dir.clone()));
        log.record(e.clone());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "precious\n", "a symlinked day file is not written through");
        // A symlinked directory is refused too, and not cleared through.
        let real = base.join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("2026-01-01.jsonl"), "{}\n").unwrap();
        let linked = base.join("linked");
        std::os::unix::fs::symlink(&real, &linked).unwrap();
        let log = Log::new(Some(linked.clone()));
        log.record(e);
        log.clear();
        log.prune(crate::agent::now());
        let names: Vec<_> = std::fs::read_dir(&real).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(names, ["2026-01-01.jsonl"]);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn refusals_are_coalesced() {
        let dir = tmp("coalesce");
        let log = Log::new(Some(dir.clone()));
        let t = 1795082400;
        log.record(refused(t, "AccessOff"));
        for i in 1..=500 {
            log.record(refused(t + i % 50, "AccessOff"));
        }
        // Another refusal or target is its own line.
        log.record(refused(t + 2, "Locked"));
        let got = lines(&dir);
        assert_eq!(got.len(), 2, "{got:?}");
        // The minute is up: the counted ones are one line.
        log.flush(t + 30);
        assert_eq!(lines(&dir).len(), 2, "not yet");
        log.flush(t + COALESCE_SECS);
        let got = lines(&dir);
        assert_eq!(got.len(), 3, "{got:?}");
        let summary = &got[2];
        assert_eq!(summary["outcome"], "refused:AccessOff");
        assert_eq!(summary["count"], 500);
        assert_eq!(summary["time"].as_i64(), Some(t));
        assert_eq!(summary["until"].as_i64(), Some(t + 49));
        // A refusal after the minute starts anew, written at once.
        log.record(refused(t + 100, "AccessOff"));
        log.record(refused(t + 101, "AccessOff"));
        assert_eq!(lines(&dir).len(), 4);
        // One counted call is an ordinary line.
        log.flush(t + 200);
        let got = lines(&dir);
        assert_eq!(got.len(), 5);
        assert!(got[4].get("count").is_none());
        // Allowed calls are never coalesced.
        for _ in 0..3 {
            log.record(Entry { time: t, method: "ListWindows".into(), ..Entry::default() });
        }
        assert_eq!(lines(&dir).len(), 8);
        // GetStatus is not recorded at all.
        log.record(Entry { time: t, method: "GetStatus".into(), ..Entry::default() });
        assert_eq!(lines(&dir).len(), 8);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn day_files_are_capped() {
        let dir = tmp("cap");
        let log = Log::new(Some(dir.clone()));
        let t = 1795082400;
        let file = dir.join(format!("{}.jsonl", day_of(t)));
        std::fs::create_dir_all(&dir).unwrap();
        // Just under the refusals' cap.
        std::fs::write(&file, "x".repeat(REFUSED_CAP as usize - 10) + "\n").unwrap();
        log.record(refused(t, "Locked"));
        log.record(refused(t, "Remote"));
        let text = std::fs::read_to_string(&file).unwrap();
        let tail: Vec<&str> = text.lines().skip(1).collect();
        assert_eq!(tail.len(), 1, "one marker, then refusals are dropped");
        let marker: serde_json::Value = serde_json::from_str(tail[0]).unwrap();
        assert_eq!((marker["method"].as_str(), marker["dropped"].as_str()), (Some(LOG_FULL), Some("refusals")));
        // Allowed calls still go in.
        log.record(Entry { time: t, method: "GetState".into(), ..Entry::default() });
        assert!(std::fs::read_to_string(&file).unwrap().ends_with("\"method\":\"GetState\",\"outcome\":\"allowed\"}\n"));
        // Up to the day's cap.
        std::fs::OpenOptions::new().append(true).open(&file).unwrap().write_all(("y".repeat(DAY_CAP as usize - REFUSED_CAP as usize) + "\n").as_bytes()).unwrap();
        let before = std::fs::metadata(&file).unwrap().len();
        log.record(Entry { time: t, method: "GetState".into(), ..Entry::default() });
        log.record(Entry { time: t, method: "GetState".into(), ..Entry::default() });
        let after = std::fs::read_to_string(&file).unwrap();
        let last: serde_json::Value = serde_json::from_str(after.lines().last().unwrap()).unwrap();
        assert_eq!(last["dropped"], "all");
        assert!(std::fs::metadata(&file).unwrap().len() < before + 200, "only the marker");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_days_are_pruned() {
        let dir = tmp("prune");
        std::fs::create_dir_all(&dir).unwrap();
        let now = crate::agent::now();
        let today = day_of(now);
        let days_ago = |n: i64| day_of(now - n * 86400);
        for name in [today.clone(), days_ago(1), days_ago(5), days_ago(40), "2001-01-01".into()] {
            std::fs::write(dir.join(format!("{name}.jsonl")), "{}\n").unwrap();
        }
        std::fs::write(dir.join("notes.txt"), "").unwrap();
        std::fs::write(dir.join("1999-01-01.jsonl.bak"), "").unwrap();
        let log = Log::new(Some(dir.clone()));
        log.set_retention(30, now);
        let mut names: Vec<String> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().into_string().unwrap()).collect();
        names.sort();
        let mut want = vec!["1999-01-01.jsonl.bak".to_owned(), format!("{}.jsonl", days_ago(5)), format!("{}.jsonl", days_ago(1)), format!("{today}.jsonl"), "notes.txt".into()];
        want.sort();
        assert_eq!(names, want);
        // Shorter retention prunes at once; the same one does nothing.
        log.set_retention(1, now);
        assert!(!dir.join(format!("{}.jsonl", days_ago(1))).exists() && dir.join(format!("{today}.jsonl")).exists());
        // The day changing prunes again.
        log.set_retention(2, now);
        std::fs::write(dir.join(format!("{}.jsonl", days_ago(1))), "{}\n").unwrap();
        log.tick(now);
        assert!(dir.join(format!("{}.jsonl", days_ago(1))).exists(), "same day: not pruned again");
        log.tick(now + 86400);
        assert!(!dir.join(format!("{}.jsonl", days_ago(1))).exists());
        // Clearing removes the day files only.
        log.clear();
        let mut names: Vec<String> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().into_string().unwrap()).collect();
        names.sort();
        assert_eq!(names, ["1999-01-01.jsonl.bak", "notes.txt"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
