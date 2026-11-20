//! Talking to the user (helixdesktop/spec
//! `contracts/agent-access/README.md`, "Talk to the user methods";
//! helixdesktop/desktop#148): what `Ask` and `Confirm` show and how the
//! answer comes back, and the reminders `AddReminder` keeps.
//!
//! The question travels to the UI as a [`QuestionRequest`] and the answer
//! back only on its oneshot channel: no D-Bus method answers it, and the
//! UI shows it as the consent prompt is shown, out of synthetic input's
//! and accessibility actions' reach (see `ui/agent_question.rs`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use zbus::zvariant::OwnedValue;

use super::policy::AgentError;

/// The longest question and reminder text, in characters.
pub const QUESTION_MAX: usize = 500;
/// The most choices `Ask` offers, and the fewest.
pub const CHOICES_MAX: usize = 8;
pub const CHOICES_MIN: usize = 2;
/// The longest choice.
pub const CHOICE_MAX: usize = 64;
/// The longest typed answer (and `default`).
pub const ANSWER_MAX: usize = 1000;
/// The longest button label of `Confirm`.
pub const LABEL_MAX: usize = 24;
/// The most reminders an agent may have waiting.
pub const REMINDERS_MAX: usize = 20;
/// How far ahead a reminder may be (366 days), and how far back (a time
/// just past is shown at once).
pub const REMINDER_AHEAD: i64 = 366 * 86_400;
pub const REMINDER_BEHIND: i64 = 60;

/// Characters that reorder text around them (bidirectional embeddings,
/// overrides, isolates and marks): removed, so an agent's text cannot
/// rearrange what the dialog shows next to it.
fn reorders(c: char) -> bool {
    matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// `text` as plain text for the dialog or a notification: control
/// characters (line breaks too) become spaces, characters that reorder
/// text are removed, and the ends are trimmed.
pub fn clean(text: &str) -> String {
    let out: String = text.chars().filter(|c| !reorders(*c)).map(|c| if c.is_control() { ' ' } else { c }).collect();
    out.trim().to_owned()
}

/// [`clean`] `text`, not empty and at most `max` characters, or
/// `InvalidArgs` about `what`.
pub fn bounded(text: &str, what: &str, max: usize) -> Result<String, AgentError> {
    let text = clean(text);
    if text.is_empty() {
        return Err(AgentError::invalid_args(format!("{what} is empty")));
    }
    if text.chars().count() > max {
        return Err(AgentError::invalid_args(format!("{what} is longer than {max} characters")));
    }
    Ok(text)
}

/// What the dialog asks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dialog {
    /// A question answered by typing (`choices` empty, `default` filled
    /// in) or by picking a choice (`default`: the one focused first).
    Ask { question: String, choices: Vec<String>, default: String },
    /// Yes or no; empty labels are "Yes" and "No" in the user's language.
    Confirm { question: String, yes: String, no: String },
}

impl Dialog {
    pub fn question(&self) -> &str {
        match self {
            Dialog::Ask { question, .. } | Dialog::Confirm { question, .. } => question,
        }
    }
}

/// The user's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// The text typed.
    Text(String),
    /// The choice picked (its index).
    Choice(usize),
    /// `Confirm`'s yes or no.
    Yes(bool),
    /// Dismissed (Cancel, Escape), or gone unanswered (the screen locked).
    Cancelled,
}

fn text_option(options: &HashMap<String, OwnedValue>, key: &str) -> Result<Option<String>, AgentError> {
    options.get(key).map(|v| String::try_from(v.clone()).map_err(|_| AgentError::invalid_args(format!("“{key}” must be a string")))).transpose()
}

/// `Ask`'s question and options (`choices`, `default`).
pub fn ask_dialog(question: &str, options: &HashMap<String, OwnedValue>) -> Result<Dialog, AgentError> {
    let question = bounded(question, "the question", QUESTION_MAX)?;
    let choices = match options.get("choices") {
        None => Vec::new(),
        Some(v) => {
            let list = <Vec<String>>::try_from(v.clone()).map_err(|_| AgentError::invalid_args("“choices” must be a list of strings"))?;
            if !(CHOICES_MIN..=CHOICES_MAX).contains(&list.len()) {
                return Err(AgentError::invalid_args(format!("“choices” must have {CHOICES_MIN} to {CHOICES_MAX} answers")));
            }
            let mut out: Vec<String> = Vec::new();
            for choice in &list {
                let choice = bounded(choice, "a choice", CHOICE_MAX)?;
                if out.contains(&choice) {
                    return Err(AgentError::invalid_args("two choices are the same"));
                }
                out.push(choice);
            }
            out
        }
    };
    let default = match text_option(options, "default")? {
        None => String::new(),
        Some(d) if choices.is_empty() => {
            let d = clean(&d);
            if d.chars().count() > ANSWER_MAX {
                return Err(AgentError::invalid_args(format!("“default” is longer than {ANSWER_MAX} characters")));
            }
            d
        }
        Some(d) => {
            let d = clean(&d);
            if !choices.contains(&d) {
                return Err(AgentError::invalid_args("“default” is not one of the choices"));
            }
            d
        }
    };
    Ok(Dialog::Ask { question, choices, default })
}

/// `Confirm`'s question and options (`yes`, `no`).
pub fn confirm_dialog(question: &str, options: &HashMap<String, OwnedValue>) -> Result<Dialog, AgentError> {
    let question = bounded(question, "the question", QUESTION_MAX)?;
    let label = |key: &str| -> Result<String, AgentError> {
        match text_option(options, key)? {
            None => Ok(String::new()),
            Some(l) => bounded(&l, &format!("“{key}”"), LABEL_MAX),
        }
    };
    let (yes, no) = (label("yes")?, label("no")?);
    if !yes.is_empty() && yes == no {
        return Err(AgentError::invalid_args("“yes” and “no” are the same"));
    }
    Ok(Dialog::Confirm { question, yes, no })
}

/// The `timeout-ms` option: `None` (absent or 0) for no limit.
pub fn timeout(options: &HashMap<String, OwnedValue>) -> Result<Option<Duration>, AgentError> {
    match options.get("timeout-ms") {
        None => Ok(None),
        Some(v) => {
            let ms = u32::try_from(v.clone()).map_err(|_| AgentError::invalid_args("“timeout-ms” must be a number (u)"))?;
            Ok((ms > 0).then(|| Duration::from_millis(u64::from(ms))))
        }
    }
}

/// A question for the UI.
#[derive(Debug)]
pub struct QuestionRequest {
    /// Tells this question apart, to close it ([`crate::UiEvent::AgentQuestionClose`]).
    pub id: u64,
    /// The agent's display name ("" for none), and its id.
    pub name: String,
    pub agent: String,
    pub dialog: Dialog,
    pub reply: oneshot::Sender<Reply>,
}

/// A [`QuestionRequest`] that can travel in a [`crate::UiEvent`].
#[derive(Debug, Clone)]
pub struct QuestionSlot(Arc<Mutex<Option<QuestionRequest>>>);

impl QuestionSlot {
    pub fn new(request: QuestionRequest) -> Self {
        Self(Arc::new(Mutex::new(Some(request))))
    }

    pub fn take(&self) -> Option<QuestionRequest> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

impl PartialEq for QuestionSlot {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// A reminder an agent set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reminder {
    pub id: String,
    /// The agent's id, executable and display name when it was set.
    pub agent: String,
    #[serde(default)]
    pub executable: String,
    #[serde(default)]
    pub name: String,
    /// When it is due, and when it was set (Unix seconds).
    pub time: i64,
    pub text: String,
    #[serde(default)]
    pub added: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    version: u32,
    #[serde(default)]
    reminders: Vec<Reminder>,
}

/// `$XDG_STATE_HOME/helix/agent-reminders.json`.
pub fn default_path() -> PathBuf {
    let state = std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| gio::glib::home_dir().join(".local/state"));
    state.join("helix/agent-reminders.json")
}

/// A new reminder id: 16 random hex digits.
fn new_id() -> String {
    let mut bytes = [0u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        // Unique enough without randomness: the time.
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
        bytes = nanos.to_le_bytes();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Why a reminder could not be added.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddError {
    /// The time is too far back or ahead.
    Time,
    /// The agent has [`REMINDERS_MAX`] waiting.
    Full,
}

/// The reminders waiting to be shown, kept in a file (0600, written whole
/// through a temporary file and a rename) so they outlive the shell.
#[derive(Debug, Default)]
pub struct Reminders {
    path: Option<PathBuf>,
    list: Vec<Reminder>,
}

impl Reminders {
    /// Read them from `path` (`None`: memory only). An unreadable file is
    /// left alone until a reminder changes.
    pub fn load(path: Option<PathBuf>) -> Reminders {
        let list = path.as_deref().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|text| match serde_json::from_str::<File>(&text) {
            Ok(file) => Some(file.reminders),
            Err(err) => {
                tracing::warn!(%err, "cannot read the agents' reminders");
                None
            }
        });
        let mut list = list.unwrap_or_default();
        list.retain(|r| !r.id.is_empty() && !r.agent.is_empty());
        list.sort_by_key(|r| r.time);
        Reminders { path, list }
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        let file = File { version: 1, reminders: self.list.clone() };
        let text = serde_json::to_string_pretty(&file).unwrap_or_default() + "\n";
        if let Err(err) = super::grants::write_private(path, &text) {
            tracing::warn!(%err, path = %path.display(), "cannot keep the agents' reminders");
        }
    }

    /// Add a reminder of `agent` at `time` (checked against `now`); its id.
    pub fn add(&mut self, agent: &super::identity::Agent, time: i64, text: String, now: i64) -> Result<Reminder, AddError> {
        if time < now - REMINDER_BEHIND || time > now + REMINDER_AHEAD {
            return Err(AddError::Time);
        }
        if self.list.iter().filter(|r| r.agent == agent.id).count() >= REMINDERS_MAX {
            return Err(AddError::Full);
        }
        let mut id = new_id();
        while self.list.iter().any(|r| r.id == id) {
            id = new_id();
        }
        let reminder = Reminder { id, agent: agent.id.clone(), executable: agent.executable.clone(), name: agent.name.clone(), time, text, added: now };
        self.list.push(reminder.clone());
        self.list.sort_by_key(|r| r.time);
        self.save();
        Ok(reminder)
    }

    /// The reminders of `agent` (every agent's for `None`), by time.
    pub fn of(&self, agent: Option<&str>) -> Vec<Reminder> {
        self.list.iter().filter(|r| agent.is_none_or(|a| r.agent == a)).cloned().collect()
    }

    /// Remove reminder `id`, when it is `agent`'s (anyone's for `None`).
    pub fn cancel(&mut self, agent: Option<&str>, id: &str) -> Option<Reminder> {
        let at = self.list.iter().position(|r| r.id == id && agent.is_none_or(|a| r.agent == a))?;
        let gone = self.list.remove(at);
        self.save();
        Some(gone)
    }

    /// Take the reminders due at `now`.
    pub fn take_due(&mut self, now: i64) -> Vec<Reminder> {
        let (due, rest): (Vec<Reminder>, Vec<Reminder>) = std::mem::take(&mut self.list).into_iter().partition(|r| r.time <= now);
        self.list = rest;
        if !due.is_empty() {
            self.save();
        }
        due
    }

    /// When the next one is due.
    pub fn next(&self) -> Option<i64> {
        self.list.iter().map(|r| r.time).min()
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

/// The reminder's line in the log and in records: `reminder:<id>`.
pub fn log_target(id: &str) -> String {
    format!("reminder:{id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::Value;

    fn opts(pairs: &[(&str, Value<'static>)]) -> HashMap<String, OwnedValue> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), OwnedValue::try_from(v.clone()).unwrap())).collect()
    }

    fn kind<T: std::fmt::Debug>(r: Result<T, AgentError>) -> String {
        match r {
            Ok(v) => format!("ok {v:?}"),
            Err(e) => e.kind().to_owned(),
        }
    }

    fn agent(id: &str) -> super::super::identity::Agent {
        super::super::identity::Agent { id: id.into(), executable: id.into(), name: "Bot".into(), identified: true, pid: 1, caller_pid: 1, ssh: false }
    }

    #[test]
    fn plain_text() {
        assert_eq!(clean("  Deploy\nnow?\t "), "Deploy now?");
        // Reordering characters go; the rest stays.
        assert_eq!(clean("a\u{202E}cba\u{2066}x\u{2069}"), "acbax");
        assert_eq!(clean("<b>bold</b>"), "<b>bold</b>", "no markup: shown as written");
        assert_eq!(kind(bounded(" \n ", "the question", 10)), "InvalidArgs");
        assert_eq!(kind(bounded(&"é".repeat(11), "the question", 10)), "InvalidArgs");
        assert_eq!(bounded(&"é".repeat(10), "the question", 10).unwrap().chars().count(), 10, "characters, not bytes");
    }

    #[test]
    fn ask_options() {
        let d = ask_dialog("Which branch?", &opts(&[])).unwrap();
        assert_eq!(d, Dialog::Ask { question: "Which branch?".into(), choices: vec![], default: String::new() });
        let d = ask_dialog("Which?", &opts(&[("choices", Value::from(vec!["main", "dev"])), ("default", Value::from("dev"))])).unwrap();
        assert_eq!(d, Dialog::Ask { question: "Which?".into(), choices: vec!["main".into(), "dev".into()], default: "dev".into() });
        assert_eq!(kind(ask_dialog("Which?", &opts(&[("choices", Value::from(vec!["only"]))]))), "InvalidArgs", "at least two");
        assert_eq!(kind(ask_dialog("Which?", &opts(&[("choices", Value::from(vec!["a"; 9]))]))), "InvalidArgs");
        assert_eq!(kind(ask_dialog("Which?", &opts(&[("choices", Value::from(vec!["a", "a"]))]))), "InvalidArgs", "no two the same");
        assert_eq!(kind(ask_dialog("Which?", &opts(&[("choices", Value::from(vec!["a", ""]))]))), "InvalidArgs");
        assert_eq!(kind(ask_dialog("Which?", &opts(&[("choices", Value::from(vec!["a", "b"])), ("default", Value::from("c"))]))), "InvalidArgs");
        assert_eq!(kind(ask_dialog("Which?", &opts(&[("choices", Value::from("a"))]))), "InvalidArgs");
        assert_eq!(kind(ask_dialog(&"q".repeat(QUESTION_MAX + 1), &opts(&[]))), "InvalidArgs");
        assert_eq!(kind(ask_dialog("Name?", &opts(&[("default", Value::from("x".repeat(ANSWER_MAX + 1)))]))), "InvalidArgs");
    }

    #[test]
    fn confirm_options() {
        assert_eq!(confirm_dialog("Go?", &opts(&[])).unwrap(), Dialog::Confirm { question: "Go?".into(), yes: String::new(), no: String::new() });
        let d = confirm_dialog("Deploy?", &opts(&[("yes", Value::from("Deploy")), ("no", Value::from("Not now"))])).unwrap();
        assert_eq!(d, Dialog::Confirm { question: "Deploy?".into(), yes: "Deploy".into(), no: "Not now".into() });
        assert_eq!(kind(confirm_dialog("Go?", &opts(&[("yes", Value::from("x".repeat(LABEL_MAX + 1)))]))), "InvalidArgs");
        assert_eq!(kind(confirm_dialog("Go?", &opts(&[("yes", Value::from("Same")), ("no", Value::from("Same"))]))), "InvalidArgs");
        assert_eq!(kind(confirm_dialog("Go?", &opts(&[("no", Value::from(" "))]))), "InvalidArgs");
    }

    #[test]
    fn timeouts() {
        assert_eq!(timeout(&opts(&[])).unwrap(), None);
        assert_eq!(timeout(&opts(&[("timeout-ms", Value::from(0u32))])).unwrap(), None);
        assert_eq!(timeout(&opts(&[("timeout-ms", Value::from(1500u32))])).unwrap(), Some(Duration::from_millis(1500)));
        assert_eq!(kind(timeout(&opts(&[("timeout-ms", Value::from(-1i32))]))), "InvalidArgs");
    }

    #[test]
    fn reminders_persist_privately() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("helix-reminders-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("helix/agent-reminders.json");
        let now = 1_800_000_000;
        let mut r = Reminders::load(Some(path.clone()));
        let a = r.add(&agent("/usr/bin/a"), now + 600, "Stand up".into(), now).unwrap();
        let b = r.add(&agent("/usr/bin/b"), now + 60, "Tea".into(), now).unwrap();
        assert_eq!(a.id.len(), 16);
        assert_ne!(a.id, b.id);
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        // Read again (a shell restart): by time.
        let mut again = Reminders::load(Some(path.clone()));
        assert_eq!(again.of(None).iter().map(|r| r.text.as_str()).collect::<Vec<_>>(), ["Tea", "Stand up"]);
        assert_eq!(again.next(), Some(now + 60));
        // An agent sees and cancels only its own.
        assert_eq!(again.of(Some("/usr/bin/a")).len(), 1);
        assert_eq!(again.cancel(Some("/usr/bin/a"), &b.id), None);
        assert_eq!(again.take_due(now + 59), vec![]);
        assert_eq!(again.take_due(now + 60), vec![b.clone()]);
        assert_eq!(Reminders::load(Some(path.clone())).of(None), vec![a.clone()], "shown ones are gone from the file");
        assert_eq!(again.cancel(None, &a.id), Some(a));
        assert!(Reminders::load(Some(path)).of(None).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reminder_limits() {
        let now = 1_800_000_000;
        let mut r = Reminders::load(None);
        let me = agent("/usr/bin/a");
        assert_eq!(r.add(&me, now - REMINDER_BEHIND - 1, "x".into(), now), Err(AddError::Time));
        assert_eq!(r.add(&me, now + REMINDER_AHEAD + 1, "x".into(), now), Err(AddError::Time));
        assert!(r.add(&me, now - 5, "just past".into(), now).is_ok());
        for _ in 1..REMINDERS_MAX {
            r.add(&me, now + 100, "x".into(), now).unwrap();
        }
        assert_eq!(r.add(&me, now + 100, "x".into(), now), Err(AddError::Full));
        assert!(r.add(&agent("/usr/bin/b"), now + 100, "x".into(), now).is_ok(), "the cap is per agent");
    }
}
