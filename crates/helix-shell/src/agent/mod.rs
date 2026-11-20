//! Agent Access: the shell's single choke point for agents
//! (helixdesktop/spec `contracts/agent-access/README.md`).
//!
//! `org.helixdesktop.Agent1` is what agents (and `helix-agent`) call; every
//! method runs [`AgentAccess::check`] first: not sandboxed, identified,
//! switched on, not remote, not locked, within the configured level, the
//! feature switches, a grant (asking the user through the consent hook),
//! then sensitive actions. `org.helixdesktop.AgentAccess1` is the Settings
//! page's: list agents and take access away, for the Settings app only. No
//! D-Bus method grants access; grants come only from the consent prompt.
//!
//! The consent prompt and the sensitive-action prompt are hooks
//! ([`Consent`], [`Confirm`]); until the shell draws them
//! (helixdesktop/desktop#143) nobody can be asked, so calls without a grant
//! fail with `NotGranted`. The shell's UI fills them in ([`prompt`]) and
//! shows the indicator ([`AgentAccess::set_indicator`]). The activity log
//! ([`activity`], helixdesktop/desktop#144) is written from the check: a
//! refusal at once, an allowed call when its [`Caller`] is finished or
//! dropped, so every method is recorded without doing anything itself.
//!
//! The *Read only* methods (helixdesktop/desktop#146) read the UI's
//! windows, workspaces and monitors ([`desktop`]), the notification list,
//! the quick settings' state, recent files and file search ([`files`]),
//! and what document a window shows ([`document`]). The *Control the
//! desktop* methods (helixdesktop/desktop#147) open and launch through the
//! shell's launcher, act on windows and workspaces through the UI's window
//! list, change quick settings and show notifications ([`actions`]).
//! The *talk to the user* methods (helixdesktop/desktop#148) ask in a
//! dialog only the user at the machine can answer, and keep reminders
//! that come due as notifications ([`talk`]). The *Control apps* methods
//! (helixdesktop/desktop#149) read and use apps' controls over AT-SPI,
//! with the shell as the only client agents reach ([`a11y`]).

pub mod a11y;
pub mod actions;
pub mod activity;
pub mod desktop;
pub mod document;
pub mod files;
pub mod grants;
pub mod identity;
pub mod policy;
pub mod prompt;
pub mod talk;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{fdo, interface, Connection};

use crate::caller;
use crate::screensaver::{self, SharedStatus};
use grants::{Answer, Decision, Grants};
use identity::Agent;
pub use policy::{AgentError, Config, Level, Need, Target};
use policy::{Facts, RememberMode};

pub const OBJECT_PATH: &str = "/org/helixdesktop/Agent1";
/// Agent Access's version, as `GetStatus` says.
pub const VERSION: u32 = 1;

/// What the consent prompt is asked.
#[derive(Debug, Clone)]
pub struct ConsentRequest {
    pub agent: Agent,
    /// The level to approve.
    pub level: Level,
    /// Offer *Always allow* (`remember-mode` is `remember` and the agent is
    /// identified).
    pub offer_always: bool,
}

/// The consent prompt: the user's answer, or [`Answer::Unanswered`].
pub type Consent = Arc<dyn Fn(ConsentRequest) -> BoxFuture<'static, Answer> + Send + Sync>;

/// What the sensitive-action prompt is asked.
#[derive(Debug, Clone)]
pub struct ConfirmRequest {
    pub agent: Agent,
    /// The `Agent1` method.
    pub method: &'static str,
    /// What it does, when the prompt can say more (`wifi-off`, `close`).
    pub action: Option<&'static str>,
    /// The app control it uses (*Control apps*), for the prompt to name.
    pub control: Option<Control>,
}

/// An app's control a sensitive call uses, as the prompt names it: the
/// control's accessible name (plain text, at most 64 characters), its
/// role, and the app's id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Control {
    pub name: String,
    pub role: &'static str,
    pub app: String,
}

/// The sensitive-action prompt: `Some(yes)`, or `None` when the user could
/// not be asked.
pub type Confirm = Arc<dyn Fn(ConfirmRequest) -> BoxFuture<'static, Option<bool>> + Send + Sync>;

/// Nobody to ask yet (helixdesktop/desktop#143): every prompt goes
/// unanswered.
pub fn no_consent() -> Consent {
    Arc::new(|_| Box::pin(async { Answer::Unanswered }))
}

/// Nobody to ask yet (helixdesktop/desktop#143).
pub fn no_confirm() -> Confirm {
    Arc::new(|_| Box::pin(async { None }))
}

/// How [`AgentAccess`] is set up.
pub struct Setup {
    /// Where remembered grants live (`None`: memory only).
    pub grants_path: Option<PathBuf>,
    pub consent: Consent,
    pub confirm: Confirm,
    /// `/proc`.
    pub proc_root: PathBuf,
    /// Executables accepted as the Settings app.
    pub settings_exes: Vec<PathBuf>,
    /// Where the activity log goes (`None`: nowhere).
    pub log_dir: Option<PathBuf>,
    /// Where agents' reminders are kept (`None`: memory only).
    pub reminders_path: Option<PathBuf>,
}

impl Default for Setup {
    fn default() -> Self {
        Setup {
            grants_path: Some(grants::default_path()),
            consent: no_consent(),
            confirm: no_confirm(),
            proc_root: PathBuf::from("/proc"),
            settings_exes: settings_executables(),
            log_dir: Some(log_dir()),
            reminders_path: Some(talk::default_path()),
        }
    }
}

/// `helix-settings-app` beside the shell's own executable, and in
/// `/usr/bin` (README.md, "Who may manage grants").
pub fn settings_executables() -> Vec<PathBuf> {
    let mut exes = Vec::new();
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        exes.push(dir.join("helix-settings-app"));
    }
    let usr = PathBuf::from("/usr/bin/helix-settings-app");
    if !exes.contains(&usr) {
        exes.push(usr);
    }
    exes
}

/// The common options of `Agent1` methods.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallOptions {
    /// The shell may ask the user during this call.
    pub interactive: bool,
    /// The agent marked the call sensitive.
    pub sensitive: bool,
    /// What the call does, for the sensitive-action prompt (not an
    /// option: the method sets it).
    pub action: Option<&'static str>,
}

impl CallOptions {
    pub fn from_dict(options: &HashMap<String, OwnedValue>) -> CallOptions {
        let flag = |key: &str, default: bool| options.get(key).and_then(|v| bool::try_from(v.clone()).ok()).unwrap_or(default);
        CallOptions { interactive: flag("interactive", true), sensitive: flag("sensitive", false), action: None }
    }
}

impl Default for CallOptions {
    fn default() -> Self {
        CallOptions { interactive: true, sensitive: false, action: None }
    }
}

/// A caller that passed the check. Its call's line in the activity log
/// is written when it is [finished](Caller::finish), or dropped (then as
/// allowed, or as the exclusion [`Caller::check_target`] refused).
#[derive(Debug)]
pub struct Caller {
    pub agent: Agent,
    /// The settings the check used.
    pub config: Config,
    entry: Mutex<Option<activity::Entry>>,
    log: Arc<activity::Log>,
}

impl Caller {
    fn entry(&self) -> MutexGuard<'_, Option<activity::Entry>> {
        self.entry.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Record what the call acts on: an app id, `window:<id> (<app id>)`,
    /// a file URI; never contents.
    pub fn target(&self, target: impl Into<String>) {
        if let Some(e) = self.entry().as_mut() {
            e.target = target.into();
        }
    }

    /// Record a kind or length of what the agent sent (`text-length`, …).
    pub fn detail(&self, key: &'static str, value: activity::Detail) {
        if let Some(e) = self.entry().as_mut() {
            e.detail.push((key, value));
        }
    }

    /// Step 9: check `target` against the exclusions, recording it (and
    /// the refusal).
    pub fn check_target(&self, target: Target<'_>) -> Result<(), AgentError> {
        if let (Target::App(app), Some(e)) = (target, self.entry().as_mut()) {
            if e.target.is_empty() {
                e.target = app.to_owned();
            }
        }
        let result = policy::check_target(&self.config, target);
        if let (Err(err), Some(e)) = (&result, self.entry().as_mut()) {
            (e.outcome, _) = activity::outcome_of(err);
        }
        result
    }

    /// The call's result, recorded.
    pub fn finish<T>(self, result: Result<T, AgentError>) -> Result<T, AgentError> {
        if let (Err(err), Some(e)) = (&result, self.entry().as_mut()) {
            let (outcome, error) = activity::outcome_of(err);
            e.outcome = outcome;
            e.error = error.map(str::to_owned);
        }
        result
    }
}

impl Drop for Caller {
    fn drop(&mut self) {
        if let Some(entry) = self.entry().take() {
            self.log.record(entry);
        }
    }
}

struct Inner {
    grants: Mutex<Grants>,
    lock: SharedStatus,
    consent: Consent,
    confirm: Confirm,
    /// One prompt at a time.
    prompt: tokio::sync::Mutex<()>,
    /// The system bus, for logind.
    system: tokio::sync::Mutex<Option<Connection>>,
    proc_root: PathBuf,
    settings_exes: Vec<PathBuf>,
    /// The connection serving the interfaces, for signals.
    conn: OnceLock<Connection>,
    /// Where the indicator's agents go (the UI), and what it was told last.
    indicator: OnceLock<tokio::sync::mpsc::UnboundedSender<crate::UiEvent>>,
    shown: Mutex<Vec<prompt::Active>>,
    /// Calls running now (waits, questions, input), by agent id.
    running: Mutex<HashMap<String, usize>>,
    log: Arc<activity::Log>,
    /// Where the desktop's state comes from (the UI, the notifications).
    desktop: OnceLock<Desktop>,
    /// Agents' notifications, for the rate limit.
    notify_limit: Mutex<actions::NotifyLimit>,
    /// Each agent's questions, one at a time.
    turns: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// The id of the next question.
    next_question: std::sync::atomic::AtomicU64,
    /// Reminders waiting to be shown, and what wakes their timer.
    reminders: Mutex<talk::Reminders>,
    reminders_changed: Arc<tokio::sync::Notify>,
    /// Reminders added, for their rate limit.
    reminder_limit: Mutex<actions::NotifyLimit>,
    /// The "Reminder set" notification of each waiting reminder.
    reminder_notes: Mutex<HashMap<String, u32>>,
    /// The accessibility bus, for the *Control apps* methods.
    a11y: a11y::Bus,
}

/// Where the *Read only* methods read the desktop's state.
#[derive(Clone)]
pub struct Desktop {
    /// The UI, which answers [`UiEvent::AgentSnapshot`](crate::UiEvent::AgentSnapshot).
    pub ui: tokio::sync::mpsc::UnboundedSender<crate::UiEvent>,
    pub notifications: Option<crate::notifications::Center>,
}

/// How often an open question checks that its agent still has access.
const QUESTION_POLL: Duration = Duration::from_millis(500);
/// The longest the reminders' timer sleeps.
const REMINDER_POLL: Duration = Duration::from_secs(30);

/// Whether the bus still has `name` (a caller that went away has not).
async fn name_has_owner(conn: &Connection, name: &zbus::names::UniqueName<'_>) -> bool {
    let Ok(dbus) = fdo::DBusProxy::new(conn).await else { return true };
    dbus.name_has_owner(zbus::names::BusName::Unique(name.to_owned())).await.unwrap_or(true)
}

/// A Reminder record (README.md, "Records"); for Settings, with whose it is.
fn reminder_record(r: &talk::Reminder, whose: bool) -> HashMap<String, OwnedValue> {
    let mut out = HashMap::from([("id".to_owned(), value(r.id.as_str())), ("time".to_owned(), value(r.time)), ("text".to_owned(), value(r.text.as_str()))]);
    if whose {
        out.insert("agent".to_owned(), value(r.agent.as_str()));
        out.insert("executable".to_owned(), value(r.executable.as_str()));
        out.insert("name".to_owned(), value(r.name.as_str()));
    }
    out
}

/// How often waits look at the windows again.
const WAIT_POLL: Duration = Duration::from_millis(200);
/// How long a window or workspace action waits to see it done.
const SETTLE: Duration = Duration::from_secs(2);

/// Agent Access's state: grants and the policy check.
#[derive(Clone)]
pub struct AgentAccess {
    inner: Arc<Inner>,
}

pub(crate) fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

fn my_uid() -> u32 {
    // SAFETY: getuid cannot fail.
    unsafe { libc::getuid() }
}

pub(crate) fn value<'a>(v: impl Into<Value<'a>>) -> OwnedValue {
    OwnedValue::try_from(v.into()).expect("plain values hold no file descriptors")
}

/// The process id of the caller of `header`, if it is of our user.
async fn caller_pid(conn: &Connection, header: &Header<'_>) -> Option<u32> {
    let sender = header.sender()?;
    let dbus = fdo::DBusProxy::new(conn).await.ok()?;
    let creds = dbus.get_connection_credentials(zbus::names::BusName::Unique(sender.to_owned())).await.ok()?;
    if creds.unix_user_id() != Some(my_uid()) {
        return None;
    }
    creds.process_id()
}

/// Whether logind puts `pid` in a remote session: `Some(false)` for no
/// session at all, `None` when logind cannot be asked.
async fn session_remote(system: &Connection, pid: u32) -> Option<bool> {
    let ask = async {
        let reply = system
            .call_method(Some("org.freedesktop.login1"), "/org/freedesktop/login1", Some("org.freedesktop.login1.Manager"), "GetSessionByPID", &(pid,))
            .await;
        let path: OwnedObjectPath = match reply {
            Ok(reply) => reply.body().deserialize().ok()?,
            Err(zbus::Error::MethodError(name, _, _)) if name.as_str() == "org.freedesktop.login1.NoSessionForPID" => return Some(false),
            Err(err) => {
                tracing::debug!(%err, pid, "cannot ask logind for the session");
                return None;
            }
        };
        let reply = system
            .call_method(Some("org.freedesktop.login1"), path.as_str(), Some("org.freedesktop.DBus.Properties"), "Get", &("org.freedesktop.login1.Session", "Remote"))
            .await
            .ok()?;
        let remote: OwnedValue = reply.body().deserialize().ok()?;
        bool::try_from(remote).ok()
    };
    tokio::time::timeout(Duration::from_secs(3), ask).await.ok().flatten()
}

/// The executable of the caller of `header` when it is of our user, for
/// the log of a caller that was not identified.
async fn caller_exe(conn: &Connection, header: &Header<'_>, proc_root: &Path) -> String {
    let Some(pid) = caller_pid(conn, header).await else { return String::new() };
    std::fs::read_link(proc_root.join(pid.to_string()).join("exe")).map(|e| e.to_string_lossy().trim_end_matches(" (deleted)").to_owned()).unwrap_or_default()
}

/// `$XDG_STATE_HOME/helix/agent-activity`.
pub fn log_dir() -> PathBuf {
    let state = std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| gio::glib::home_dir().join(".local/state"));
    state.join("helix/agent-activity")
}

impl AgentAccess {
    pub fn new(lock: SharedStatus, setup: Setup) -> AgentAccess {
        let grants = Grants::load(setup.grants_path);
        AgentAccess {
            inner: Arc::new(Inner {
                grants: Mutex::new(grants),
                lock,
                consent: setup.consent,
                confirm: setup.confirm,
                prompt: Default::default(),
                system: Default::default(),
                proc_root: setup.proc_root,
                settings_exes: setup.settings_exes,
                conn: OnceLock::new(),
                indicator: OnceLock::new(),
                shown: Mutex::new(Vec::new()),
                running: Mutex::new(HashMap::new()),
                desktop: OnceLock::new(),
                notify_limit: Mutex::new(actions::NotifyLimit::default()),
                turns: Mutex::new(HashMap::new()),
                next_question: std::sync::atomic::AtomicU64::new(1),
                reminders: Mutex::new(talk::Reminders::load(setup.reminders_path)),
                reminders_changed: Default::default(),
                reminder_limit: Mutex::new(actions::NotifyLimit::default()),
                reminder_notes: Mutex::new(HashMap::new()),
                a11y: a11y::Bus::default(),
                log: Arc::new(activity::Log::new(setup.log_dir)),
            }),
        }
    }

    fn grants(&self) -> MutexGuard<'_, Grants> {
        self.inner.grants.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Use `conn` (serving the interfaces) for signals, and follow the
    /// settings: turning access off ends every session grant at once.
    pub fn attach(&self, conn: &Connection) {
        let _ = self.inner.conn.set(conn.clone());
        // Prune at start, then each minute write coalesced refusals and
        // prune when the day changes.
        self.inner.log.set_retention(Config::read().log_retention_days, now());
        let log = Arc::downgrade(&self.inner.log);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            tick.tick().await;
            loop {
                tick.tick().await;
                let Some(log) = log.upgrade() else { return };
                tokio::task::spawn_blocking(move || log.tick(now()));
            }
        });
        self.start_reminders();
        let access = self.clone();
        tokio::spawn(async move {
            if !helix_settings::has_schema(policy::SCHEMA) {
                tracing::warn!(schema = policy::SCHEMA, "not installed; agent access stays off");
                return;
            }
            let settings = helix_settings::GSettings::new(&[policy::SCHEMA]);
            let (mut changes, _watch) = settings.watch();
            while changes.recv().await.is_some() {
                access.settings_changed(&Config::read());
            }
        });
    }

    /// Apply changed settings.
    pub fn settings_changed(&self, config: &Config) {
        self.inner.log.set_retention(config.log_retention_days, now());
        if !config.enabled && self.grants().revoke_all() {
            tracing::info!("agent access turned off; session grants ended");
        }
        self.changed();
    }

    /// Tell agents and Settings that something changed.
    pub fn changed(&self) {
        self.publish();
        let Some(conn) = self.inner.conn.get().cloned() else { return };
        tokio::spawn(async move {
            if let Ok(emitter) = SignalEmitter::new(&conn, OBJECT_PATH) {
                let _ = Agent1::access_changed(&emitter).await;
                let _ = AgentAccessSettings::agents_changed(&emitter).await;
            }
        });
    }

    /// Whether the screen is locked or locking.
    fn locked(&self) -> bool {
        let status = screensaver::status(&self.inner.lock);
        status.locked() || status.requested
    }

    async fn system_bus(&self) -> Option<Connection> {
        let mut system = self.inner.system.lock().await;
        if system.is_none() {
            *system = Connection::system().await.map_err(|err| tracing::debug!(%err, "no system bus; agents count as remote")).ok();
        }
        system.clone()
    }

    /// Whether `agent` is in a remote session (README.md, "Remote
    /// sessions"); when logind cannot be asked, it is.
    pub async fn remote(&self, agent: &Agent) -> bool {
        if agent.ssh {
            return true;
        }
        let Some(system) = self.system_bus().await else { return true };
        for pid in [agent.caller_pid, agent.pid] {
            if session_remote(&system, pid).await != Some(false) {
                return true;
            }
        }
        false
    }

    /// Steps 3–7 of the check, asking logind only when it matters.
    async fn gate(&self, agent: &Agent, config: &Config, need: Need) -> Result<(), AgentError> {
        if need.level.is_none() || !config.enabled {
            return policy::gate(config, need, Facts::default());
        }
        let remote = !config.allow_remote && self.remote(agent).await;
        policy::gate(config, need, Facts { remote, locked: self.locked() })
    }

    /// The activity log.
    pub fn log(&self) -> &activity::Log {
        &self.inner.log
    }

    /// A line about `agent` for the activity log.
    fn entry(agent: &Agent, method: &str, level: Option<Level>, outcome: activity::Outcome) -> activity::Entry {
        activity::Entry { time: now(), agent: agent.id.clone(), name: agent.name.clone(), method: method.to_owned(), level, outcome, ..activity::Entry::default() }
    }

    /// The policy check every `Agent1` method runs first (README.md, "The
    /// policy check"). A refusal is recorded in the activity log at once;
    /// an allowed call when the returned [`Caller`] is finished or dropped.
    pub async fn check(&self, conn: &Connection, header: &Header<'_>, method: &'static str, need: Need, options: CallOptions) -> Result<Caller, AgentError> {
        let time = now();
        let mut agent = None;
        let result = self.check_steps(conn, header, method, need, options, &mut agent).await;
        let mut entry = match &agent {
            Some(a) => Self::entry(a, method, need.level, activity::Outcome::Allowed),
            None => activity::Entry { agent: caller_exe(conn, header, &self.inner.proc_root).await, method: method.to_owned(), level: need.level, ..activity::Entry::default() },
        };
        entry.time = time;
        match result {
            Ok((agent, config)) => Ok(Caller { agent, config, entry: Mutex::new(Some(entry)), log: self.inner.log.clone() }),
            Err(err) => {
                (entry.outcome, _) = activity::outcome_of(&err);
                self.inner.log.record(entry);
                Err(err)
            }
        }
    }

    async fn check_steps(&self, conn: &Connection, header: &Header<'_>, method: &'static str, need: Need, options: CallOptions, who: &mut Option<Agent>) -> Result<(Agent, Config), AgentError> {
        let sandboxed = || AgentError::Sandboxed("not available to sandboxed apps".into());
        caller::require_unsandboxed(conn, header).await.map_err(|_| sandboxed())?;
        let pid = caller_pid(conn, header).await.ok_or_else(sandboxed)?;
        let agent = identity::identify(&self.inner.proc_root, pid, my_uid()).ok_or_else(sandboxed)?;
        *who = Some(agent.clone());
        let mut config = Config::read();
        let Some(level) = need.level else { return Ok((agent, config)) };
        self.gate(&agent, &config, need).await?;
        let decision = self.grants().decide(&agent.id, level, &config);
        match decision {
            Decision::Allowed => {}
            Decision::Denied => return Err(AgentError::DeniedByUser("the user denied this agent access".into())),
            Decision::Ask => config = self.ask_consent(&agent, need, options).await?,
        }
        if let Some(after) = self.sensitive_step(&agent, &config, method, need, options, None).await? {
            config = after;
        }
        self.grants().touch(&agent, now());
        self.publish();
        Ok((agent, config))
    }

    /// Step 10: a sensitive call (with `confirm-sensitive` on) asks the
    /// user first, and is checked again after the answer; the settings in
    /// force then, or `None` when nobody had to be asked.
    async fn sensitive_step(&self, agent: &Agent, config: &Config, method: &'static str, need: Need, options: CallOptions, control: Option<Control>) -> Result<Option<Config>, AgentError> {
        if !policy::sensitive_step(config, need, options.sensitive, options.interactive)? {
            return Ok(None);
        }
        let _turn = self.inner.prompt.lock().await;
        let answer = (self.inner.confirm)(ConfirmRequest { agent: agent.clone(), method, action: options.action, control }).await;
        if let Some(yes) = answer {
            let outcome = if yes { activity::Outcome::Allowed } else { activity::Outcome::Refused("DeniedByUser".into()) };
            let detail = options.action.map(|a| ("action", activity::Detail::Kind(a))).into_iter().collect();
            self.inner.log.record(activity::Entry { target: method.to_owned(), detail, ..Self::entry(agent, activity::SENSITIVE_PROMPT, need.level, outcome) });
        }
        match answer {
            Some(true) => {}
            Some(false) => return Err(AgentError::DeniedByUser("the user declined this action".into())),
            None => return Err(AgentError::NeedsConfirmation("the user could not be asked to confirm".into())),
        }
        let config = Config::read();
        self.gate(agent, &config, need).await?;
        Ok(Some(config))
    }

    /// The policy check of a method that names a target: steps 1–8 now,
    /// and step 10 (sensitive actions) left for [`Self::confirm`] once
    /// the method has checked its target (step 9), so the user is never
    /// asked about something the exclusions refuse anyway.
    pub async fn check_targeted(&self, conn: &Connection, header: &Header<'_>, method: &'static str, need: Need, options: CallOptions) -> Result<Caller, AgentError> {
        self.check(conn, header, method, Need { sensitive: false, ..need }, CallOptions { sensitive: false, ..options }).await
    }

    /// Step 10 for a caller of [`Self::check_targeted`].
    pub async fn confirm(&self, caller: &Caller, method: &'static str, need: Need, options: CallOptions) -> Result<(), AgentError> {
        self.sensitive_step(&caller.agent, &caller.config, method, need, options, None).await.map(drop)
    }

    /// [`Self::confirm`] for using an app's `control`, which the prompt
    /// names.
    pub async fn confirm_control(&self, caller: &Caller, method: &'static str, need: Need, options: CallOptions, control: Control) -> Result<(), AgentError> {
        self.sensitive_step(&caller.agent, &caller.config, method, need, options, Some(control)).await.map(drop)
    }

    /// Step 8 without a grant: ask the user (one prompt at a time), record
    /// the answer, and check again. The settings in force afterwards.
    async fn ask_consent(&self, agent: &Agent, need: Need, options: CallOptions) -> Result<Config, AgentError> {
        let not_granted = || AgentError::NotGranted("this agent has no grant and the user could not be asked".into());
        let level = need.level.unwrap_or(Level::ReadOnly);
        if !options.interactive {
            return Err(not_granted());
        }
        let _turn = self.inner.prompt.lock().await;
        // Another call may have been answered meanwhile.
        let config = Config::read();
        self.gate(agent, &config, need).await?;
        let decision = self.grants().decide(&agent.id, level, &config);
        match decision {
            Decision::Allowed => return Ok(config),
            Decision::Denied => return Err(AgentError::DeniedByUser("the user denied this agent access".into())),
            Decision::Ask => {}
        }
        let ask = policy::level_to_ask(&config, need);
        let offer_always = config.remember == RememberMode::Remember && agent.identified;
        let mut answer = (self.inner.consent)(ConsentRequest { agent: agent.clone(), level: ask, offer_always }).await;
        if answer == Answer::Always && !offer_always {
            answer = Answer::Session;
        }
        self.grants().answer(agent, ask, answer, now());
        let (outcome, grant) = match answer {
            Answer::Session => (activity::Outcome::Allowed, Some("session")),
            Answer::Always => (activity::Outcome::Allowed, Some("always")),
            Answer::Deny => (activity::Outcome::Refused("DeniedByUser".into()), None),
            Answer::Unanswered => (activity::Outcome::Cancelled, None),
        };
        let detail = grant.map(|g| ("grant", activity::Detail::Kind(g))).into_iter().collect();
        self.inner.log.record(activity::Entry { detail, ..Self::entry(agent, activity::CONSENT_PROMPT, Some(ask), outcome) });
        match answer {
            Answer::Unanswered => return Err(not_granted()),
            Answer::Deny => {
                self.changed();
                return Err(AgentError::DeniedByUser("the user denied this agent access".into()));
            }
            Answer::Session | Answer::Always => self.changed(),
        }
        // The world may have changed while the prompt was open.
        let config = Config::read();
        self.gate(agent, &config, need).await?;
        let decision = self.grants().decide(&agent.id, level, &config);
        match decision {
            Decision::Allowed => Ok(config),
            Decision::Denied => Err(AgentError::DeniedByUser("the user denied this agent access".into())),
            Decision::Ask => Err(not_granted()),
        }
    }

    /// Read the desktop's state from `desktop` from now on.
    pub fn set_desktop(&self, desktop: Desktop) {
        if let Some(center) = &desktop.notifications {
            // The "Reminder set" notification's button.
            let weak = Arc::downgrade(&self.inner);
            center.on_reminder_cancel(Arc::new(move |id: &str| {
                if let Some(inner) = weak.upgrade() {
                    AgentAccess { inner }.cancel_reminder_by_user(id);
                }
            }));
        }
        let _ = self.inner.desktop.set(desktop);
        self.inner.reminders_changed.notify_one();
    }

    /// Ask the user `dialog` for `caller` until `timeout`, one question of
    /// an agent's at a time (README.md, "Talk to the user methods"). Ends
    /// with the answer; [`talk::Reply::Cancelled`] when the screen locks;
    /// the error a new call would get when the agent loses access
    /// meanwhile; `TimedOut`; `Failed` when the caller goes away. Whatever
    /// ends it, the dialog goes.
    pub async fn question(&self, conn: &Connection, header: &Header<'_>, caller: &Caller, dialog: talk::Dialog, timeout: Option<Duration>) -> Result<talk::Reply, AgentError> {
        let unavailable = || AgentError::Failed("the shell cannot show questions".into());
        let ui = self.inner.desktop.get().ok_or_else(unavailable)?.ui.clone();
        let sender = header.sender().map(|s| s.to_owned());
        // The indicator shows the agent while its question is open or waits.
        let _busy = self.busy(&caller.agent);
        let turn = self.inner.turns.lock().unwrap_or_else(|e| e.into_inner()).entry(caller.agent.id.clone()).or_default().clone();
        let id = self.inner.next_question.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let asking = async {
            let _turn = turn.lock().await;
            let (reply, mut answer) = tokio::sync::oneshot::channel();
            let request = talk::QuestionRequest { id, name: caller.agent.name.clone(), agent: caller.agent.id.clone(), dialog, reply };
            ui.send(crate::UiEvent::AgentQuestion(talk::QuestionSlot::new(request))).map_err(|_| unavailable())?;
            let mut poll = tokio::time::interval(QUESTION_POLL);
            loop {
                tokio::select! {
                    got = &mut answer => return Ok(got.unwrap_or(talk::Reply::Cancelled)),
                    _ = poll.tick() => {
                        match self.recheck(&caller.agent, READ).await {
                            Ok(_) => {}
                            // Locking closes the question unanswered.
                            Err(AgentError::Locked(_)) => return Ok(talk::Reply::Cancelled),
                            Err(err) => return Err(err),
                        }
                        if let Some(sender) = &sender {
                            if !name_has_owner(conn, sender).await {
                                return Err(AgentError::Failed("the caller went away".into()));
                            }
                        }
                    }
                }
            }
        };
        let result = match timeout {
            Some(t) => tokio::time::timeout(t, asking).await.unwrap_or_else(|_| Err(AgentError::TimedOut("the user did not answer in time".into()))),
            None => asking.await,
        };
        let _ = ui.send(crate::UiEvent::AgentQuestionClose(id));
        result
    }

    fn reminders(&self) -> MutexGuard<'_, talk::Reminders> {
        self.inner.reminders.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Show reminders when they come due: now (those due while the shell
    /// was not running), then at their times, looking again at least
    /// every [`REMINDER_POLL`] (the clock may jump, the machine sleep).
    fn start_reminders(&self) {
        let weak = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            loop {
                let wait = {
                    let Some(inner) = weak.upgrade() else { return };
                    let access = AgentAccess { inner };
                    access.reminders_due(now());
                    let next = access.reminders().next();
                    let secs = next.map_or(REMINDER_POLL.as_secs() as i64, |t| t - now()).clamp(1, REMINDER_POLL.as_secs() as i64);
                    let changed = access.inner.reminders_changed.clone();
                    // (Not holding the shell's state while waiting.)
                    drop(access);
                    async move {
                        tokio::select! {
                            _ = changed.notified() => {}
                            _ = tokio::time::sleep(Duration::from_secs(secs as u64)) => {}
                        }
                    }
                };
                wait.await;
            }
        });
    }

    /// How a reminder's notification names its agent, as the indicator.
    fn reminder_label(r: &talk::Reminder) -> String {
        prompt::Active { agent: r.agent.clone(), executable: r.executable.clone(), name: r.name.clone(), level: Level::ReadOnly }.label()
    }

    /// A line about reminder `r` (the event `Reminder`).
    fn reminder_entry(r: &talk::Reminder, outcome: activity::Outcome) -> activity::Entry {
        activity::Entry { time: now(), agent: r.agent.clone(), name: r.name.clone(), method: activity::REMINDER.to_owned(), target: talk::log_target(&r.id), outcome, ..activity::Entry::default() }
    }

    /// Show the reminders due at `now` as notifications (dropped unshown
    /// with access off), once notifications can be shown.
    pub fn reminders_due(&self, now: i64) {
        let Some(center) = self.inner.desktop.get().and_then(|d| d.notifications.clone()) else { return };
        let due = self.reminders().take_due(now);
        if due.is_empty() {
            return;
        }
        let enabled = Config::read().enabled;
        for r in &due {
            if let Some(note) = self.inner.reminder_notes.lock().unwrap_or_else(|e| e.into_inner()).remove(&r.id) {
                center.withdraw(note);
            }
            let outcome = if enabled {
                center.reminder_due(&Self::reminder_label(r), &r.text);
                tracing::info!(agent = %r.agent, id = %r.id, "agent reminder shown");
                activity::Outcome::Allowed
            } else {
                tracing::info!(agent = %r.agent, id = %r.id, "agent reminder dropped: agent access is off");
                activity::Outcome::Refused("AccessOff".into())
            };
            self.inner.log.record(Self::reminder_entry(r, outcome));
        }
        self.reminders_changed();
    }

    /// A reminder was added: say so in a notification the user can cancel
    /// it from, and wake the timer.
    fn reminder_added(&self, r: &talk::Reminder) {
        if let Some(center) = self.inner.desktop.get().and_then(|d| d.notifications.as_ref()) {
            let note = center.reminder_set(&Self::reminder_label(r), &r.id, r.time, &r.text);
            self.inner.reminder_notes.lock().unwrap_or_else(|e| e.into_inner()).insert(r.id.clone(), note);
        }
        self.reminders_changed();
    }

    /// Cancel reminder `id` of `agent` (anyone's for `None`); whether there
    /// was one.
    pub fn cancel_reminder(&self, agent: Option<&str>, id: &str) -> Option<talk::Reminder> {
        let gone = self.reminders().cancel(agent, id)?;
        let note = self.inner.reminder_notes.lock().unwrap_or_else(|e| e.into_inner()).remove(id);
        if let (Some(note), Some(center)) = (note, self.inner.desktop.get().and_then(|d| d.notifications.as_ref())) {
            center.withdraw(note);
        }
        self.reminders_changed();
        Some(gone)
    }

    /// The user cancelled reminder `id` (the notification's button).
    pub fn cancel_reminder_by_user(&self, id: &str) {
        if let Some(r) = self.cancel_reminder(None, id) {
            tracing::info!(agent = %r.agent, id, "agent reminder cancelled by the user");
            self.inner.log.record(Self::reminder_entry(&r, activity::Outcome::Cancelled));
        }
    }

    /// Wake the reminders' timer and tell Settings.
    fn reminders_changed(&self) {
        self.inner.reminders_changed.notify_one();
        let Some(conn) = self.inner.conn.get().cloned() else { return };
        tokio::spawn(async move {
            if let Ok(emitter) = SignalEmitter::new(&conn, OBJECT_PATH) {
                let _ = AgentAccessSettings::reminders_changed(&emitter).await;
            }
        });
    }

    /// The windows, workspaces and monitors as the UI knows them now.
    pub async fn snapshot(&self) -> Result<desktop::Snapshot, AgentError> {
        let unavailable = || AgentError::Failed("the desktop's state is not available".into());
        let ui = self.inner.desktop.get().ok_or_else(unavailable)?.ui.clone();
        let (reply, answer) = tokio::sync::oneshot::channel();
        ui.send(crate::UiEvent::AgentSnapshot(desktop::SnapshotSlot::new(reply))).map_err(|_| unavailable())?;
        tokio::time::timeout(Duration::from_secs(5), answer).await.map_err(|_| unavailable())?.map_err(|_| unavailable())
    }

    /// Window `window`'s controls for `caller` (README.md, "Control apps
    /// methods"): the window's app checked against the exclusions
    /// (recorded in the log), nothing while a system prompt is shown, the
    /// window's process checked, then its tree found on the accessibility
    /// bus.
    pub async fn a11y_scope(&self, conn: &Connection, caller: &Caller, window: u32) -> Result<(desktop::WindowInfo, a11y::Scope), AgentError> {
        let snapshot = self.snapshot().await?;
        let Some(w) = snapshot.windows.iter().find(|w| w.id == window).cloned() else { return Err(AgentError::NotFound(format!("window {window}"))) };
        caller.target(format!("window:{window} ({})", w.app_id));
        caller.check_target(Target::App(&w.app_id))?;
        if snapshot.system_prompt {
            caller.check_target(Target::SystemPrompt)?;
        }
        let (config, proc_root, shown, pid) = (caller.config.clone(), self.inner.proc_root.clone(), snapshot.clone(), w.pid);
        blocking(move || a11y::check_process(pid, &shown, &config, &desktop::installed(), &proc_root)).await?;
        let bus = self.inner.a11y.get(conn).await?;
        let mut scope = a11y::resolve(&bus, &w, &snapshot).await?;
        scope.read_text_fields = caller.config.read_text_fields;
        Ok((w, scope))
    }

    /// Just before using an app's control: still unlocked, no system prompt
    /// came meanwhile, and the window is still there.
    async fn a11y_still_clear(&self, caller: &Caller, window: u32) -> Result<(), AgentError> {
        if self.locked() {
            return Err(AgentError::Locked("the screen is locked".into()));
        }
        let snapshot = self.snapshot().await?;
        if snapshot.system_prompt {
            caller.check_target(Target::SystemPrompt)?;
        }
        if !snapshot.windows.iter().any(|w| w.id == window) {
            return Err(AgentError::NotFound(format!("window {window}")));
        }
        Ok(())
    }

    /// Have the UI make `ops` of the compositor's window list.
    async fn window_ops(&self, ops: Vec<actions::WindowOp>) -> Result<(), AgentError> {
        if ops.is_empty() {
            return Ok(());
        }
        let unavailable = || AgentError::Failed("the window list is not available".into());
        let ui = self.inner.desktop.get().ok_or_else(unavailable)?.ui.clone();
        let (reply, answer) = tokio::sync::oneshot::channel();
        ui.send(crate::UiEvent::AgentWindow(actions::WindowSlot::new(actions::WindowRequest { ops, reply }))).map_err(|_| unavailable())?;
        let done = tokio::time::timeout(Duration::from_secs(5), answer).await.map_err(|_| unavailable())?.map_err(|_| unavailable())?;
        done.map_err(AgentError::NotSupported)
    }

    /// Wait (at most [`SETTLE`]) until the windows show `done`.
    async fn settle(&self, done: impl Fn(&desktop::Snapshot) -> bool) {
        let until = Instant::now() + SETTLE;
        while Instant::now() < until {
            if self.snapshot().await.is_ok_and(|s| done(&s)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Tell quick settings the volume and brightness again.
    async fn refresh_quick(&self, conn: &Connection) {
        crate::status::changed();
        let Some(ui) = self.inner.desktop.get().map(|d| d.ui.clone()) else { return };
        let (_, max) = crate::shell_keys::volume_limits();
        let volume = crate::shell_keys::volume_state().await.map(|(p, m)| (p, m, max));
        let brightness = crate::shell_keys::brightness_percent(conn).await;
        let _ = ui.send(crate::UiEvent::QuickSettings { volume, brightness });
    }

    /// The notification list, newest first.
    fn notifications(&self) -> Vec<(crate::notify_store::Notification, i64)> {
        self.inner.desktop.get().and_then(|d| d.notifications.as_ref()).map(|c| c.listed()).unwrap_or_default()
    }

    /// Whether `agent` may still make a call needing `need` (steps 3–8
    /// without asking): for calls that keep running (README.md,
    /// "Settings take effect at once").
    pub async fn recheck(&self, agent: &Agent, need: Need) -> Result<Config, AgentError> {
        let config = Config::read();
        self.gate(agent, &config, need).await?;
        let decision = self.grants().decide(&agent.id, need.level.unwrap_or(Level::ReadOnly), &config);
        match decision {
            Decision::Allowed => Ok(config),
            Decision::Denied => Err(AgentError::DeniedByUser("the user denied this agent access".into())),
            Decision::Ask => Err(AgentError::NotGranted("this agent's access ended".into())),
        }
    }

    /// The status dictionary of `GetStatus` and `RequestAccess`.
    pub async fn status(&self, agent: &Agent, config: &Config) -> HashMap<String, OwnedValue> {
        let remote = self.remote(agent).await;
        let (granted, grant) = {
            let grants = self.grants();
            (grants.effective(&agent.id, config).map_or("", Level::id), grants.kind(&agent.id, config))
        };
        HashMap::from([
            ("version".to_owned(), value(VERSION)),
            ("enabled".to_owned(), value(config.enabled)),
            ("access-level".to_owned(), value(config.level.id())),
            ("granted-level".to_owned(), value(granted)),
            ("grant".to_owned(), value(grant)),
            ("agent".to_owned(), value(agent.id.as_str())),
            ("executable".to_owned(), value(agent.executable.as_str())),
            ("name".to_owned(), value(agent.name.as_str())),
            ("identified".to_owned(), value(agent.identified)),
            ("remote".to_owned(), value(remote)),
            ("allow-remote".to_owned(), value(config.allow_remote)),
            ("locked".to_owned(), value(self.locked())),
            ("notifications".to_owned(), value(config.notifications)),
            ("clipboard".to_owned(), value(config.clipboard)),
            ("read-text-fields".to_owned(), value(config.read_text_fields)),
            ("confirm-sensitive".to_owned(), value(config.confirm_sensitive)),
        ])
    }

    /// Show the indicator through `ui` from now on (README.md,
    /// "Indicator"): the active agents are sent as
    /// [`UiEvent::AgentsActive`](crate::UiEvent::AgentsActive) whenever
    /// they change, also when an agent goes quiet for a minute.
    pub fn set_indicator(&self, ui: tokio::sync::mpsc::UnboundedSender<crate::UiEvent>) {
        if self.inner.indicator.set(ui).is_err() {
            return;
        }
        let weak = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            loop {
                tick.tick().await;
                let Some(inner) = weak.upgrade() else { return };
                AgentAccess { inner }.publish();
            }
        });
    }

    /// The agents the indicator lists at `now`: holding a grant, and with
    /// an allowed call in the last minute or a call running.
    pub fn active(&self, now: i64) -> Vec<prompt::Active> {
        let config = Config::read();
        if !config.enabled {
            return Vec::new();
        }
        let running = self.inner.running.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let grants = self.grants();
        let mut out: Vec<prompt::Active> = grants
            .list()
            .filter(|(id, e)| running.get(*id).is_some_and(|n| *n > 0) || (e.last_active > 0 && now - e.last_active < prompt::ACTIVE_FOR))
            .filter_map(|(id, e)| {
                let level = grants.effective(id, &config)?;
                Some(prompt::Active { agent: id.clone(), executable: e.executable.clone(), name: e.name.clone(), level })
            })
            .collect();
        out.sort_by(|a, b| a.agent.cmp(&b.agent));
        out
    }

    /// Send the indicator's agents to the UI if they changed.
    fn publish(&self) {
        let Some(ui) = self.inner.indicator.get() else { return };
        let active = self.active(now());
        let mut shown = self.inner.shown.lock().unwrap_or_else(|e| e.into_inner());
        if *shown != active {
            *shown = active.clone();
            let _ = ui.send(crate::UiEvent::AgentsActive(active));
        }
    }

    /// A call of `agent`'s that keeps running (a wait, a question, input):
    /// the indicator shows it until the guard is dropped.
    pub fn busy(&self, agent: &Agent) -> Busy {
        *self.inner.running.lock().unwrap_or_else(|e| e.into_inner()).entry(agent.id.clone()).or_default() += 1;
        self.publish();
        Busy { access: self.clone(), agent: agent.id.clone() }
    }

    /// End `agent`'s session grant and denial (Settings' *Revoke*, the
    /// indicator's *Stop*).
    pub fn revoke(&self, agent: &str) {
        if self.grants().revoke(agent) {
            tracing::info!(agent, "agent access revoked");
            self.changed();
        }
    }

    /// *Stop* in the indicator: end `agent`'s session grant, and ask again
    /// even if it is always allowed (README.md, "Indicator").
    pub fn stop(&self, agent: &str) {
        let held = {
            let grants = self.grants();
            let config = Config::read();
            let name = grants.list().find(|(id, _)| id.as_str() == agent).map(|(_, e)| e.name.clone());
            name.map(|name| (name, grants.effective(agent, &config)))
        };
        if self.grants().stop(agent) {
            let (name, level) = held.unwrap_or_default();
            self.inner.log.record(activity::Entry { time: now(), agent: agent.to_owned(), name, method: activity::STOP.to_owned(), level, outcome: activity::Outcome::Cancelled, ..activity::Entry::default() });
            tracing::info!(agent, "agent stopped from the indicator");
            self.changed();
        }
    }

    /// Revoke and forget a remembered grant.
    pub fn forget(&self, agent: &str) {
        if self.grants().forget(agent) {
            tracing::info!(agent, "agent forgotten");
            self.changed();
        }
    }

    /// Revoke every agent.
    pub fn stop_all(&self) {
        if self.grants().revoke_all() {
            tracing::info!("every agent's access revoked");
            self.changed();
        }
    }

    /// Every agent with a grant or denial (`ListAgents`).
    pub fn list(&self) -> Vec<HashMap<String, OwnedValue>> {
        let grants = self.grants();
        grants
            .list()
            .map(|(id, e)| {
                let mut kinds = Vec::new();
                if e.session.is_some() {
                    kinds.push("session");
                }
                if e.always.is_some() {
                    kinds.push("always");
                }
                if e.denied {
                    kinds.push("denied");
                }
                let level = e.session.map(|(l, _)| l).max(e.always.as_ref().map(|a| a.level));
                let granted = e.session.map(|(_, t)| t).or(e.always.as_ref().map(|a| a.granted)).unwrap_or(0);
                HashMap::from([
                    ("agent".to_owned(), value(id.as_str())),
                    ("executable".to_owned(), value(e.executable.as_str())),
                    ("name".to_owned(), value(e.name.as_str())),
                    ("level".to_owned(), value(level.map_or("", Level::id))),
                    ("kinds".to_owned(), value(kinds)),
                    ("granted".to_owned(), value(granted)),
                    ("last-active".to_owned(), value(e.last_active)),
                    ("identified".to_owned(), value(e.identified)),
                ])
            })
            .collect()
    }

    /// Record a call of Settings' that takes access away (`Revoke`,
    /// `Forget`, `StopAll`), by the Settings executable `exe`, about agent
    /// `target` ("" for every agent).
    fn record_settings(&self, exe: &Path, method: &str, target: &str) {
        let entry = activity::Entry { time: now(), agent: exe.to_string_lossy().into_owned(), method: method.to_owned(), target: target.to_owned(), ..activity::Entry::default() };
        self.inner.log.record(entry);
    }

    /// Refuse callers other than the Settings app (README.md, "Who may
    /// manage grants"), recording the refusal; Settings' executable.
    async fn require_settings(&self, conn: &Connection, header: &Header<'_>, method: &str) -> fdo::Result<PathBuf> {
        let result = self.settings_exe(conn, header).await;
        if let Err(err) = &result {
            let exe = caller_exe(conn, header, &self.inner.proc_root).await;
            tracing::warn!(%err, exe, "refused agent access management from a program other than Settings");
            let refused = activity::Outcome::Refused("AccessDenied".into());
            self.inner.log.record(activity::Entry { time: now(), agent: exe, method: method.to_owned(), outcome: refused, ..activity::Entry::default() });
        }
        result
    }

    async fn settings_exe(&self, conn: &Connection, header: &Header<'_>) -> fdo::Result<PathBuf> {
        let denied = || fdo::Error::AccessDenied("only Settings may manage agent access".into());
        caller::require_unsandboxed(conn, header).await?;
        let pid = caller_pid(conn, header).await.ok_or_else(denied)?;
        let exe = std::fs::read_link(self.inner.proc_root.join(pid.to_string()).join("exe")).map_err(|_| denied())?;
        let exe = match exe.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
            Some(trimmed) => PathBuf::from(trimmed),
            None => exe,
        };
        if self.inner.settings_exes.contains(&exe) { Ok(exe) } else { Err(denied()) }
    }
}

/// A running call ([`AgentAccess::busy`]).
pub struct Busy {
    access: AgentAccess,
    agent: String,
}

impl Drop for Busy {
    fn drop(&mut self) {
        {
            let mut running = self.access.inner.running.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(n) = running.get_mut(&self.agent) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    running.remove(&self.agent);
                }
            }
        }
        self.access.publish();
    }
}

/// The common options and the policy check of a *Read only* method.
const READ: Need = Need::level(Level::ReadOnly);
/// A *Control the desktop* method's.
const DESKTOP: Need = Need::level(Level::Desktop);
/// A *Control apps* method's.
const APPS: Need = Need::level(Level::Apps);

/// Run `f` where it may block (GIO looking up and starting apps).
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, AgentError> + Send + 'static) -> Result<T, AgentError> {
    tokio::task::spawn_blocking(f).await.map_err(|e| AgentError::Failed(e.to_string()))?
}

type Records = Vec<HashMap<String, OwnedValue>>;

/// The quick settings' state for `GetState` (README.md, "Records",
/// State): the settings, and what the sound server, NetworkManager and
/// the backlight say when they answer.
async fn quick_state(conn: &Connection, system: Option<&Connection>) -> HashMap<String, OwnedValue> {
    let mut out = {
        let interface = helix_settings::Schema::open("org.gnome.desktop.interface");
        let notifications = helix_settings::Schema::open("org.gnome.desktop.notifications");
        let color = helix_settings::Schema::open("org.gnome.settings-daemon.plugins.color");
        HashMap::from([
            ("dark-style".to_owned(), value(interface.get::<String>("color-scheme").as_deref() == Some("prefer-dark"))),
            ("do-not-disturb".to_owned(), value(!notifications.get_or("show-banners", true))),
            ("night-light".to_owned(), value(color.get_or("night-light-enabled", false))),
        ])
    };
    let status = match system {
        Some(system) => tokio::time::timeout(Duration::from_secs(3), crate::status::read(Some(system))).await.ok(),
        None => None,
    };
    if let Some(status) = status {
        if let Some((wifi, _)) = status.wifi {
            out.insert("wifi".to_owned(), value(wifi));
        }
        if status.wifi.is_some() || status.bluetooth.is_some() {
            out.insert("airplane-mode".to_owned(), value(status.airplane));
        }
    }
    if let Ok(Some((percent, muted))) = tokio::time::timeout(Duration::from_secs(3), crate::shell_keys::volume_state()).await {
        out.insert("volume".to_owned(), value(f64::from(percent) / 100.0));
        out.insert("muted".to_owned(), value(muted));
    }
    if let Ok(Some(percent)) = tokio::time::timeout(Duration::from_secs(3), crate::shell_keys::brightness_percent(conn)).await {
        out.insert("brightness".to_owned(), value(f64::from(percent) / 100.0));
    }
    out
}

/// A timeout in milliseconds (0: none) as a deadline.
fn deadline(timeout_ms: u32) -> Option<Instant> {
    (timeout_ms > 0).then(|| Instant::now() + Duration::from_millis(u64::from(timeout_ms)))
}

impl Agent1 {
    /// Turn quick setting `toggle` on or off, as quick settings does.
    async fn toggle(&self, conn: &Connection, toggle: actions::Toggle, on: bool) -> Result<(), AgentError> {
        use actions::Toggle;
        let unavailable = || AgentError::NotSupported(format!("{} is not available", toggle.name()));
        match toggle {
            Toggle::DarkStyle | Toggle::NightLight => {
                let done = if toggle == Toggle::DarkStyle { crate::status::set_dark_style(on) } else { crate::status::set_night_light(on) };
                if !done {
                    return Err(unavailable());
                }
                gio::Settings::sync();
            }
            Toggle::DoNotDisturb => {
                if !helix_settings::Schema::open("org.gnome.desktop.notifications").has("show-banners") {
                    return Err(unavailable());
                }
                crate::notifications::set_do_not_disturb(on);
            }
            Toggle::Wifi | Toggle::AirplaneMode => {
                let system = self.access.system_bus().await.ok_or_else(unavailable)?;
                let (wifi, bluetooth) = tokio::time::timeout(Duration::from_secs(3), crate::status::radios(&system)).await.map_err(|_| unavailable())?;
                let there = if toggle == Toggle::Wifi { wifi.is_some() } else { wifi.is_some() || bluetooth.is_some() };
                if !there {
                    return Err(unavailable());
                }
                let change = if toggle == Toggle::Wifi { crate::status::Toggle::Wifi(on) } else { crate::status::Toggle::Airplane(on) };
                crate::status::apply(&system, change).await.map_err(|e| AgentError::Failed(e.to_string()))?;
                crate::status::changed();
            }
            Toggle::Mute => {
                crate::shell_keys::volume_state().await.ok_or_else(unavailable)?;
                crate::shell_keys::set_mute(on).await;
                self.access.refresh_quick(conn).await;
            }
        }
        Ok(())
    }

    /// The documents the visible windows show, where their apps tell.
    async fn documents(conn: &Connection, windows: &[&desktop::WindowInfo]) -> HashMap<u32, String> {
        let players = document::players(conn).await;
        let windows: Vec<desktop::WindowInfo> = windows.iter().map(|w| (*w).clone()).collect();
        tokio::task::spawn_blocking(move || {
            let installed = desktop::installed();
            let recent = files::read_xbel(&files::xbel_path());
            let refs: Vec<&desktop::WindowInfo> = windows.iter().collect();
            document::documents(&refs, &installed, &players, &recent)
        })
        .await
        .unwrap_or_default()
    }

    /// Window records of `windows`, with their documents.
    async fn window_records(conn: &Connection, snapshot: &desktop::Snapshot, windows: &[&desktop::WindowInfo]) -> Records {
        let docs = Self::documents(conn, windows).await;
        windows.iter().map(|w| desktop::window_record(snapshot, w, docs.get(&w.id).map(String::as_str))).collect()
    }
}

/// `org.helixdesktop.Agent1`.
pub struct Agent1 {
    pub access: AgentAccess,
}

#[interface(name = "org.helixdesktop.Agent1")]
impl Agent1 {
    async fn get_status(&self, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> Result<HashMap<String, OwnedValue>, AgentError> {
        let caller = self.access.check(conn, &header, "GetStatus", Need::STATUS, CallOptions::default()).await?;
        Ok(self.access.status(&caller.agent, &caller.config).await)
    }

    async fn request_access(
        &self,
        level: &str,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> Result<HashMap<String, OwnedValue>, AgentError> {
        let options = CallOptions::from_dict(&options);
        let level = match level {
            "" => Some(Config::read().level),
            id => Level::from_id(id),
        };
        let Some(level) = level else {
            // Learn nothing about arguments without access.
            let caller = self.access.check(conn, &header, "RequestAccess", Need::STATUS, options).await?;
            if !caller.config.enabled {
                return caller.finish(Err(AgentError::AccessOff("agent access is turned off".into())));
            }
            return caller.finish(Err(AgentError::invalid_args("unknown access level")));
        };
        let need = Need { exact: true, ..Need::level(level) };
        let caller = self.access.check(conn, &header, "RequestAccess", need, options).await?;
        let status = self.access.status(&caller.agent, &caller.config).await;
        caller.finish(Ok(status))
    }

    async fn get_state(&self, options: HashMap<String, OwnedValue>, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> Result<HashMap<String, OwnedValue>, AgentError> {
        let caller = self.access.check(conn, &header, "GetState", READ, CallOptions::from_dict(&options)).await?;
        let result = async {
            let snapshot = self.access.snapshot().await?;
            let shown = desktop::visible(&snapshot, &caller.config);
            let focused = if shown.iter().any(|w| w.id == snapshot.focused) { snapshot.focused } else { 0 };
            let system = self.access.system_bus().await;
            let mut state = quick_state(conn, system.as_ref()).await;
            state.insert("focused-window".to_owned(), value(focused));
            state.insert("active-workspace".to_owned(), value(snapshot.active));
            Ok(state)
        }
        .await;
        caller.finish(result)
    }

    async fn list_windows(&self, options: HashMap<String, OwnedValue>, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> Result<Records, AgentError> {
        let caller = self.access.check(conn, &header, "ListWindows", READ, CallOptions::from_dict(&options)).await?;
        let result = async {
            let wanted = desktop::Match::from_dict(&options)?;
            wanted.record(&caller)?;
            let snapshot = self.access.snapshot().await?;
            let shown: Vec<&desktop::WindowInfo> = desktop::visible(&snapshot, &caller.config).into_iter().filter(|w| wanted.matches(&snapshot, w)).collect();
            Ok(Self::window_records(conn, &snapshot, &shown).await)
        }
        .await;
        caller.finish(result)
    }

    async fn list_apps(&self, options: HashMap<String, OwnedValue>, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> Result<Records, AgentError> {
        let caller = self.access.check(conn, &header, "ListApps", READ, CallOptions::from_dict(&options)).await?;
        let result = async {
            let running = match options.get("running") {
                None => false,
                Some(v) => bool::try_from(v.clone()).map_err(|_| AgentError::invalid_args("“running” must be a boolean"))?,
            };
            let snapshot = self.access.snapshot().await?;
            let windows: Vec<desktop::WindowInfo> = desktop::visible(&snapshot, &caller.config).into_iter().cloned().collect();
            let config = caller.config.clone();
            tokio::task::spawn_blocking(move || {
                let refs: Vec<&desktop::WindowInfo> = windows.iter().collect();
                desktop::app_records(&desktop::installed(), &refs, &config, running)
            })
            .await
            .map_err(|e| AgentError::Failed(e.to_string()))
        }
        .await;
        caller.finish(result)
    }

    async fn list_workspaces(&self, options: HashMap<String, OwnedValue>, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> Result<Records, AgentError> {
        let caller = self.access.check(conn, &header, "ListWorkspaces", READ, CallOptions::from_dict(&options)).await?;
        let result = self.access.snapshot().await.map(|snapshot| desktop::workspace_records(&snapshot, &desktop::visible(&snapshot, &caller.config)));
        caller.finish(result)
    }

    async fn list_monitors(&self, options: HashMap<String, OwnedValue>, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> Result<Records, AgentError> {
        let caller = self.access.check(conn, &header, "ListMonitors", READ, CallOptions::from_dict(&options)).await?;
        let result = self.access.snapshot().await.map(|snapshot| desktop::monitor_records(&snapshot));
        caller.finish(result)
    }

    async fn list_notifications(&self, options: HashMap<String, OwnedValue>, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> Result<Records, AgentError> {
        let caller = self.access.check(conn, &header, "ListNotifications", READ, CallOptions::from_dict(&options)).await?;
        let limit = match options.get("limit") {
            None => crate::notify_store::LIST_LIMIT,
            Some(v) => match u32::try_from(v.clone()) {
                Ok(n) => n.max(1) as usize,
                Err(_) => return caller.finish(Err(AgentError::invalid_args("“limit” must be a number (u)"))),
            },
        };
        let snapshot = self.access.snapshot().await.unwrap_or_default();
        let notes: Vec<desktop::NoteInfo> = self
            .access
            .notifications()
            .into_iter()
            .map(|(n, time)| desktop::NoteInfo {
                id: n.id,
                app_id: n.app_id.clone(),
                app_name: n.app_name.clone(),
                time,
                summary: n.summary.clone(),
                body: if n.markup { crate::markup::plain(&n.body) } else { n.body.clone() },
                window: match n.origin {
                    crate::notify_store::Origin::Window { id } => Some(id),
                    _ => None,
                },
            })
            .collect();
        let config = caller.config.clone();
        let contents = config.notifications;
        let result = tokio::task::spawn_blocking(move || {
            let installed = desktop::installed();
            notes.iter().filter(|n| !desktop::note_hidden(n, &snapshot, &installed, &config)).take(limit).map(|n| desktop::note_record(n, contents)).collect()
        })
        .await
        .map_err(|e| AgentError::Failed(e.to_string()));
        caller.finish(result)
    }

    async fn recent_files(&self, options: HashMap<String, OwnedValue>, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> Result<Records, AgentError> {
        let caller = self.access.check(conn, &header, "RecentFiles", READ, CallOptions::from_dict(&options)).await?;
        let result = async {
            let limit = files::limit(&options)?;
            let mime = match options.get("type") {
                None => None,
                Some(v) => Some(String::try_from(v.clone()).map_err(|_| AgentError::invalid_args("“type” must be a string"))?).filter(|m| !m.is_empty()),
            };
            let config = caller.config.clone();
            tokio::task::spawn_blocking(move || {
                // Nothing when the user turned recent files off.
                let Some(cutoff) = helix_settings::files::privacy_cutoff() else { return Vec::new() };
                let found = files::recent(files::read_xbel(&files::xbel_path()), &desktop::installed(), &config, cutoff, mime.as_deref(), limit);
                found.iter().map(files::FileInfo::record).collect()
            })
            .await
            .map_err(|e| AgentError::Failed(e.to_string()))
        }
        .await;
        caller.finish(result)
    }

    async fn search_files(&self, query: &str, options: HashMap<String, OwnedValue>, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> Result<Records, AgentError> {
        let caller = self.access.check(conn, &header, "SearchFiles", READ, CallOptions::from_dict(&options)).await?;
        // The query can be private ("letter to my lawyer"): only its
        // length is recorded.
        caller.detail("query-length", activity::Detail::Count(query.chars().count() as u64));
        let search = match files::search_options(query, &options) {
            Ok(search) => search,
            Err(err) => return caller.finish(Err(err)),
        };
        caller.target(gio::prelude::FileExt::uri(&gio::File::for_path(&search.folder)).to_string());
        let _busy = self.access.busy(&caller.agent);
        let found = files::search(conn, &search).await.iter().map(files::FileInfo::record).collect();
        caller.finish(Ok(found))
    }

    async fn wait_for_window(
        &self,
        wanted_match: HashMap<String, OwnedValue>,
        timeout_ms: u32,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> Result<HashMap<String, OwnedValue>, AgentError> {
        let caller = self.access.check(conn, &header, "WaitForWindow", READ, CallOptions::from_dict(&wanted_match)).await?;
        let result = async {
            let wanted = desktop::Match::from_dict(&wanted_match)?;
            wanted.record(&caller)?;
            let _busy = self.access.busy(&caller.agent);
            let until = deadline(timeout_ms);
            let mut config = caller.config.clone();
            loop {
                let snapshot = self.access.snapshot().await?;
                if let Some(w) = desktop::visible(&snapshot, &config).into_iter().find(|w| wanted.matches(&snapshot, w)) {
                    let mut records = Self::window_records(conn, &snapshot, &[w]).await;
                    return Ok(records.remove(0));
                }
                if until.is_some_and(|u| Instant::now() >= u) {
                    return Err(AgentError::TimedOut("no such window appeared".into()));
                }
                tokio::time::sleep(WAIT_POLL).await;
                config = self.access.recheck(&caller.agent, READ).await?;
            }
        }
        .await;
        caller.finish(result)
    }

    async fn wait_for_window_closed(&self, window: u32, timeout_ms: u32, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> Result<(), AgentError> {
        let caller = self.access.check(conn, &header, "WaitForWindowClosed", READ, CallOptions::default()).await?;
        caller.target(format!("window:{window}"));
        let result = async {
            let snapshot = self.access.snapshot().await?;
            let Some(w) = snapshot.windows.iter().find(|w| w.id == window) else { return Err(AgentError::NotFound(format!("window {window}"))) };
            caller.target(format!("window:{window} ({})", w.app_id));
            caller.check_target(Target::App(&w.app_id))?;
            let _busy = self.access.busy(&caller.agent);
            let until = deadline(timeout_ms);
            loop {
                let snapshot = self.access.snapshot().await?;
                if !snapshot.windows.iter().any(|w| w.id == window) {
                    return Ok(());
                }
                if until.is_some_and(|u| Instant::now() >= u) {
                    return Err(AgentError::TimedOut("the window is still open".into()));
                }
                tokio::time::sleep(WAIT_POLL).await;
                self.access.recheck(&caller.agent, READ).await?;
            }
        }
        .await;
        caller.finish(result)
    }

    async fn open(&self, uri: &str, options: HashMap<String, OwnedValue>, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> Result<(), AgentError> {
        let call = CallOptions::from_dict(&options);
        let caller = self.access.check_targeted(conn, &header, "Open", DESKTOP, call).await?;
        let result = async {
            let app = actions::text_option(&options, "app")?;
            let uri = actions::Uri::parse(uri)?;
            caller.target(uri.log_target());
            if let Some(app) = &app {
                caller.check_target(Target::App(app))?;
            }
            let plan = blocking(move || actions::plan_open(&uri, app.as_deref())).await?;
            for app in &plan.apps {
                caller.check_target(Target::App(app))?;
            }
            self.access.confirm(&caller, "Open", DESKTOP, call).await?;
            blocking(move || actions::launch(&plan.launch)).await
        }
        .await;
        caller.finish(result)
    }

    async fn launch(
        &self,
        app_id: &str,
        uris: Vec<String>,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> Result<(), AgentError> {
        let call = CallOptions::from_dict(&options);
        let caller = self.access.check_targeted(conn, &header, "Launch", DESKTOP, call).await?;
        if !uris.is_empty() {
            caller.detail("uri-count", activity::Detail::Count(uris.len() as u64));
        }
        let result = async {
            if app_id.is_empty() {
                return Err(AgentError::invalid_args("no app id"));
            }
            caller.check_target(Target::App(app_id))?;
            let uris = uris.iter().map(|u| actions::Uri::parse(u).map(|u| u.uri())).collect::<Result<Vec<_>, _>>()?;
            let id = app_id.to_owned();
            let plan = blocking(move || actions::plan_launch(&id, uris)).await?;
            for app in &plan.apps {
                caller.check_target(Target::App(app))?;
            }
            self.access.confirm(&caller, "Launch", DESKTOP, call).await?;
            blocking(move || actions::launch(&plan.launch)).await
        }
        .await;
        caller.finish(result)
    }

    async fn window_action(
        &self,
        window: u32,
        action: &str,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> Result<(), AgentError> {
        // Closing may lose what the window holds.
        let sensitive = actions::window_action_sensitive(action);
        let need = Need { sensitive, ..DESKTOP };
        let call = CallOptions { action: sensitive.then_some("close"), ..CallOptions::from_dict(&options) };
        let caller = self.access.check_targeted(conn, &header, "WindowAction", need, call).await?;
        caller.target(format!("window:{window}"));
        let result = async {
            let command = actions::WindowCommand::parse(action, &options)?;
            caller.detail("action", activity::Detail::Kind(command.name()));
            let snapshot = self.access.snapshot().await?;
            let Some(w) = snapshot.windows.iter().find(|w| w.id == window) else { return Err(AgentError::NotFound(format!("window {window}"))) };
            caller.target(format!("window:{window} ({})", w.app_id));
            caller.check_target(Target::App(&w.app_id))?;
            let ops = actions::plan_window(command, w, &snapshot)?;
            self.access.confirm(&caller, "WindowAction", need, call).await?;
            self.access.window_ops(ops).await?;
            self.access.settle(|s| actions::window_done(command, window, s)).await;
            Ok(())
        }
        .await;
        caller.finish(result)
    }

    async fn switch_workspace(&self, index: u32, options: HashMap<String, OwnedValue>, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> Result<(), AgentError> {
        let call = CallOptions::from_dict(&options);
        let caller = self.access.check_targeted(conn, &header, "SwitchWorkspace", DESKTOP, call).await?;
        caller.target(format!("workspace:{index}"));
        let result = async {
            self.access.confirm(&caller, "SwitchWorkspace", DESKTOP, call).await?;
            let snapshot = self.access.snapshot().await?;
            if !actions::workspace_exists(&snapshot, index) {
                return Err(AgentError::NotFound(format!("workspace {index}")));
            }
            if snapshot.active != index {
                self.access.window_ops(vec![actions::WindowOp::SwitchWorkspace(index)]).await?;
                self.access.settle(|s| s.active == index).await;
            }
            Ok(())
        }
        .await;
        caller.finish(result)
    }

    async fn set_toggle(&self, name: &str, on: bool, options: HashMap<String, OwnedValue>, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> Result<(), AgentError> {
        // Wi-Fi and airplane mode are sensitive: they change the network.
        let toggle = actions::Toggle::from_name(name);
        let need = Need { sensitive: toggle.is_some_and(actions::Toggle::sensitive), ..DESKTOP };
        let call = CallOptions { action: toggle.and_then(|t| t.action(on)), ..CallOptions::from_dict(&options) };
        let caller = self.access.check_targeted(conn, &header, "SetToggle", need, call).await?;
        let result = async {
            let toggle = toggle.ok_or_else(|| AgentError::NotSupported(format!("toggle {name}")))?;
            caller.target(toggle.name());
            caller.detail("state", activity::Detail::Kind(if on { "on" } else { "off" }));
            self.access.confirm(&caller, "SetToggle", need, call).await?;
            self.toggle(conn, toggle, on).await
        }
        .await;
        caller.finish(result)
    }

    async fn set_value(&self, name: &str, value: f64, options: HashMap<String, OwnedValue>, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> Result<(), AgentError> {
        let call = CallOptions::from_dict(&options);
        let caller = self.access.check_targeted(conn, &header, "SetValue", DESKTOP, call).await?;
        let result = async {
            let slider = actions::Slider::from_name(name).ok_or_else(|| AgentError::NotSupported(format!("value {name}")))?;
            caller.target(slider.name());
            self.access.confirm(&caller, "SetValue", DESKTOP, call).await?;
            let unavailable = || AgentError::NotSupported(format!("{name} is not available"));
            match slider {
                actions::Slider::Volume => {
                    let (_, max) = crate::shell_keys::volume_limits();
                    let percent = actions::percent(value, f64::from(max) / 100.0)?;
                    crate::shell_keys::volume_state().await.ok_or_else(unavailable)?;
                    crate::shell_keys::set_volume(percent).await;
                }
                actions::Slider::Brightness => {
                    let percent = actions::percent(value, 1.0)?;
                    crate::shell_keys::brightness_percent(conn).await.ok_or_else(unavailable)?;
                    crate::shell_keys::set_brightness_percent(conn, percent).await;
                }
            }
            self.access.refresh_quick(conn).await;
            Ok(())
        }
        .await;
        caller.finish(result)
    }

    async fn notify(&self, title: &str, body: &str, options: HashMap<String, OwnedValue>, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> Result<u32, AgentError> {
        let caller = self.access.check(conn, &header, "Notify", DESKTOP, CallOptions::from_dict(&options)).await?;
        // The text itself is never recorded.
        caller.detail("title-length", activity::Detail::Count(title.chars().count() as u64));
        caller.detail("body-length", activity::Detail::Count(body.chars().count() as u64));
        let result = async {
            if title.trim().is_empty() {
                return Err(AgentError::invalid_args("the title is empty"));
            }
            let icon = actions::text_option(&options, "icon")?.map(|i| actions::icon_name(&i).map(str::to_owned)).transpose()?;
            let low = match actions::text_option(&options, "urgency")?.as_deref() {
                None | Some("normal") => false,
                Some("low") => true,
                Some(_) => return Err(AgentError::invalid_args("“urgency” must be low or normal")),
            };
            let center = self.access.inner.desktop.get().and_then(|d| d.notifications.clone()).ok_or_else(|| AgentError::Failed("notifications are not available".into()))?;
            let allowed = self.access.inner.notify_limit.lock().unwrap_or_else(|e| e.into_inner()).allow(&caller.agent.id, Instant::now());
            if !allowed {
                return Err(AgentError::Failed(format!("too many notifications: at most {} a minute", actions::NOTIFY_MAX)));
            }
            Ok(center.agent_notice(&actions::agent_label(&caller.agent), title, body, icon.as_deref(), low))
        }
        .await;
        caller.finish(result)
    }

    async fn ask(&self, question: &str, options: HashMap<String, OwnedValue>, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> Result<HashMap<String, OwnedValue>, AgentError> {
        // `interactive` false spares the consent prompt only: the question
        // is what the call asks for.
        let caller = self.access.check(conn, &header, "Ask", READ, CallOptions::from_dict(&options)).await?;
        // Never the question, the choices or the answer: lengths, counts.
        caller.detail("question-length", activity::Detail::Count(question.chars().count() as u64));
        let result = async {
            let dialog = talk::ask_dialog(question, &options)?;
            let timeout = talk::timeout(&options)?;
            let choices = match &dialog {
                talk::Dialog::Ask { choices, .. } => choices.clone(),
                talk::Dialog::Confirm { .. } => Vec::new(),
            };
            if !choices.is_empty() {
                caller.detail("choice-count", activity::Detail::Count(choices.len() as u64));
            }
            let answer = |text: &str, index: i32| HashMap::from([("answer".to_owned(), value(text)), ("choice".to_owned(), value(index))]);
            match self.access.question(conn, &header, &caller, dialog, timeout).await? {
                talk::Reply::Text(text) => Ok(answer(&text, -1)),
                talk::Reply::Choice(i) if i < choices.len() => Ok(answer(&choices[i], i as i32)),
                _ => Err(AgentError::Cancelled("the user cancelled the question".into())),
            }
        }
        .await;
        caller.finish(result)
    }

    async fn confirm(&self, question: &str, options: HashMap<String, OwnedValue>, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> Result<bool, AgentError> {
        let caller = self.access.check(conn, &header, "Confirm", READ, CallOptions::from_dict(&options)).await?;
        caller.detail("question-length", activity::Detail::Count(question.chars().count() as u64));
        let result = async {
            let dialog = talk::confirm_dialog(question, &options)?;
            let timeout = talk::timeout(&options)?;
            match self.access.question(conn, &header, &caller, dialog, timeout).await? {
                talk::Reply::Yes(yes) => Ok(yes),
                _ => Err(AgentError::Cancelled("the user cancelled the question".into())),
            }
        }
        .await;
        caller.finish(result)
    }

    async fn add_reminder(&self, time: i64, text: &str, options: HashMap<String, OwnedValue>, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> Result<String, AgentError> {
        let caller = self.access.check(conn, &header, "AddReminder", READ, CallOptions::from_dict(&options)).await?;
        caller.detail("text-length", activity::Detail::Count(text.chars().count() as u64));
        let result = async {
            let text = talk::bounded(text, "the text", talk::QUESTION_MAX)?;
            let at = now();
            if time < at - talk::REMINDER_BEHIND || time > at + talk::REMINDER_AHEAD {
                return Err(AgentError::invalid_args("the time must be from a minute ago to 366 days ahead"));
            }
            let allowed = self.access.inner.reminder_limit.lock().unwrap_or_else(|e| e.into_inner()).allow(&caller.agent.id, Instant::now());
            if !allowed {
                return Err(AgentError::Failed(format!("too many reminders: at most {} a minute", actions::NOTIFY_MAX)));
            }
            let added = self.access.reminders().add(&caller.agent, time, text, at);
            let reminder = added.map_err(|e| match e {
                talk::AddError::Time => AgentError::invalid_args("the time must be from a minute ago to 366 days ahead"),
                talk::AddError::Full => AgentError::Failed(format!("this agent already has {} reminders waiting", talk::REMINDERS_MAX)),
            })?;
            caller.target(talk::log_target(&reminder.id));
            tracing::info!(agent = %caller.agent.id, id = %reminder.id, "agent reminder added");
            self.access.reminder_added(&reminder);
            Ok(reminder.id)
        }
        .await;
        caller.finish(result)
    }

    async fn list_reminders(&self, options: HashMap<String, OwnedValue>, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> Result<Records, AgentError> {
        let caller = self.access.check(conn, &header, "ListReminders", READ, CallOptions::from_dict(&options)).await?;
        // Its own only.
        let mine = self.access.reminders().of(Some(&caller.agent.id));
        caller.finish(Ok(mine.iter().map(|r| reminder_record(r, false)).collect()))
    }

    async fn cancel_reminder(&self, id: &str, options: HashMap<String, OwnedValue>, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> Result<(), AgentError> {
        let caller = self.access.check(conn, &header, "CancelReminder", READ, CallOptions::from_dict(&options)).await?;
        caller.target(talk::log_target(id));
        // Another agent's reminder is not there for it.
        let result = match self.access.cancel_reminder(Some(&caller.agent.id), id) {
            Some(_) => Ok(()),
            None => Err(AgentError::NotFound(format!("reminder {id}"))),
        };
        caller.finish(result)
    }

    async fn a11y_tree(&self, window: u32, options: HashMap<String, OwnedValue>, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> Result<HashMap<String, OwnedValue>, AgentError> {
        let caller = self.access.check(conn, &header, "A11yTree", APPS, CallOptions::from_dict(&options)).await?;
        caller.target(format!("window:{window}"));
        let result = async {
            let read = a11y::ReadOptions::from_dict(&options)?;
            let (w, scope) = self.access.a11y_scope(conn, &caller, window).await?;
            let tree = scope.read(&read).await?;
            caller.detail("node-count", activity::Detail::Count(tree.nodes.len() as u64));
            hidden_count(&caller, tree.nodes.iter().map(|(info, _, _)| info));
            Ok(HashMap::from([
                ("window".to_owned(), value(window)),
                ("app-id".to_owned(), value(w.app_id.as_str())),
                ("nodes".to_owned(), value(scope.records(&tree))),
                ("truncated".to_owned(), value(tree.truncated)),
            ]))
        }
        .await;
        caller.finish(result)
    }

    async fn a11y_find(&self, window: u32, options: HashMap<String, OwnedValue>, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> Result<Records, AgentError> {
        let caller = self.access.check(conn, &header, "A11yFind", APPS, CallOptions::from_dict(&options)).await?;
        caller.target(format!("window:{window}"));
        let result = async {
            let wanted = a11y::Match::from_dict(&options)?;
            let timeout_ms = match options.get("timeout-ms") {
                None => 0,
                Some(v) => u32::try_from(v.clone()).map_err(|_| AgentError::invalid_args("“timeout-ms” must be a number (u)"))?,
            };
            // What it looks for, by kind and length only.
            if let Some(role) = wanted.role {
                caller.detail("role", activity::Detail::Kind(a11y::role_name(role)));
            }
            if let Some(name) = &wanted.name {
                caller.detail("name-length", activity::Detail::Count(name.chars().count() as u64));
            }
            let (_, mut scope) = self.access.a11y_scope(conn, &caller, window).await?;
            let _busy = (timeout_ms > 0).then(|| self.access.busy(&caller.agent));
            let until = deadline(timeout_ms);
            let everything = a11y::ReadOptions { max_nodes: a11y::NODES_MAX, ..a11y::ReadOptions::default() };
            loop {
                let tree = scope.read(&everything).await?;
                let records = scope.records(&tree);
                let found: Vec<_> = tree.nodes.iter().zip(records).filter(|((info, _, _), _)| wanted.matches(info)).take(wanted.limit as usize).collect();
                if !found.is_empty() || timeout_ms == 0 {
                    caller.detail("node-count", activity::Detail::Count(found.len() as u64));
                    hidden_count(&caller, found.iter().map(|((info, _, _), _)| info));
                    return Ok(found.into_iter().map(|(_, r)| r).collect());
                }
                if until.is_some_and(|u| Instant::now() >= u) {
                    return Err(AgentError::TimedOut("no such node appeared".into()));
                }
                tokio::time::sleep(a11y::WAIT_POLL).await;
                // The switch as it is now (README.md, "Settings take
                // effect at once").
                scope.read_text_fields = self.access.recheck(&caller.agent, APPS).await?.read_text_fields;
                self.access.a11y_still_clear(&caller, window).await?;
            }
        }
        .await;
        caller.finish(result)
    }

    async fn a11y_action(
        &self,
        window: u32,
        node: &str,
        action: &str,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> Result<(), AgentError> {
        let call = CallOptions::from_dict(&options);
        let caller = self.access.check_targeted(conn, &header, "A11yAction", APPS, call).await?;
        caller.target(format!("window:{window}"));
        let result = async {
            if action.is_empty() {
                return Err(AgentError::invalid_args("no action"));
            }
            // The action's name only when it is a known one: an app names
            // its actions.
            caller.detail("action", activity::Detail::Kind(a11y::log_action(action)));
            let (w, scope) = self.access.a11y_scope(conn, &caller, window).await?;
            let target = scope.node(node, |n| caller.detail("role", activity::Detail::Kind(a11y::role_name(n.role)))).await?;
            // Pasting into a field and reading it would read the clipboard.
            if a11y::clipboard_action(action) && !caller.config.clipboard {
                return Err(AgentError::FeatureOff("clipboard".into()));
            }
            let sensitive = action != "focus" && a11y::sensitive_control(&target.name, &target.description, action);
            let need = Need { sensitive, ..APPS };
            self.access.confirm_control(&caller, "A11yAction", need, call, control(&target, &w)).await?;
            self.access.a11y_still_clear(&caller, window).await?;
            scope.act(&target, action).await
        }
        .await;
        caller.finish(result)
    }

    async fn a11y_set_value(
        &self,
        window: u32,
        node: &str,
        value: OwnedValue,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> Result<(), AgentError> {
        let call = CallOptions::from_dict(&options);
        let caller = self.access.check_targeted(conn, &header, "A11ySetValue", APPS, call).await?;
        caller.target(format!("window:{window}"));
        // Never the value: a text by its length.
        if let Value::Str(text) = &*value {
            caller.detail("text-length", activity::Detail::Count(text.chars().count() as u64));
        }
        let result = async {
            let (w, scope) = self.access.a11y_scope(conn, &caller, window).await?;
            let target = scope.node(node, |n| caller.detail("role", activity::Detail::Kind(a11y::role_name(n.role)))).await?;
            let to = a11y::SetTo::of(&value, &target)?;
            caller.detail("action", activity::Detail::Kind(if matches!(to, a11y::SetTo::Text(_)) { "set-text" } else { "set-value" }));
            self.access.confirm_control(&caller, "A11ySetValue", APPS, call, control(&target, &w)).await?;
            self.access.a11y_still_clear(&caller, window).await?;
            scope.set(&target, &to).await
        }
        .await;
        caller.finish(result)
    }

    #[zbus(signal)]
    pub async fn access_changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

/// Record in the log how many of `nodes` had their text withheld
/// (`read-text-fields` off), when any did: a count, never which.
fn hidden_count<'a>(caller: &Caller, nodes: impl Iterator<Item = &'a a11y::Info>) {
    let hidden = nodes.filter(|n| n.value_hidden).count();
    if hidden > 0 {
        caller.detail("hidden-count", activity::Detail::Count(hidden as u64));
    }
}

/// How the sensitive-action prompt names an app's control: by its name
/// (as plain text, at most 64 characters), role and app.
fn control(node: &a11y::Info, w: &desktop::WindowInfo) -> Control {
    let name: String = talk::clean(&node.name).chars().take(64).collect();
    Control { name, role: a11y::role_name(node.role), app: w.app_id.clone() }
}

/// `org.helixdesktop.AgentAccess1`, for the Settings app.
pub struct AgentAccessSettings {
    pub access: AgentAccess,
}

#[interface(name = "org.helixdesktop.AgentAccess1")]
impl AgentAccessSettings {
    async fn list_agents(&self, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> fdo::Result<Vec<HashMap<String, OwnedValue>>> {
        self.access.require_settings(conn, &header, "ListAgents").await?;
        Ok(self.access.list())
    }

    async fn revoke(&self, agent: &str, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> fdo::Result<()> {
        let exe = self.access.require_settings(conn, &header, "Revoke").await?;
        self.access.revoke(agent);
        self.access.record_settings(&exe, "Revoke", agent);
        Ok(())
    }

    async fn forget(&self, agent: &str, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> fdo::Result<()> {
        let exe = self.access.require_settings(conn, &header, "Forget").await?;
        self.access.forget(agent);
        self.access.record_settings(&exe, "Forget", agent);
        Ok(())
    }

    async fn stop_all(&self, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> fdo::Result<()> {
        let exe = self.access.require_settings(conn, &header, "StopAll").await?;
        self.access.stop_all();
        self.access.record_settings(&exe, "StopAll", "");
        Ok(())
    }

    async fn clear_log(&self, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> fdo::Result<()> {
        let exe = self.access.require_settings(conn, &header, "ClearLog").await?;
        let log = self.access.inner.log.clone();
        // The one Settings call recorded: that the log was cleared, and
        // by whom.
        let entry = activity::Entry { time: now(), agent: exe.to_string_lossy().into_owned(), method: "ClearLog".to_owned(), ..activity::Entry::default() };
        let _ = tokio::task::spawn_blocking(move || {
            log.clear();
            log.record(entry);
        })
        .await;
        tracing::info!("agent activity log cleared");
        Ok(())
    }

    /// Every agent's waiting reminders (Reminder records with `agent`,
    /// `executable` and `name`), by time.
    async fn list_reminders(&self, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> fdo::Result<Vec<HashMap<String, OwnedValue>>> {
        self.access.require_settings(conn, &header, "ListReminders").await?;
        Ok(self.access.reminders().of(None).iter().map(|r| reminder_record(r, true)).collect())
    }

    /// Cancel any agent's reminder (unknown ids are not an error).
    async fn cancel_reminder(&self, id: &str, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> fdo::Result<()> {
        let exe = self.access.require_settings(conn, &header, "CancelReminder").await?;
        self.access.cancel_reminder(None, id);
        self.access.record_settings(&exe, "CancelReminder", &talk::log_target(id));
        Ok(())
    }

    #[zbus(signal)]
    pub async fn agents_changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    /// The waiting reminders changed.
    #[zbus(signal)]
    pub async fn reminders_changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}
