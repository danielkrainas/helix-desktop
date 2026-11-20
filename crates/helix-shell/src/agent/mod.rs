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
//! and what document a window shows ([`document`]).

pub mod activity;
pub mod desktop;
pub mod document;
pub mod files;
pub mod grants;
pub mod identity;
pub mod policy;
pub mod prompt;

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
}

impl CallOptions {
    pub fn from_dict(options: &HashMap<String, OwnedValue>) -> CallOptions {
        let flag = |key: &str, default: bool| options.get(key).and_then(|v| bool::try_from(v.clone()).ok()).unwrap_or(default);
        CallOptions { interactive: flag("interactive", true), sensitive: flag("sensitive", false) }
    }
}

impl Default for CallOptions {
    fn default() -> Self {
        CallOptions { interactive: true, sensitive: false }
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
}

/// Where the *Read only* methods read the desktop's state.
#[derive(Clone)]
pub struct Desktop {
    /// The UI, which answers [`UiEvent::AgentSnapshot`](crate::UiEvent::AgentSnapshot).
    pub ui: tokio::sync::mpsc::UnboundedSender<crate::UiEvent>,
    pub notifications: Option<crate::notifications::Center>,
}

/// How often waits look at the windows again.
const WAIT_POLL: Duration = Duration::from_millis(200);

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
        if policy::sensitive_step(&config, need, options.sensitive, options.interactive)? {
            let _turn = self.inner.prompt.lock().await;
            let answer = (self.inner.confirm)(ConfirmRequest { agent: agent.clone(), method }).await;
            if let Some(yes) = answer {
                let outcome = if yes { activity::Outcome::Allowed } else { activity::Outcome::Refused("DeniedByUser".into()) };
                self.inner.log.record(activity::Entry { target: method.to_owned(), ..Self::entry(&agent, activity::SENSITIVE_PROMPT, need.level, outcome) });
            }
            match answer {
                Some(true) => {}
                Some(false) => return Err(AgentError::DeniedByUser("the user declined this action".into())),
                None => return Err(AgentError::NeedsConfirmation("the user could not be asked to confirm".into())),
            }
            config = Config::read();
            self.gate(&agent, &config, need).await?;
        }
        self.grants().touch(&agent, now());
        self.publish();
        Ok((agent, config))
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
        let _ = self.inner.desktop.set(desktop);
    }

    /// The windows, workspaces and monitors as the UI knows them now.
    pub async fn snapshot(&self) -> Result<desktop::Snapshot, AgentError> {
        let unavailable = || AgentError::Failed("the desktop's state is not available".into());
        let ui = self.inner.desktop.get().ok_or_else(unavailable)?.ui.clone();
        let (reply, answer) = tokio::sync::oneshot::channel();
        ui.send(crate::UiEvent::AgentSnapshot(desktop::SnapshotSlot::new(reply))).map_err(|_| unavailable())?;
        tokio::time::timeout(Duration::from_secs(5), answer).await.map_err(|_| unavailable())?.map_err(|_| unavailable())
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

    #[zbus(signal)]
    pub async fn access_changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
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
        self.access.require_settings(conn, &header, "Revoke").await?;
        self.access.revoke(agent);
        Ok(())
    }

    async fn forget(&self, agent: &str, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> fdo::Result<()> {
        self.access.require_settings(conn, &header, "Forget").await?;
        self.access.forget(agent);
        Ok(())
    }

    async fn stop_all(&self, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> fdo::Result<()> {
        self.access.require_settings(conn, &header, "StopAll").await?;
        self.access.stop_all();
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

    #[zbus(signal)]
    pub async fn agents_changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}
