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
//! with the shell as the only client agents reach ([`a11y`]). The
//! clipboard methods (helixdesktop/desktop#151) read and set the clipboard
//! through the compositor, refusing content whose source is protected
//! ([`clipboard`]). The *Screen and input* methods
//! (helixdesktop/desktop#150) take screenshots the compositor draws with
//! protected windows and shell surfaces filled, and send synthetic input
//! the compositor checks before every event and stops when the user takes
//! over ([`screen`]). The scoped actions' first batch
//! (helixdesktop/desktop#156) reads the media players and controls them
//! ([`players`]), reads the battery and power mode ([`power`]), and lists
//! the files starred in the store GNOME Files shares ([`files`]). The
//! second batch (helixdesktop/desktop#157) changes settings (the
//! background, the accent color, the power mode, quick settings' `until`,
//! stars and the dock's favorites), keeping each change with its old
//! value so it can be put back when its time runs out, when the user
//! stops the agent, or from Settings ([`changes`], [`background`]); and
//! dismisses notifications, presses their buttons, reveals files and moves
//! windows to another monitor.

pub mod a11y;
pub mod actions;
pub mod activity;
pub mod background;
pub mod changes;
pub mod clipboard;
pub mod desktop;
pub mod document;
pub mod files;
pub mod grants;
pub mod identity;
pub mod players;
pub mod policy;
pub mod power;
pub mod prompt;
pub mod screen;
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
    /// Until when a temporary change lasts (Unix seconds), for the prompt
    /// to say.
    pub until: Option<i64>,
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
    /// Where agents' changes and pending restores are kept (`None`:
    /// memory only).
    pub changes_path: Option<PathBuf>,
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
            changes_path: Some(changes::default_path()),
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
    /// Until when a temporary change lasts, for the prompt (the method
    /// sets it from its `until`).
    pub until: Option<i64>,
}

impl CallOptions {
    pub fn from_dict(options: &HashMap<String, OwnedValue>) -> CallOptions {
        let flag = |key: &str, default: bool| options.get(key).and_then(|v| bool::try_from(v.clone()).ok()).unwrap_or(default);
        CallOptions { interactive: flag("interactive", true), sensitive: flag("sensitive", false), action: None, until: None }
    }
}

impl Default for CallOptions {
    fn default() -> Self {
        CallOptions { interactive: true, sensitive: false, action: None, until: None }
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
            e.reason = activity::reason_of(err);
        }
        result
    }

    /// The call's result, recorded.
    pub fn finish<T>(self, result: Result<T, AgentError>) -> Result<T, AgentError> {
        if let (Err(err), Some(e)) = (&result, self.entry().as_mut()) {
            let (outcome, error) = activity::outcome_of(err);
            e.outcome = outcome;
            e.reason = activity::reason_of(err);
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
    /// Until when each agent shows as controlling the screen (its last
    /// screenshot or input, and [`screen::CONTROL_SHOWN`]).
    controlling: Mutex<HashMap<String, Instant>>,
    /// Input runs going on: their agent and what stops them.
    inputs: Mutex<Vec<(u64, String, Arc<tokio::sync::Notify>)>>,
    next_input: std::sync::atomic::AtomicU64,
    /// Agents' changes and pending restores; one change, restore or undo
    /// at a time; what wakes the restores' timer.
    changes: Mutex<changes::Changes>,
    change_turn: tokio::sync::Mutex<()>,
    changes_wake: Arc<tokio::sync::Notify>,
    /// Changes made, for their rate limit (by method and agent).
    change_limit: Mutex<actions::NotifyLimit>,
    /// The connection serving notifications (their signals go out on it).
    public: OnceLock<Connection>,
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
                controlling: Mutex::new(HashMap::new()),
                inputs: Mutex::new(Vec::new()),
                next_input: std::sync::atomic::AtomicU64::new(1),
                changes: Mutex::new(changes::Changes::load(setup.changes_path)),
                change_turn: Default::default(),
                changes_wake: Default::default(),
                change_limit: Mutex::new(actions::NotifyLimit::with_max(changes::CHANGES_PER_MINUTE)),
                public: OnceLock::new(),
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
        self.start_changes();
        self.changes_settings(&Config::read());
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
        if !config.enabled {
            self.cancel_inputs(None);
        }
        if !config.enabled && self.grants().revoke_all() {
            tracing::info!("agent access turned off; session grants ended");
        }
        self.changes_settings(config);
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
                entry.reason = activity::reason_of(&err);
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
        let answer = (self.inner.confirm)(ConfirmRequest { agent: agent.clone(), method, action: options.action, control, until: options.until }).await;
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
        prompt::Active { agent: r.agent.clone(), executable: r.executable.clone(), name: r.name.clone(), level: Level::ReadOnly, controlling: false, restores: Vec::new() }.label()
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

    /// Have the UI do clipboard request `make(reply)` through the
    /// compositor.
    async fn clipboard_request<T>(&self, make: impl FnOnce(tokio::sync::oneshot::Sender<Result<T, String>>) -> clipboard::Request) -> Result<T, AgentError> {
        let unavailable = || AgentError::Failed("the clipboard is not available".into());
        let ui = self.inner.desktop.get().ok_or_else(unavailable)?.ui.clone();
        let (reply, answer) = tokio::sync::oneshot::channel();
        ui.send(crate::UiEvent::AgentClipboard(clipboard::ClipboardSlot::new(make(reply)))).map_err(|_| unavailable())?;
        let done = tokio::time::timeout(Duration::from_secs(5), answer).await.map_err(|_| unavailable())?.map_err(|_| unavailable())?;
        done.map_err(AgentError::NotSupported)
    }

    /// The clipboard methods' checks of the moment: not locked, and no
    /// system prompt shown (README.md, "Hard exclusions"). The windows,
    /// for checking the clipboard's source.
    async fn clipboard_clear(&self, caller: &Caller) -> Result<desktop::Snapshot, AgentError> {
        if self.locked() {
            return Err(AgentError::Locked("the screen is locked".into()));
        }
        let snapshot = self.snapshot().await?;
        if snapshot.system_prompt {
            caller.check_target(Target::SystemPrompt)?;
        }
        Ok(snapshot)
    }

    /// What the clipboard offers, for `caller` to read: refused when the
    /// shell, Settings, a system prompt's program or an excluded app set
    /// it (by its app id, its process's other windows, program or unit),
    /// and when a password manager marked it secret. The app that set it
    /// is the call's target.
    async fn clipboard_offer(&self, caller: &Caller) -> Result<clipboard::Offer, AgentError> {
        let snapshot = self.clipboard_clear(caller).await?;
        let offer = self.clipboard_request(clipboard::Request::Peek).await?;
        if !offer.app_id.is_empty() {
            caller.target(offer.app_id.clone());
        }
        if let Some(target) = clipboard::source_target(&offer) {
            caller.check_target(target)?;
        }
        if matches!(offer.source, clipboard::SourceKind::App | clipboard::SourceKind::X11) && offer.pid != 0 {
            let (config, proc_root, pid) = (caller.config.clone(), self.inner.proc_root.clone(), offer.pid);
            blocking(move || a11y::check_process(pid, &snapshot, &config, &desktop::installed(), &proc_root)).await?;
        }
        if offer.types.iter().any(|t| t == clipboard::PASSWORD_HINT) {
            // Unreadable counts as secret.
            let hint = self.clipboard_read(&offer, clipboard::PASSWORD_HINT, 64, clipboard::hint_timeout()).await;
            if hint.map_or(true, |h| clipboard::hint_is_secret(&h)) {
                caller.check_target(Target::PasswordField)?;
            }
        }
        Ok(offer)
    }

    /// The clipboard of `offer` as `mime`: at most `max` bytes within
    /// `timeout`. Nothing when the clipboard changed since `offer`.
    async fn clipboard_read(&self, offer: &clipboard::Offer, mime: &str, max: usize, timeout: Duration) -> Result<Vec<u8>, AgentError> {
        let (generation, mime) = (offer.generation, mime.to_owned());
        let fd = self.clipboard_request(|reply| clipboard::Request::Read { generation, mime, reply }).await?;
        blocking(move || {
            clipboard::read_pipe(fd, max, timeout).map_err(|e| match e {
                clipboard::ReadError::TooLarge => AgentError::Failed(format!("the clipboard holds more than {} MiB", max >> 20)),
                clipboard::ReadError::TimedOut => AgentError::TimedOut("the app that copied did not send it in time".into()),
                clipboard::ReadError::Failed(e) => AgentError::Failed(e),
            })
        })
        .await
    }

    /// Have the UI make screen request `make(reply)` of the compositor,
    /// waiting at most `timeout` for the answer.
    async fn screen_request<T>(&self, make: impl FnOnce(tokio::sync::oneshot::Sender<T>) -> screen::Request, timeout: Duration) -> Result<T, AgentError> {
        let unavailable = || AgentError::Failed("the screen is not available".into());
        let ui = self.inner.desktop.get().ok_or_else(unavailable)?.ui.clone();
        let (reply, answer) = tokio::sync::oneshot::channel();
        ui.send(crate::UiEvent::AgentScreen(screen::ScreenSlot::new(make(reply)))).map_err(|_| unavailable())?;
        tokio::time::timeout(timeout, answer).await.map_err(|_| AgentError::Failed("the compositor did not answer in time".into()))?.map_err(|_| unavailable())
    }

    /// Have the UI make screen request `request`, without an answer.
    fn screen_send(&self, request: screen::Request) {
        if let Some(desktop) = self.inner.desktop.get() {
            let _ = desktop.ui.send(crate::UiEvent::AgentScreen(screen::ScreenSlot::new(request)));
        }
    }

    /// The *Screen and input* methods' checks of the moment: not locked,
    /// no system prompt shown. The windows.
    async fn screen_clear(&self, caller: &Caller) -> Result<desktop::Snapshot, AgentError> {
        if self.locked() {
            return Err(AgentError::Locked("the screen is locked".into()));
        }
        let snapshot = self.snapshot().await?;
        if snapshot.system_prompt {
            caller.check_target(Target::SystemPrompt)?;
        }
        Ok(snapshot)
    }

    /// The windows a screenshot may show: those whose app, and process
    /// (where it is known), are not protected (README.md, "Screen and
    /// input methods").
    async fn shown_windows(&self, caller: &Caller, snapshot: &desktop::Snapshot) -> Vec<u32> {
        let (config, proc_root, shown) = (caller.config.clone(), self.inner.proc_root.clone(), snapshot.clone());
        blocking(move || {
            let installed = desktop::installed();
            Ok(shown
                .windows
                .iter()
                .filter(|w| policy::check_target(&config, Target::App(&w.app_id)).is_ok())
                .filter(|w| w.pid == 0 || a11y::check_process(w.pid, &shown, &config, &installed, &proc_root).is_ok())
                .map(|w| w.id)
                .collect())
        })
        .await
        .unwrap_or_default()
    }

    /// Window `id` as a screenshot's or input's target: recorded, and
    /// refused when its app or process is protected.
    async fn screen_window(&self, caller: &Caller, snapshot: &desktop::Snapshot, id: u32) -> Result<desktop::WindowInfo, AgentError> {
        let Some(w) = snapshot.windows.iter().find(|w| w.id == id).cloned() else { return Err(AgentError::NotFound(format!("window {id}"))) };
        caller.target(format!("window:{id} ({})", w.app_id));
        caller.check_target(Target::App(&w.app_id))?;
        if w.pid != 0 {
            let (config, proc_root, shown, pid) = (caller.config.clone(), self.inner.proc_root.clone(), snapshot.clone(), w.pid);
            blocking(move || a11y::check_process(pid, &shown, &config, &desktop::installed(), &proc_root)).await?;
        }
        Ok(w)
    }

    /// `agent` took a screenshot or sends input: the indicator says it
    /// controls the screen, for [`screen::CONTROL_SHOWN`] more.
    fn controlling(&self, agent: &Agent) {
        self.inner.controlling.lock().unwrap_or_else(|e| e.into_inner()).insert(agent.id.clone(), Instant::now() + screen::CONTROL_SHOWN);
        self.publish();
        let weak = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            tokio::time::sleep(screen::CONTROL_SHOWN + Duration::from_millis(50)).await;
            if let Some(inner) = weak.upgrade() {
                AgentAccess { inner }.publish();
            }
        });
    }

    /// Stop `agent`'s input runs (`None`: everyone's) at once: *Stop*,
    /// Settings' `Revoke` and `StopAll`, access turned off.
    pub fn cancel_inputs(&self, agent: Option<&str>) {
        for (_, a, stop) in self.inner.inputs.lock().unwrap_or_else(|e| e.into_inner()).iter() {
            if agent.is_none_or(|x| x == a) {
                stop.notify_one();
            }
        }
    }

    /// Send `plan`'s steps into `window` for `caller`, one at a time, each
    /// checked by the compositor just before it is sent and by the shell
    /// (the grant, system prompts) before it goes. How many steps were
    /// done, and the error that stopped the rest.
    async fn run_input(&self, caller: &Caller, window: u32, plan: &screen::Plan) -> (u64, Result<(), AgentError>) {
        let run_id = self.inner.next_input.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let stop = Arc::new(tokio::sync::Notify::new());
        self.inner.inputs.lock().unwrap_or_else(|e| e.into_inner()).push((run_id, caller.agent.id.clone(), stop.clone()));
        let agent = caller.agent.id.clone();
        let started = self.screen_request(|reply| screen::Request::Start { agent, window, reply }, Duration::from_secs(5)).await;
        let result = match started {
            Ok(Ok(run)) => {
                let result = self.input_steps(caller, window, run, plan, &stop).await;
                self.screen_send(screen::Request::End { run });
                result
            }
            Ok(Err(why)) => (0, Err(AgentError::NotSupported(why))),
            Err(err) => (0, Err(err)),
        };
        self.inner.inputs.lock().unwrap_or_else(|e| e.into_inner()).retain(|(id, _, _)| *id != run_id);
        result
    }

    async fn input_steps(&self, caller: &Caller, window: u32, run: u64, plan: &screen::Plan, stop: &tokio::sync::Notify) -> (u64, Result<(), AgentError>) {
        let stopped = || AgentError::Cancelled("stopped".into());
        let mut done = 0u64;
        for step in &plan.steps {
            // The grant, the level, the lock (README.md, "Settings take
            // effect at once"), and the shell's system prompts.
            if let Err(err) = self.recheck(&caller.agent, SCREEN).await {
                return (done, Err(err));
            }
            if let Err(err) = self.screen_clear(caller).await {
                return (done, Err(err));
            }
            self.controlling(&caller.agent);
            if let screen::Step::Wait(ms) = step {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(u64::from(*ms))) => {}
                    _ = stop.notified() => return (done, Err(stopped())),
                }
                done += 1;
                continue;
            }
            // Long enough for a text typed a character at a time.
            let chars = if let screen::Step::Text(t) = step { t.chars().count() as u64 } else { 1 };
            let timeout = Duration::from_secs(5) + Duration::from_millis(20 * chars);
            let (reply, answer) = tokio::sync::oneshot::channel();
            self.screen_send(screen::Request::Step { run, step: step.clone(), reply });
            let outcome = tokio::select! {
                got = tokio::time::timeout(timeout, answer) => got,
                _ = stop.notified() => {
                    self.screen_send(screen::Request::Cancel { run });
                    return (done, Err(stopped()));
                }
            };
            match outcome {
                Ok(Ok(Ok(()))) => done += 1,
                Ok(Ok(Err(why))) => {
                    // A stop the hard stop or *Stop* caused is the
                    // user's: Cancelled.
                    if why == screen::Stopped::Focus && self.locked() {
                        return (done, Err(AgentError::Locked("the screen is locked".into())));
                    }
                    return (done, Err(why.error(window)));
                }
                Ok(Err(_)) => return (done, Err(AgentError::Failed("the input could not be sent".into()))),
                Err(_) => {
                    self.screen_send(screen::Request::Cancel { run });
                    return (done, Err(AgentError::Failed("the compositor did not carry out the input in time".into())));
                }
            }
        }
        (done, Ok(()))
    }

    /// The media players `caller` may see: not Settings' nor an excluded
    /// app's (README.md, "Media, power and stars").
    async fn players_for(&self, conn: &Connection, caller: &Caller) -> Result<Vec<players::Player>, AgentError> {
        let all = players::read(conn).await;
        Ok(self.players_visible(caller, all).await)
    }

    /// Of `all`, the players `caller` may see.
    async fn players_visible(&self, caller: &Caller, all: Vec<players::Player>) -> Vec<players::Player> {
        if all.is_empty() {
            return all;
        }
        let snapshot = self.snapshot().await.unwrap_or_default();
        let (config, proc_root) = (caller.config.clone(), self.inner.proc_root.clone());
        blocking(move || {
            let installed = desktop::installed();
            Ok(all.into_iter().filter(|p| players::check(p, &snapshot, &config, &installed, &proc_root).is_ok()).collect())
        })
        .await
        .unwrap_or_default()
    }

    /// Step 9 for media player `p`: refused when it is Settings' or an
    /// excluded app's.
    async fn player_check(&self, caller: &Caller, p: &players::Player) -> Result<(), AgentError> {
        let snapshot = self.snapshot().await.unwrap_or_default();
        let (config, proc_root, p) = (caller.config.clone(), self.inner.proc_root.clone(), p.clone());
        blocking(move || players::check(&p, &snapshot, &config, &desktop::installed(), &proc_root)).await
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

    /// Notification `id` when `ListNotifications` would give it to
    /// `caller` now (not an excluded app's, nor about an excluded app's
    /// window); `NotFound` otherwise, so nothing is learnt of the rest.
    async fn visible_note(&self, caller: &Caller, id: u32) -> Result<(crate::notifications::Center, crate::notify_store::Notification), AgentError> {
        let not_found = || AgentError::NotFound(format!("notification {id}"));
        let center = self.inner.desktop.get().and_then(|d| d.notifications.clone()).ok_or_else(not_found)?;
        let (note, time) = self.notifications().into_iter().find(|(n, _)| n.id == id).ok_or_else(not_found)?;
        let info = desktop::note_info(&note, time);
        if !info.app_id.is_empty() {
            caller.target(format!("notification:{id} ({})", info.app_id));
        }
        let snapshot = self.snapshot().await.unwrap_or_default();
        let config = caller.config.clone();
        let hidden = blocking(move || Ok(desktop::note_hidden(&info, &snapshot, &desktop::installed(), &config))).await?;
        if hidden {
            return Err(not_found());
        }
        Ok((center, note))
    }

    /// `RevealFile`'s step 9: the file manager (the app owning
    /// `org.freedesktop.FileManager1`, else the default app for folders
    /// when the name is only activatable) must not be excluded or
    /// Settings. `NotSupported` when there is none.
    async fn file_manager_check(&self, conn: &Connection, caller: &Caller) -> Result<(), AgentError> {
        let none = || AgentError::NotSupported("no file manager serves org.freedesktop.FileManager1".into());
        let dbus = fdo::DBusProxy::new(conn).await.map_err(|_| none())?;
        let name = zbus::names::BusName::from_static_str(FILE_MANAGER).map_err(|_| none())?;
        if let Ok(owner) = dbus.get_name_owner(name.clone()).await {
            let pid = dbus.get_connection_unix_process_id(zbus::names::BusName::Unique(owner.into_inner())).await.unwrap_or(0);
            let snapshot = self.snapshot().await.unwrap_or_default();
            let (config, proc_root) = (caller.config.clone(), self.inner.proc_root.clone());
            return blocking(move || a11y::check_app_process(pid, &snapshot, &config, &desktop::installed(), &proc_root)).await;
        }
        let activatable = dbus.list_activatable_names().await.unwrap_or_default();
        if !activatable.iter().any(|n| n.as_str() == FILE_MANAGER) {
            return Err(none());
        }
        for app in blocking(|| Ok(actions::folder_handler().unwrap_or_default())).await? {
            caller.check_target(Target::App(&app))?;
        }
        Ok(())
    }

    /// Send notifications' signals on `public` (the connection serving
    /// them) from now on.
    pub fn set_public(&self, public: &Connection) {
        let _ = self.inner.public.set(public.clone());
    }

    fn public_conn(&self) -> Option<Connection> {
        self.inner.public.get().cloned()
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
    /// an allowed call in the last minute or a call running; and agents
    /// with temporary changes waiting to be put back, with their rows.
    pub fn active(&self, now: i64) -> Vec<prompt::Active> {
        let config = Config::read();
        if !config.enabled {
            return Vec::new();
        }
        let running = self.inner.running.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let controlling = {
            let mut c = self.inner.controlling.lock().unwrap_or_else(|e| e.into_inner());
            let at = Instant::now();
            c.retain(|_, until| *until > at);
            c.clone()
        };
        let mut pending = self.pending_rows();
        let grants = self.grants();
        let shown: Vec<(&String, &grants::Entry)> = grants
            .list()
            .filter(|(id, e)| running.get(*id).is_some_and(|n| *n > 0) || (e.last_active > 0 && now - e.last_active < prompt::ACTIVE_FOR) || pending.contains_key(*id))
            .collect();
        let mut out: Vec<prompt::Active> = shown
            .into_iter()
            .filter_map(|(id, e)| {
                let level = grants.effective(id, &config).or(pending.contains_key(id).then_some(Level::Desktop))?;
                let restores = pending.remove(id).map(|(_, _, rows)| rows).unwrap_or_default();
                Some(prompt::Active { agent: id.clone(), executable: e.executable.clone(), name: e.name.clone(), level, controlling: controlling.contains_key(id), restores })
            })
            .collect();
        // Agents whose grant is gone (a shell restarted meanwhile) still
        // show what they will have put back.
        for (id, (executable, name, restores)) in pending {
            out.push(prompt::Active { agent: id, executable, name, level: Level::Desktop, controlling: false, restores });
        }
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
        self.cancel_inputs(Some(agent));
        self.restore_soon(Some(agent), changes::Cause::Stopped);
        if self.grants().revoke(agent) {
            tracing::info!(agent, "agent access revoked");
            self.changed();
        }
    }

    /// *Stop* in the indicator: end `agent`'s session grant, and ask again
    /// even if it is always allowed (README.md, "Indicator").
    pub fn stop(&self, agent: &str) {
        self.cancel_inputs(Some(agent));
        self.restore_soon(Some(agent), changes::Cause::Stopped);
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
        self.cancel_inputs(Some(agent));
        self.restore_soon(Some(agent), changes::Cause::Stopped);
        if self.grants().forget(agent) {
            tracing::info!(agent, "agent forgotten");
            self.changed();
        }
    }

    /// Revoke every agent.
    pub fn stop_all(&self) {
        self.cancel_inputs(None);
        self.restore_soon(None, changes::Cause::Stopped);
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
/// `GetClipboard`'s: reading, with the `clipboard` switch.
const CLIPBOARD_READ: Need = Need { switch: Some(policy::Switch::Clipboard), ..READ };
/// `SetClipboard`'s: controlling the desktop, with the `clipboard` switch.
const CLIPBOARD_SET: Need = Need { switch: Some(policy::Switch::Clipboard), ..DESKTOP };
/// A *Screen and input* method's.
const SCREEN: Need = Need::level(Level::ScreenAndInput);
/// `NotificationAction`'s: controlling the desktop, with the
/// `notifications` switch (the buttons' labels say what the notification
/// says).
const NOTE_ACTION: Need = Need { switch: Some(policy::Switch::Notifications), ..DESKTOP };
/// The file manager's name (`RevealFile`).
const FILE_MANAGER: &str = "org.freedesktop.FileManager1";

/// Run `f` where it may block (GIO looking up and starting apps).
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, AgentError> + Send + 'static) -> Result<T, AgentError> {
    tokio::task::spawn_blocking(f).await.map_err(|e| AgentError::Failed(e.to_string()))?
}

type Records = Vec<HashMap<String, OwnedValue>>;
type Record = HashMap<String, OwnedValue>;

/// The quick settings' state for `GetState` (README.md, "Records",
/// State): the settings, and what the sound server, NetworkManager and
/// the backlight say when they answer.
async fn quick_state(conn: &Connection, system: Option<&Connection>) -> HashMap<String, OwnedValue> {
    let mut out = {
        let interface = helix_settings::Schema::open("org.gnome.desktop.interface");
        let notifications = helix_settings::Schema::open("org.gnome.desktop.notifications");
        let color = helix_settings::Schema::open("org.gnome.settings-daemon.plugins.color");
        let mut out = HashMap::from([
            ("dark-style".to_owned(), value(interface.get::<String>("color-scheme").as_deref() == Some("prefer-dark"))),
            ("do-not-disturb".to_owned(), value(!notifications.get_or("show-banners", true))),
            ("night-light".to_owned(), value(color.get_or("night-light-enabled", false))),
        ]);
        // As `SetAccent` names it (batch 2).
        if let Some(accent) = interface.get::<String>("accent-color") {
            out.insert("accent".to_owned(), value(accent));
        }
        out
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
    /// `WindowAction` `to-monitor`: move window `window` a monitor at a
    /// time, as Super+Shift+arrow does, until it is on the one named.
    async fn to_monitor(&self, caller: &Caller, window: u32, options: &HashMap<String, OwnedValue>, call: CallOptions) -> Result<(), AgentError> {
        caller.detail("action", activity::Detail::Kind("to-monitor"));
        let connector = actions::monitor_option(options)?;
        let snapshot = self.access.snapshot().await?;
        let Some(w) = snapshot.windows.iter().find(|w| w.id == window).cloned() else { return Err(AgentError::NotFound(format!("window {window}"))) };
        caller.target(format!("window:{window} ({})", w.app_id));
        caller.check_target(Target::App(&w.app_id))?;
        actions::to_monitor_step(&w, &snapshot, &connector)?;
        self.access.confirm(caller, "WindowAction", DESKTOP, call).await?;
        let on = |s: &desktop::Snapshot| s.windows.iter().find(|w| w.id == window).and_then(|w| desktop::monitor_of(s, w.geometry)).map(|m| m.connector.clone());
        for _ in 0..snapshot.monitors.len().max(1) {
            let snapshot = self.access.snapshot().await?;
            let Some(w) = snapshot.windows.iter().find(|w| w.id == window).cloned() else { return Err(AgentError::NotFound(format!("window {window}"))) };
            let Some(step) = actions::to_monitor_step(&w, &snapshot, &connector)? else { return Ok(()) };
            let before = on(&snapshot);
            self.access.window_ops(vec![actions::WindowOp::Action(window, step)]).await?;
            self.access.settle(|s| on(s) != before).await;
            if on(&self.access.snapshot().await?) == before {
                return Err(AgentError::Failed("the window could not be moved to that monitor".into()));
            }
        }
        match on(&self.access.snapshot().await?) {
            Some(c) if c == connector => Ok(()),
            _ => Err(AgentError::Failed("the window could not be moved to that monitor".into())),
        }
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
            .map(|(n, time)| desktop::note_info(&n, time))
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

    async fn starred_files(&self, options: HashMap<String, OwnedValue>, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> Result<Records, AgentError> {
        let caller = self.access.check(conn, &header, "StarredFiles", READ, CallOptions::from_dict(&options)).await?;
        let result = async {
            let limit = files::limit(&options)?;
            let mime = actions::text_option(&options, "type")?.filter(|m| !m.is_empty());
            tokio::task::spawn_blocking(move || {
                let home = gio::glib::home_dir();
                let found = files::starred(&home, &helix_settings::stars::data_home(), mime.as_deref(), limit);
                found.iter().map(files::FileInfo::record).collect()
            })
            .await
            .map_err(|e| AgentError::Failed(e.to_string()))
        }
        .await;
        caller.finish(result)
    }

    async fn get_media(&self, options: HashMap<String, OwnedValue>, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> Result<Records, AgentError> {
        let caller = self.access.check(conn, &header, "GetMedia", READ, CallOptions::from_dict(&options)).await?;
        let result = async {
            let shown = self.access.players_for(conn, &caller).await?;
            Ok(players::records(&shown.iter().collect::<Vec<_>>()))
        }
        .await;
        caller.finish(result)
    }

    async fn get_power(&self, options: HashMap<String, OwnedValue>, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> Result<HashMap<String, OwnedValue>, AgentError> {
        let caller = self.access.check(conn, &header, "GetPower", READ, CallOptions::from_dict(&options)).await?;
        let system = self.access.system_bus().await;
        let record = power::read(system.as_ref()).await;
        caller.finish(Ok(record))
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
        if action == "to-monitor" {
            let result = self.to_monitor(&caller, window, &options, call).await;
            return caller.finish(result);
        }
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
        let until = changes::until_option(&options);
        let call = CallOptions { action: toggle.and_then(|t| t.action(on)), until: until.as_ref().ok().copied().flatten(), ..CallOptions::from_dict(&options) };
        let caller = self.access.check_targeted(conn, &header, "SetToggle", need, call).await?;
        let result = async {
            let toggle = toggle.ok_or_else(|| AgentError::NotSupported(format!("toggle {name}")))?;
            caller.target(toggle.name());
            caller.detail("state", activity::Detail::Kind(if on { "on" } else { "off" }));
            let until = until?.map(|u| changes::check_until(u, now())).transpose()?;
            self.access.confirm(&caller, "SetToggle", need, call).await?;
            // Its reply stays empty (README.md, "Changing settings"): the
            // change is in the log.
            self.access.make_change(&caller, "SetToggle", changes::Setting::Toggle(toggle), None, until, |_| changes::Values::flag(on)).await.map(drop)
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

    async fn media_control(
        &self,
        player: &str,
        action: &str,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> Result<(), AgentError> {
        let call = CallOptions::from_dict(&options);
        let caller = self.access.check_targeted(conn, &header, "MediaControl", DESKTOP, call).await?;
        if !player.is_empty() {
            caller.target(format!("player:{player}"));
        }
        let result = async {
            let action = players::Action::parse(action, &options)?;
            caller.detail("action", activity::Detail::Kind(action.name()));
            let all = players::read(conn).await;
            // The player named, or the active one of those the agent may
            // see (an excluded app's is never the active one).
            let target = if player.is_empty() {
                let shown = self.access.players_visible(&caller, all).await;
                let active = players::active(&shown.iter().collect::<Vec<_>>()).map(str::to_owned);
                let found = shown.into_iter().find(|p| Some(&p.bus_name) == active.as_ref());
                found.ok_or_else(|| AgentError::NotFound("no media player".into()))?
            } else {
                let found = all.into_iter().find(|p| p.id() == player);
                let found = found.ok_or_else(|| AgentError::NotFound(format!("player {player}")))?;
                caller.target(found.log_target());
                self.access.player_check(&caller, &found).await?;
                found
            };
            caller.target(target.log_target());
            action.supported(&target)?;
            self.access.confirm(&caller, "MediaControl", DESKTOP, call).await?;
            action.send(conn, &target).await
        }
        .await;
        caller.finish(result)
    }

    async fn set_wallpaper(&self, uri: &str, options: HashMap<String, OwnedValue>, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> Result<Record, AgentError> {
        let until = changes::until_option(&options);
        let call = CallOptions { until: until.as_ref().ok().copied().flatten(), ..CallOptions::from_dict(&options) };
        let caller = self.access.check_targeted(conn, &header, "SetWallpaper", DESKTOP, call).await?;
        caller.target("wallpaper");
        let result = async {
            let variant: &'static str = match actions::text_option(&options, "variant")?.as_deref() {
                None | Some("both") => "both",
                Some("light") => "light",
                Some("dark") => "dark",
                Some(_) => return Err(AgentError::invalid_args("“variant” must be light, dark or both")),
            };
            caller.detail("variant", activity::Detail::Kind(variant));
            let fit = actions::text_option(&options, "fit")?;
            if fit.as_deref().is_some_and(|f| !changes::FITS.contains(&f)) {
                return Err(AgentError::invalid_args(format!("“fit” must be one of {}", changes::FITS.join(", "))));
            }
            let until = until?.map(|u| changes::check_until(u, now())).transpose()?;
            let path = match actions::Uri::parse(uri)? {
                actions::Uri::File(path) => path,
                actions::Uri::Address { scheme, .. } => return Err(AgentError::NotSupported(format!("{scheme}: only local files can be the background"))),
            };
            // Opened once; every check is of the file opened (step 9 for
            // where it is).
            let places = background::Places::current();
            let (p, opened_in) = (path.clone(), places.clone());
            let picture = blocking(move || background::open(&p, &opened_in)).await?;
            self.access.confirm(&caller, "SetWallpaper", DESKTOP, call).await?;
            let copied = !places.in_backgrounds(&picture.real);
            let picture = Arc::new(picture);
            let (shown, placed_in) = (picture.clone(), places.clone());
            let uri = blocking(move || background::place(&shown, &placed_in, &background::stamp())).await?;
            let made = self
                .access
                .make_change(&caller, "SetWallpaper", changes::Setting::Wallpaper, Some(variant), until, |old| {
                    let mut new = old.clone();
                    if variant != "dark" {
                        new.value = changes::Val::Text(uri.clone());
                    }
                    if variant != "light" {
                        new.dark = Some(uri.clone());
                    }
                    // Without a fit, a picture would not show with none.
                    new.fit = match fit {
                        Some(f) => Some(f),
                        None if old.fit.as_deref() == Some("none") => Some("zoom".to_owned()),
                        None => old.fit.clone(),
                    };
                    new
                })
                .await;
            if made.is_err() && copied {
                if let Some(path) = gio::glib::filename_from_uri(&uri).ok().map(|(p, _)| p) {
                    let _ = std::fs::remove_file(path);
                }
            }
            made
        }
        .await;
        caller.finish(result)
    }

    async fn set_accent(&self, name: &str, options: HashMap<String, OwnedValue>, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> Result<Record, AgentError> {
        let until = changes::until_option(&options);
        let call = CallOptions { until: until.as_ref().ok().copied().flatten(), ..CallOptions::from_dict(&options) };
        let caller = self.access.check_targeted(conn, &header, "SetAccent", DESKTOP, call).await?;
        caller.target("accent");
        let result = async {
            if !changes::ACCENTS.contains(&name) {
                return Err(AgentError::NotSupported(format!("accent color {name}")));
            }
            let until = until?.map(|u| changes::check_until(u, now())).transpose()?;
            self.access.confirm(&caller, "SetAccent", DESKTOP, call).await?;
            self.access.make_change(&caller, "SetAccent", changes::Setting::Accent, None, until, |_| changes::Values::text(name)).await
        }
        .await;
        caller.finish(result)
    }

    async fn set_power_mode(&self, profile: &str, options: HashMap<String, OwnedValue>, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> Result<Record, AgentError> {
        let until = changes::until_option(&options);
        let call = CallOptions { until: until.as_ref().ok().copied().flatten(), ..CallOptions::from_dict(&options) };
        let caller = self.access.check_targeted(conn, &header, "SetPowerMode", DESKTOP, call).await?;
        caller.target("power-mode");
        let result = async {
            if !changes::PROFILES.contains(&profile) {
                return Err(AgentError::NotSupported(format!("power profile {profile}")));
            }
            let until = until?.map(|u| changes::check_until(u, now())).transpose()?;
            let system = self.access.system_bus().await;
            let power = power::read(system.as_ref()).await;
            let offered: Vec<String> = power.get("power-profiles").and_then(|v| Vec::<String>::try_from(v.clone()).ok()).unwrap_or_default();
            if !power.contains_key("power-profile") {
                return Err(AgentError::NotSupported("power-profiles-daemon is not running".into()));
            }
            if !offered.iter().any(|p| p == profile) {
                return Err(AgentError::NotSupported(format!("this computer does not offer the power profile {profile}")));
            }
            self.access.confirm(&caller, "SetPowerMode", DESKTOP, call).await?;
            self.access.make_change(&caller, "SetPowerMode", changes::Setting::PowerMode, None, until, |_| changes::Values::text(profile)).await
        }
        .await;
        caller.finish(result)
    }

    async fn star_file(&self, uri: &str, starred: bool, options: HashMap<String, OwnedValue>, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> Result<Record, AgentError> {
        let call = CallOptions::from_dict(&options);
        let caller = self.access.check_targeted(conn, &header, "StarFile", DESKTOP, call).await?;
        let result = async {
            let path = match actions::Uri::parse(uri)? {
                actions::Uri::File(path) => path,
                actions::Uri::Address { scheme, .. } => return Err(AgentError::NotSupported(format!("{scheme}: only files in the home folder can be starred"))),
            };
            // As GIO names it (`.` and `..` resolved).
            let path = gio::prelude::FileExt::path(&gio::File::for_path(&path)).unwrap_or(path);
            caller.target(actions::Uri::File(path.clone()).log_target());
            if std::fs::symlink_metadata(&path).is_err() {
                return Err(AgentError::NotFound(path.display().to_string()));
            }
            if !helix_settings::stars::can_star(&gio::glib::home_dir(), &path) {
                return Err(AgentError::NotSupported("only files in the home folder can be starred".into()));
            }
            let file_uri = helix_settings::stars::uri_for_path(&path).ok_or_else(|| AgentError::NotSupported("stars are not available".into()))?;
            caller.target(file_uri.clone());
            blocking(|| {
                let store = helix_settings::stars::Store::open(&helix_settings::stars::data_home(), true);
                match store.access() {
                    helix_settings::stars::Access::ReadWrite => Ok(()),
                    _ => Err(AgentError::NotSupported(format!("stars cannot be written: {}", store.note()))),
                }
            })
            .await?;
            self.access.confirm(&caller, "StarFile", DESKTOP, call).await?;
            self.access.make_change(&caller, "StarFile", changes::Setting::Star(file_uri), None, None, |_| changes::Values::flag(starred)).await
        }
        .await;
        caller.finish(result)
    }

    async fn set_favorite(&self, app_id: &str, pinned: bool, options: HashMap<String, OwnedValue>, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> Result<Record, AgentError> {
        let call = CallOptions::from_dict(&options);
        let caller = self.access.check_targeted(conn, &header, "SetFavorite", DESKTOP, call).await?;
        let id = app_id.strip_suffix(".desktop").unwrap_or(app_id).to_owned();
        let result = async {
            if id.is_empty() {
                return Err(AgentError::invalid_args("no app id"));
            }
            caller.check_target(Target::App(&id))?;
            let wanted = id.clone();
            let plan = blocking(move || actions::plan_launch(&wanted, Vec::new())).await?;
            for app in &plan.apps {
                caller.check_target(Target::App(app))?;
            }
            self.access.confirm(&caller, "SetFavorite", DESKTOP, call).await?;
            self.access.make_change(&caller, "SetFavorite", changes::Setting::Favorite(id.clone()), None, None, |_| changes::Values::flag(pinned)).await
        }
        .await;
        caller.finish(result)
    }

    async fn reveal_file(&self, uri: &str, options: HashMap<String, OwnedValue>, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> Result<(), AgentError> {
        let call = CallOptions::from_dict(&options);
        let caller = self.access.check_targeted(conn, &header, "RevealFile", DESKTOP, call).await?;
        let result = async {
            let path = match actions::Uri::parse(uri)? {
                actions::Uri::File(path) => path,
                actions::Uri::Address { scheme, .. } => return Err(AgentError::NotSupported(format!("{scheme}: only local files can be revealed"))),
            };
            let file_uri = actions::Uri::File(path.clone()).uri();
            caller.target(file_uri.clone());
            if std::fs::symlink_metadata(&path).is_err() {
                return Err(AgentError::NotFound(path.display().to_string()));
            }
            self.access.file_manager_check(conn, &caller).await?;
            self.access.confirm(&caller, "RevealFile", DESKTOP, call).await?;
            let token = crate::activation::token_async(None).await.unwrap_or_default();
            let body = (vec![file_uri.as_str()], token.as_str());
            let shown = conn.call_method(Some(FILE_MANAGER), "/org/freedesktop/FileManager1", Some(FILE_MANAGER), "ShowItems", &body);
            match tokio::time::timeout(Duration::from_secs(5), shown).await {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(err)) => Err(AgentError::Failed(format!("the file manager did not show it: {err}"))),
                Err(_) => Err(AgentError::Failed("the file manager did not answer in time".into())),
            }
        }
        .await;
        caller.finish(result)
    }

    async fn dismiss_notification(&self, id: u32, options: HashMap<String, OwnedValue>, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> Result<(), AgentError> {
        let call = CallOptions::from_dict(&options);
        let caller = self.access.check_targeted(conn, &header, "DismissNotification", DESKTOP, call).await?;
        caller.target(format!("notification:{id}"));
        let result = async {
            let (center, note) = self.access.visible_note(&caller, id).await?;
            self.access.confirm(&caller, "DismissNotification", DESKTOP, call).await?;
            // The user's ×: the app is told it was dismissed.
            let public = self.access.public_conn().unwrap_or_else(|| conn.clone());
            center.close(&public, note.id, crate::notify_store::CloseReason::Dismissed).await.map_err(|e| AgentError::Failed(e.to_string()))
        }
        .await;
        caller.finish(result)
    }

    async fn notification_action(&self, id: u32, action: &str, options: HashMap<String, OwnedValue>, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> Result<(), AgentError> {
        let call = CallOptions::from_dict(&options);
        let caller = self.access.check_targeted(conn, &header, "NotificationAction", NOTE_ACTION, call).await?;
        caller.target(format!("notification:{id}"));
        // Never the button's key or label.
        caller.detail("action", activity::Detail::Kind(if action == "default" { "default" } else { "button" }));
        let result = async {
            if action.is_empty() {
                return Err(AgentError::invalid_args("no action"));
            }
            let (center, note) = self.access.visible_note(&caller, id).await?;
            let info = desktop::note_info(&note, 0);
            let sensitive_label = a11y::sensitive_button;
            let (key, label, sensitive) = if action == "default" {
                if !info.default_action {
                    return Err(AgentError::NotFound(format!("notification {id} has no default action")));
                }
                // Apps often make the default action the one that answers.
                (None, String::new(), info.actions.iter().any(|(_, l)| sensitive_label(l)))
            } else {
                let Some((key, label)) = info.actions.iter().find(|(k, _)| k == action) else { return Err(AgentError::NotFound(format!("notification {id} has no action {action}"))) };
                (Some(key.clone()), label.clone(), sensitive_label(label))
            };
            let app = if info.app_id.is_empty() { info.app_name.clone() } else { info.app_id.clone() };
            let control = Control { name: talk::clean(&label).chars().take(64).collect(), role: "button", app };
            self.access.confirm_control(&caller, "NotificationAction", Need { sensitive, ..NOTE_ACTION }, call, control).await?;
            let public = self.access.public_conn().unwrap_or_else(|| conn.clone());
            center.activate(&public, note.id, key).await.map_err(|e| AgentError::Failed(e.to_string()))
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

    async fn get_clipboard(&self, options: HashMap<String, OwnedValue>, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> Result<HashMap<String, OwnedValue>, AgentError> {
        let caller = self.access.check(conn, &header, "GetClipboard", CLIPBOARD_READ, CallOptions::from_dict(&options)).await?;
        let result = async {
            let want = actions::text_option(&options, "type")?;
            let types_only = clipboard::flag(&options, "types-only")?;
            let offer = self.access.clipboard_offer(&caller).await?;
            if types_only {
                return Ok(HashMap::from([("types".to_owned(), value(offer.types))]));
            }
            let mime = clipboard::pick(&offer.types, want.as_deref())?;
            // The kind of data and its length; never the data.
            caller.detail("mime-type", activity::Detail::Kind(clipboard::log_kind(&mime)));
            let data = self.access.clipboard_read(&offer, &mime, clipboard::MAX_BYTES, clipboard::READ_TIMEOUT).await?;
            if data.is_empty() {
                let now = self.access.clipboard_request(clipboard::Request::Peek).await?;
                if now.generation != offer.generation {
                    return Err(AgentError::Failed("the clipboard changed while it was read".into()));
                }
            }
            if self.access.locked() {
                return Err(AgentError::Locked("the screen is locked".into()));
            }
            caller.detail("data-length", activity::Detail::Count(data.len() as u64));
            Ok(clipboard::record(&mime, data, &offer.types))
        }
        .await;
        caller.finish(result)
    }

    async fn set_clipboard(
        &self,
        mime_type: &str,
        data: Vec<u8>,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> Result<(), AgentError> {
        let call = CallOptions::from_dict(&options);
        let caller = self.access.check_targeted(conn, &header, "SetClipboard", CLIPBOARD_SET, call).await?;
        caller.detail("mime-type", activity::Detail::Kind(clipboard::log_kind(if mime_type.is_empty() { clipboard::TEXT } else { mime_type })));
        caller.detail("data-length", activity::Detail::Count(data.len() as u64));
        let result = async {
            let mime = clipboard::check_set(mime_type, &data)?;
            self.access.clipboard_clear(&caller).await?;
            self.access.confirm(&caller, "SetClipboard", CLIPBOARD_SET, call).await?;
            self.access.clipboard_clear(&caller).await?;
            // The compositor owns it on the agent's behalf.
            self.access.clipboard_request(|reply| clipboard::Request::Set { mime, data, reply }).await
        }
        .await;
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

    async fn screenshot(&self, options: HashMap<String, OwnedValue>, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> Result<HashMap<String, OwnedValue>, AgentError> {
        let caller = self.access.check(conn, &header, "Screenshot", SCREEN, CallOptions::from_dict(&options)).await?;
        let result = async {
            let shot = screen::ShotOptions::from_dict(&options)?;
            caller.detail("scope", activity::Detail::Kind(shot.scope()));
            caller.target(shot.scope());
            let _busy = self.access.busy(&caller.agent);
            let snapshot = self.access.screen_clear(&caller).await?;
            // The window, or the monitor, or the screen.
            let (window, area) = match (&shot.window, &shot.monitor) {
                (Some(id), _) => {
                    self.access.screen_window(&caller, &snapshot, *id).await?;
                    (*id, shot.area)
                }
                (None, Some(connector)) => {
                    caller.target(format!("monitor:{connector}"));
                    let Some(m) = snapshot.monitors.iter().find(|m| &m.connector == connector) else { return Err(AgentError::NotFound(format!("monitor {connector}"))) };
                    let (mx, my, mw, mh) = m.area;
                    let area = match shot.area {
                        None => (mx, my, mw, mh),
                        Some((x, y, w, h)) => {
                            if x < 0 || y < 0 || x + w > mw || y + h > mh {
                                return Err(AgentError::invalid_args("the area is not on the monitor"));
                            }
                            (mx + x, my + y, w, h)
                        }
                    };
                    (0, Some(area))
                }
                (None, None) => (0, shot.area),
            };
            let mut shown = self.access.shown_windows(&caller, &snapshot).await;
            if window != 0 && !shown.contains(&window) {
                // Checked just now; only a window that came since.
                shown.push(window);
            }
            let layers: Vec<String> = screen::SHOWN_LAYERS.iter().map(|l| (*l).to_owned()).collect();
            // Again just before the compositor draws it.
            self.access.screen_clear(&caller).await?;
            self.access.controlling(&caller.agent);
            let got = self.access.screen_request(|reply| screen::Request::Capture { window, area, shown, layers, reply }, Duration::from_secs(10)).await?;
            let taken = got.map_err(|e| match e {
                screen::ShotError::Locked => AgentError::Locked("the screen is locked".into()),
                screen::ShotError::NoWindow => AgentError::NotFound(format!("window {window}")),
                screen::ShotError::Invalid => AgentError::invalid_args(format!("the area is empty, not on the screen or the window, or more than {} pixels across", 16384)),
                screen::ShotError::Failed => AgentError::Failed("the compositor could not take the screenshot".into()),
            })?;
            let (rect, filled) = (taken.rect, taken.filled);
            let png = blocking(move || screen::read_png(taken.file)).await?;
            if self.access.locked() {
                return Err(AgentError::Locked("the screen is locked".into()));
            }
            let record = screen::record(png, rect, filled)?;
            // Its size and how much was filled; never the pixels.
            if let (Some(w), Some(h)) = (record.get("width"), record.get("height")) {
                caller.detail("width", activity::Detail::Count(i32::try_from(w.clone()).unwrap_or(0) as u64));
                caller.detail("height", activity::Detail::Count(i32::try_from(h.clone()).unwrap_or(0) as u64));
            }
            caller.detail("filled-count", activity::Detail::Count(u64::from(filled)));
            Ok(record)
        }
        .await;
        caller.finish(result)
    }

    async fn input(
        &self,
        events: Vec<HashMap<String, OwnedValue>>,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> Result<(), AgentError> {
        let call = CallOptions::from_dict(&options);
        let caller = self.access.check_targeted(conn, &header, "Input", SCREEN, call).await?;
        let result = async {
            let plan = screen::plan(&events)?;
            // Counts and lengths only: never keys or text.
            caller.detail("event-count", activity::Detail::Count(plan.events));
            for (key, n) in [("move-count", plan.moves), ("click-count", plan.clicks), ("button-count", plan.buttons), ("scroll-count", plan.scrolls), ("key-count", plan.keys), ("shortcut-count", plan.shortcuts), ("text-length", plan.text_length)] {
                if n > 0 {
                    caller.detail(key, activity::Detail::Count(n));
                }
            }
            let wanted = match options.get("window") {
                None => None,
                Some(v) => Some(u32::try_from(v.clone()).map_err(|_| AgentError::invalid_args("“window” must be a number (u)"))?).filter(|w| *w != 0),
            };
            let snapshot = self.access.screen_clear(&caller).await?;
            let window = match wanted {
                Some(w) => w,
                None if snapshot.focused != 0 => snapshot.focused,
                None => return Err(AgentError::NotFound("no window has the keyboard focus".into())),
            };
            let w = self.access.screen_window(&caller, &snapshot, window).await?;
            self.access.confirm(&caller, "Input", SCREEN, call).await?;
            let snapshot = self.access.screen_clear(&caller).await?;
            if snapshot.focused != window {
                // The window named: focused first, then checked again.
                self.access.window_ops(vec![actions::WindowOp::Activate(window)]).await?;
                self.access.settle(|s| s.focused == window).await;
                let snapshot = self.access.screen_clear(&caller).await?;
                if snapshot.focused != window {
                    return Err(AgentError::Failed("the window did not take the keyboard focus".into()));
                }
                self.access.screen_window(&caller, &snapshot, window).await?;
            }
            // Positions are in the window: not past its size.
            let (_, _, width, height) = w.geometry;
            for step in &plan.steps {
                if let screen::Step::Motion(x, y) = step {
                    if *x >= f64::from(width) || *y >= f64::from(height) {
                        return Err(AgentError::invalid_args(format!("a position outside the window ({width}×{height})")));
                    }
                }
            }
            let _busy = self.access.busy(&caller.agent);
            self.access.controlling(&caller.agent);
            let (done, result) = self.access.run_input(&caller, window, &plan).await;
            caller.detail("done-count", activity::Detail::Count(done));
            result
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
        // The changes go with the log (pending restores stay).
        self.access.changes().clear();
        self.access.changes_changed();
        tracing::info!("agent activity log cleared");
        Ok(())
    }

    /// Every change that can still be undone, newest first (README.md,
    /// "Who may manage grants").
    async fn list_changes(&self, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> fdo::Result<Vec<HashMap<String, OwnedValue>>> {
        self.access.require_settings(conn, &header, "ListChanges").await?;
        Ok(self.access.list_changes().await)
    }

    /// Put change `id`'s old value back, whatever the agents' access.
    async fn undo(&self, id: &str, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> Result<(), changes::UndoError> {
        let exe = self.access.require_settings(conn, &header, "Undo").await.map_err(changes::UndoError::Denied)?;
        let (change, result) = self.access.undo(id).await;
        let mut detail = vec![("change", activity::Detail::Text(id.to_owned()))];
        let mut target = String::new();
        if let Some(c) = &change {
            target = c.setting().map(|s| s.log_target()).unwrap_or_default();
            detail.extend(changes::log_values(&c.setting, &c.new, &c.old));
        }
        let error = result.as_ref().err().map(|skip| AgentError::from(skip.clone()).kind().to_owned());
        let entry = activity::Entry { time: now(), agent: exe.to_string_lossy().into_owned(), method: "Undo".to_owned(), target, error, detail, ..activity::Entry::default() };
        self.access.inner.log.record(entry);
        result.map_err(|skip| changes::UndoError::Agent(skip.into()))
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

    /// What `ListChanges` gives may have changed.
    #[zbus(signal)]
    pub async fn changes_changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}
