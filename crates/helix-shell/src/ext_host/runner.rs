//! Running extensions (helix-desktop-spec `contracts/extensions/README.md`):
//! starting each in its own process with a private D-Bus connection,
//! serving it `org.helix.Extension1.Host`, following its items, and
//! restarting or stopping it.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::Connection;

use super::manifest::{self, Activation, Manifest, SettingValue, Zone};
use super::store::{self, Found, Kind, Log};
use crate::UiEvent;

pub const HOST_PATH: &str = "/org/helix/Extension1/Host";
const IFACE_PANEL: &str = "org.helix.Extension1.PanelItem";
const IFACE_TOGGLE: &str = "org.helix.Extension1.Toggle";
const IFACE_SLIDER: &str = "org.helix.Extension1.Slider";
const IFACE_MENU: &str = "org.helix.Extension1.Menu";
const IFACE_DOCK: &str = "org.helix.Extension1.DockItem";
const IFACE_COMMANDS: &str = "org.helix.Extension1.Commands";
const COMMANDS_PATH: &str = "/org/helix/Extension/commands";
/// Calls to items wait this long.
const CALL_TIMEOUT: Duration = Duration::from_millis(250);
/// From start to `Hello`.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const RESTART_DELAYS: [u64; 3] = [1, 5, 30];
const CRASH_WINDOW: Duration = Duration::from_secs(600);
const CRASH_LIMIT: usize = 5;
/// Content limits.
const PANEL_LABEL_MAX: usize = 64;
const LABEL_MAX: usize = 128;
const TOOLTIP_MAX: usize = 512;
const TOOLTIP_LINES: usize = 8;
const GRAPH_MAX: usize = 64;
const MENU_LEVEL_MAX: usize = 64;
const MENU_MAX: usize = 256;

fn cut(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Disabled,
    Invalid(String),
    NeedsApproval,
    Starting,
    Running,
    Stopped,
    Crashed(String),
}

impl State {
    pub fn name(&self) -> &'static str {
        match self {
            State::Disabled => "disabled",
            State::Invalid(_) => "invalid",
            State::NeedsApproval => "needs-approval",
            State::Starting => "starting",
            State::Running => "running",
            State::Stopped => "stopped",
            State::Crashed(_) => "crashed",
        }
    }
}

/// A panel item as the panel draws it.
#[derive(Debug, Clone, PartialEq)]
pub struct PanelItemView {
    /// `<extension id>/<item id>`.
    pub full_id: String,
    pub name: String,
    pub zones: Vec<Zone>,
    pub dir: PathBuf,
    pub icon: String,
    pub icon_data: Vec<u8>,
    pub label: String,
    pub tooltip: String,
    pub accessible_name: String,
    pub visible: bool,
    pub attention: bool,
    pub meter: f64,
    pub graph: Vec<f64>,
    pub has_menu: bool,
    /// The extension is running (else the item is drawn dimmed, from its
    /// manifest).
    pub running: bool,
    /// A development extension's: shown without being in the layout.
    pub development: bool,
    /// (1.1) A click toggles the extension's popover.
    pub popover: bool,
}

/// (1.1) A dock item as the dock draws it.
#[derive(Debug, Clone, PartialEq)]
pub struct DockItemView {
    pub full_id: String,
    pub name: String,
    pub dir: PathBuf,
    pub icon: String,
    pub icon_data: Vec<u8>,
    pub label: String,
    /// The running dot.
    pub dot: bool,
    pub count: i64,
    pub progress: f64,
    pub urgent: bool,
    pub has_menu: bool,
    pub accepts_drops: bool,
    pub running: bool,
}

/// (1.1) A command, for keys and the run dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandView {
    pub full_id: String,
    pub title: String,
    pub accelerator: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToggleView {
    pub full_id: String,
    pub dir: PathBuf,
    pub title: String,
    pub subtitle: String,
    pub icon: String,
    pub active: bool,
    pub sensitive: bool,
    pub has_details: bool,
    pub running: bool,
    pub development: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SliderView {
    pub full_id: String,
    pub dir: PathBuf,
    pub title: String,
    pub icon: String,
    pub value: f64,
    pub sensitive: bool,
    pub running: bool,
    pub development: bool,
}

/// What the UI shows of the extensions.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Snapshot {
    pub panel: Vec<PanelItemView>,
    pub toggles: Vec<ToggleView>,
    pub sliders: Vec<SliderView>,
    pub dock: Vec<DockItemView>,
    pub commands: Vec<CommandView>,
}

/// A menu entry from `org.helix.Extension1.Menu`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuEntry {
    pub id: i32,
    pub parent: i32,
    pub kind: String,
    pub label: String,
    pub enabled: bool,
    pub checked: bool,
}

/// An extension as the Extensions dialog lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct Info {
    pub id: String,
    pub name: String,
    pub description: String,
    pub version: String,
    pub path: PathBuf,
    pub kind: Kind,
    pub enabled: bool,
    pub approved: bool,
    pub state: State,
    pub permissions: Vec<String>,
    /// The reason it is invalid or crashed, else its last log line.
    pub error: String,
    pub settings: Vec<(manifest::SettingDecl, SettingValue)>,
    /// (1.1) Its commands: id, title, accelerator.
    pub commands: Vec<(String, String, String)>,
}

/// What the user did to an item.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Primary { x: i32, y: i32 },
    Secondary { x: i32, y: i32 },
    Middle { x: i32, y: i32 },
    Scroll { steps: i32, horizontal: bool },
    /// Flip a toggle.
    Toggle,
    /// A toggle's details menu, at `x`.
    Details { x: i32 },
    SetValue(f64),
    /// Entry `id` of the menu at `menu` was chosen.
    MenuChosen { menu: String, id: i32 },
    /// (1.1) A popover item's primary click: its popover at `x`, `y` on
    /// monitor `output`.
    Popover { x: i32, y: i32, output: String },
    /// (1.1) A dock item: clicked; its menu at `x`; files dropped.
    DockActivate,
    DockMenu { x: i32, y: i32 },
    DockDrop(Vec<String>),
}

/// A running process.
struct Run {
    pid: i32,
    conn: Option<Connection>,
    said_hello: bool,
    granted: Vec<String>,
    /// Inhibitor handles → `org.gnome.SessionManager` cookies.
    inhibitors: HashMap<u32, u32>,
    next_handle: u32,
    stopping: bool,
    /// Calls that timed out in a row.
    timeouts: u32,
}

struct Ext {
    found: Found,
    state: State,
    log: Log,
    run: Option<Run>,
    /// Bumped on every start and stop: late events of older runs are
    /// ignored.
    generation: u64,
    crashes: Vec<Instant>,
    settings: BTreeMap<String, SettingValue>,
    /// Item properties by item path.
    props: HashMap<String, HashMap<String, OwnedValue>>,
    dev_stamp: Option<std::time::SystemTime>,
    /// A start is under way (its process not yet recorded).
    in_flight: bool,
    /// (1.1) Commands to run once it has said hello.
    pending: Vec<String>,
    /// The run whose crash was last counted (each counts once).
    counted: u64,
}

impl Ext {
    fn manifest(&self) -> Option<&Manifest> {
        self.found.manifest.as_ref().ok()
    }
}

struct Inner {
    exts: BTreeMap<String, Ext>,
    keys: store::Keys,
    session: Option<Connection>,
    center: Option<crate::notifications::Center>,
    quick_opened: bool,
    flush_pending: bool,
    /// (1.1) Session bus names that have an owner (for `onDBusName`).
    names: std::collections::HashSet<String>,
}

/// The extensions: shared by the shell's services and the UI bridge.
#[derive(Clone)]
pub struct ExtHost {
    inner: Arc<Mutex<Inner>>,
    ui: mpsc::UnboundedSender<UiEvent>,
}

fn systemd_user() -> bool {
    std::env::var_os("XDG_RUNTIME_DIR").is_some_and(|d| Path::new(&d).join("systemd/private").exists())
}

/// An item's object path: `-` in its id becomes `_` (D-Bus paths have
/// no `-`).
fn item_path(kind: &str, id: &str) -> String {
    format!("/org/helix/Extension/{kind}/{}", id.replace('-', "_"))
}

impl ExtHost {
    pub fn new(ui: mpsc::UnboundedSender<UiEvent>) -> Self {
        let inner = Inner { exts: BTreeMap::new(), keys: store::Keys::default(), session: None, center: None, quick_opened: false, flush_pending: false, names: Default::default() };
        Self { inner: Arc::new(Mutex::new(inner)), ui }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Find the extensions, start those that should run, and follow the
    /// settings for the rest of the session.
    pub async fn run(self, session: Connection, center: crate::notifications::Center) {
        {
            let mut inner = self.lock();
            inner.session = Some(session);
            inner.center = Some(center);
        }
        self.rescan();
        // `onDBusName` (1.1): follow which names have owners.
        let session = self.lock().session.clone();
        if let Some(session) = session {
            let host = self.clone();
            tokio::spawn(async move { host.follow_names(session).await });
        }
        let watched = helix_settings::GSettings::new(&[store::SCHEMA, crate::panel_layout::SCHEMA]);
        let (mut changes, _watch) = watched.watch();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                change = changes.recv() => {
                    if change.is_none() {
                        break;
                    }
                    self.reconcile();
                }
                _ = tick.tick() => self.check_development(),
            }
        }
    }

    async fn follow_names(&self, session: Connection) {
        let Ok(dbus) = zbus::fdo::DBusProxy::new(&session).await else { return };
        let Ok(mut changes) = dbus.receive_name_owner_changed().await else { return };
        if let Ok(names) = dbus.list_names().await {
            self.lock().names = names.into_iter().map(|n| n.to_string()).filter(|n| !n.starts_with(':')).collect();
        }
        self.reconcile();
        while let Some(change) = changes.next().await {
            let Ok(args) = change.args() else { continue };
            let name = args.name().to_string();
            if name.starts_with(':') {
                continue;
            }
            let appeared = args.new_owner().is_some();
            let wanted = {
                let mut inner = self.lock();
                if appeared {
                    inner.names.insert(name.clone());
                } else {
                    inner.names.remove(&name);
                }
                appeared && inner.exts.values().any(|e| e.manifest().is_some_and(|m| m.activation.contains(&Activation::OnDBusName(name.clone()))))
            };
            if wanted {
                self.reconcile();
            }
        }
    }

    /// Read the extension directories again.
    pub fn rescan(&self) {
        let found = store::discover(&store::user_dir(), &store::system_dirs());
        {
            let mut inner = self.lock();
            inner.keys = store::Keys::read();
            // Keep running ones; add new ones; forget removed ones (they
            // are stopped first by reconcile).
            for (id, f) in found {
                match inner.exts.get_mut(&id) {
                    Some(ext) if ext.found.kind != Kind::Development => ext.found = f,
                    Some(_) => {}
                    None => {
                        let settings = f.manifest.as_ref().map(|m| store::read_settings(m, &store::settings_path(&m.id))).unwrap_or_default();
                        inner.exts.insert(id.clone(), Ext { found: f, state: State::Stopped, log: Log::for_id(&id), run: None, generation: 0, crashes: Vec::new(), settings, props: HashMap::new(), dev_stamp: None, in_flight: false, counted: 0, pending: Vec::new() });
                    }
                }
            }
        }
        self.reconcile();
    }

    /// Start what should run, stop what should not, place new items.
    pub fn reconcile(&self) {
        let layout = crate::panel_layout::read();
        let mut to_start = Vec::new();
        let mut to_stop = Vec::new();
        let mut placements = Vec::new();
        {
            let mut inner = self.lock();
            inner.keys = store::Keys::read();
            let keys = inner.keys.clone();
            let quick_opened = inner.quick_opened;
            let names = inner.names.clone();
            for (id, ext) in inner.exts.iter_mut() {
                let manifest = match &ext.found.manifest {
                    Ok(m) => m.clone(),
                    Err(reason) => {
                        ext.state = State::Invalid(reason.clone());
                        continue;
                    }
                };
                let enabled = keys.is_enabled(&ext.found);
                let approved = keys.is_approved(&ext.found);
                if enabled && approved && ext.found.kind != Kind::Development {
                    placements.push(manifest.clone());
                }
                let visible = manifest.panel_items.iter().any(|p| {
                    let full = format!("{id}/{}", p.id);
                    layout.left.contains(&full) || layout.center.contains(&full) || layout.right.contains(&full)
                }) || (quick_opened && manifest.toggles.iter().chain(&manifest.sliders).any(|q| layout.quick.contains(&format!("{id}/{}", q.id))));
                let named = manifest.activation.iter().any(|a| matches!(a, Activation::OnDBusName(n) if names.contains(n)));
                let wanted = manifest.activation.contains(&Activation::OnStartup) || named || !ext.pending.is_empty() || (manifest.activation.contains(&Activation::OnVisible) && (visible || ext.found.kind == Kind::Development));
                let running = ext.run.is_some();
                if !enabled {
                    if running {
                        to_stop.push(id.clone());
                    }
                    ext.state = State::Disabled;
                } else if !approved {
                    if running {
                        to_stop.push(id.clone());
                    }
                    ext.state = State::NeedsApproval;
                } else if wanted && !running && !matches!(ext.state, State::Crashed(_) | State::Starting) {
                    // (A start in progress, or a restart waiting, is left
                    // to finish.)
                    to_start.push(id.clone());
                } else if !running && !matches!(ext.state, State::Crashed(_) | State::Starting) {
                    ext.state = State::Stopped;
                }
            }
        }
        for m in placements {
            self.place(&m);
        }
        for id in to_stop {
            self.stop(&id, State::Disabled);
        }
        for id in to_start {
            self.start(&id);
        }
        self.publish();
        self.publish_list();
    }

    /// Add `m`'s items to the layout the first time it is enabled.
    fn place(&self, m: &Manifest) {
        self.place_commands(m);
        let placed = self.lock().keys.placed.clone();
        let (panel, quick, newly) = store::first_placement(m, &placed);
        if newly.is_empty() {
            return;
        }
        let mut layout = crate::panel_layout::read();
        for (zone, id) in panel {
            let list = match zone {
                0 => &mut layout.left,
                1 => &mut layout.center,
                _ => &mut layout.right,
            };
            if !list.contains(&id) {
                // The right zone keeps System Status and Power at its end.
                let at = if zone == 2 { list.iter().position(|i| i == "helix:status" || i == "helix:power").unwrap_or(list.len()) } else { list.len() };
                list.insert(at, id);
            }
        }
        for id in quick {
            if !layout.quick.contains(&id) {
                layout.quick.push(id);
            }
        }
        crate::panel_layout::write(&layout);
        let mut inner = self.lock();
        inner.keys.placed.extend(newly);
        inner.keys.write();
        tracing::info!(id = m.id, "extension's items placed");
    }

    /// (1.1) Give `m`'s commands their suggested keys, once, if free.
    fn place_commands(&self, m: &Manifest) {
        let fresh: Vec<&manifest::CommandDecl> = {
            let inner = self.lock();
            m.commands.iter().filter(|c| !inner.keys.placed.contains(&format!("{}/{}", m.id, c.id))).collect()
        };
        if fresh.is_empty() {
            return;
        }
        let mut taken = crate::shell_keys::taken_accelerators();
        taken.extend(self.lock().keys.command_keys.values().filter(|a| !a.is_empty()).cloned());
        let mut refused = Vec::new();
        {
            let mut inner = self.lock();
            for c in fresh {
                let full = format!("{}/{}", m.id, c.id);
                inner.keys.placed.push(full.clone());
                if c.keybinding.is_empty() {
                    continue;
                }
                let valid = crate::accel::Accelerator::parse(&c.keybinding).and_then(|a| a.to_wire()).is_some();
                if valid && !taken.iter().any(|t| crate::shell_keys::same_keys(t, &c.keybinding)) {
                    inner.keys.command_keys.insert(full, c.keybinding.clone());
                    taken.push(c.keybinding.clone());
                } else {
                    refused.push(format!("{} ({})", c.title, c.keybinding));
                }
            }
            inner.keys.write();
        }
        if !refused.is_empty() {
            let center = self.lock().center.clone();
            if let Some(center) = center {
                center.notice("Extensions", &format!("{}: keys left unset", m.name), &format!("Already in use: {}. Set them in Extensions…", refused.join(", ")), "input-keyboard-symbolic", false);
            }
        }
    }

    /// Run command `full_id` (`<extension id>/<command id>`), starting its
    /// extension when it is not running.
    pub async fn run_command(&self, full_id: &str) -> Result<(), String> {
        let (id, command) = full_id.split_once('/').ok_or("not a command")?;
        if self.lock().center.as_ref().is_some_and(|c| c.locked()) {
            return Err("the session is locked".into());
        }
        let (target, queue) = {
            let mut inner = self.lock();
            let keys = inner.keys.clone();
            let ext = inner.exts.get_mut(id).ok_or_else(|| format!("no extension {id}"))?;
            let m = ext.manifest().ok_or("the extension is invalid")?;
            if !m.commands.iter().any(|c| c.id == command) {
                return Err(format!("{id} has no command {command}"));
            }
            if ext.found.kind != Kind::Development && !(keys.is_enabled(&ext.found) && keys.is_approved(&ext.found)) {
                return Err(format!("{id} is not turned on"));
            }
            match ext.run.as_ref().filter(|r| r.said_hello).and_then(|r| r.conn.clone()) {
                Some(conn) => (Some((conn, ext.generation)), false),
                None => {
                    ext.pending.push(command.to_owned());
                    (None, true)
                }
            }
        };
        if let Some((conn, generation)) = target {
            return self.call(Target { conn: &conn, id, generation }, COMMANDS_PATH, IFACE_COMMANDS, "Run", &(command,)).await;
        }
        if queue {
            self.reconcile();
        }
        Ok(())
    }

    /// Set command `full_id`'s key (`""`: none).
    pub fn bind_command(&self, full_id: &str, accelerator: &str) -> Result<(), String> {
        let (id, command) = full_id.split_once('/').ok_or("not a command")?;
        let known = self.lock().exts.get(id).and_then(|e| e.manifest()).is_some_and(|m| m.commands.iter().any(|c| c.id == command));
        if !known {
            return Err(format!("no command {full_id}"));
        }
        if !accelerator.is_empty() {
            if crate::accel::Accelerator::parse(accelerator).and_then(|a| a.to_wire()).is_none() {
                return Err(format!("{accelerator:?} is not a key combination"));
            }
            let mut taken = crate::shell_keys::taken_accelerators();
            taken.extend(self.lock().keys.command_keys.iter().filter(|(c, a)| *c != full_id && !a.is_empty()).map(|(_, a)| a.clone()));
            if taken.iter().any(|t| crate::shell_keys::same_keys(t, accelerator)) {
                return Err(format!("{accelerator} is already in use"));
            }
        }
        let mut keys = store::Keys::read();
        keys.command_keys.insert(full_id.to_owned(), accelerator.to_owned());
        keys.write();
        self.lock().keys = keys;
        self.publish();
        self.publish_list();
        Ok(())
    }

    /// Start extension `id`'s process.
    fn start(&self, id: &str) {
        let (dir, exec, generation) = {
            let mut inner = self.lock();
            let Some(ext) = inner.exts.get_mut(id) else { return };
            // Already running, or starting from another path.
            if ext.run.is_some() || ext.in_flight {
                return;
            }
            let Some(m) = ext.manifest() else { return };
            let exec = m.exec.clone();
            ext.in_flight = true;
            ext.generation += 1;
            ext.state = State::Starting;
            ext.props.clear();
            (ext.found.dir.clone(), exec, ext.generation)
        };
        let host = self.clone();
        let id = id.to_owned();
        tokio::spawn(async move {
            if let Err(err) = host.spawn(&id, &dir, &exec, generation).await {
                if let Some(ext) = host.lock().exts.get_mut(&id).filter(|e| e.generation == generation) {
                    ext.in_flight = false;
                }
                host.log(&id, format!("cannot start: {err}"));
                host.crashed(&id, generation);
            }
        });
    }

    async fn spawn(&self, id: &str, dir: &Path, exec: &[String], generation: u64) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;
        let program = if Path::new(&exec[0]).is_absolute() { PathBuf::from(&exec[0]) } else { dir.join(&exec[0]) };
        let (ours, theirs) = std::os::unix::net::UnixStream::pair()?;
        let mut cmd = if systemd_user() {
            let mut c = tokio::process::Command::new("systemd-run");
            let unit = format!("app-helix\\x2dextension-{}-{}-{generation}.scope", id.replace('.', "_"), std::process::id());
            c.args(["--user", "--scope", "--quiet", "--collect", "-p", "MemoryMax=256M", "-p", "CPUWeight=50"]).arg(format!("--unit={unit}")).arg("--").arg(&program).args(&exec[1..]);
            c
        } else {
            let mut c = tokio::process::Command::new(&program);
            c.args(&exec[1..]);
            c
        };
        cmd.current_dir(dir)
            .env("HELIX_EXTENSION_ID", id)
            .env("HELIX_EXTENSION_FD", "3")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let fd = theirs.as_raw_fd();
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                if fd != 3 && libc::dup2(fd, 3) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let flags = libc::fcntl(3, libc::F_GETFD);
                if flags < 0 || libc::fcntl(3, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = cmd.spawn()?;
        let started = tokio::time::Instant::now();
        drop(theirs);
        let pid = child.id().map_or(0, |p| p as i32);
        self.log(id, format!("started (pid {pid})"));
        // Its output goes to its log.
        for stream in [child.stdout.take().map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>), child.stderr.take().map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>)].into_iter().flatten() {
            let host = self.clone();
            let id = id.to_owned();
            tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(stream).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    host.log(&id, line);
                }
            });
        }
        let current = {
            let mut inner = self.lock();
            if let Some(ext) = inner.exts.get_mut(id).filter(|e| e.generation == generation) {
                ext.in_flight = false;
            }
            match inner.exts.get_mut(id).filter(|e| e.generation == generation && e.state == State::Starting) {
                Some(ext) => {
                    ext.run = Some(Run { pid, conn: None, said_hello: false, granted: Vec::new(), inhibitors: HashMap::new(), next_handle: 1, stopping: false, timeouts: 0 });
                    true
                }
                None => false,
            }
        };
        if !current {
            // Stopped or started again meanwhile: this one is not wanted.
            let _ = child.start_kill();
            return Ok(());
        }
        // Waiting for it to end.
        let host = self.clone();
        let waited = id.to_owned();
        tokio::spawn(async move {
            let status = child.wait().await;
            host.exited(&waited, generation, status.map(|s| s.to_string()).unwrap_or_else(|e| e.to_string()));
        });
        // The connection, and Hello in time.
        ours.set_nonblocking(true)?;
        let stream = tokio::net::UnixStream::from_std(ours)?;
        let host_iface = HostIface { host: self.clone(), id: id.to_owned(), generation };
        let build = async {
            zbus::connection::Builder::unix_stream(stream).server(zbus::Guid::generate())?.p2p().serve_at(HOST_PATH, host_iface)?.build().await
        };
        let conn = match tokio::time::timeout_at(started + HELLO_TIMEOUT, build).await {
            Ok(Ok(conn)) => conn,
            // The process exists: end it; its exit counts as the crash.
            Ok(Err(err)) => {
                let alive = self.lock().exts.get(id).filter(|e| e.generation == generation).is_some_and(|e| e.run.is_some());
                if alive {
                    self.log(id, format!("no D-Bus connection: {err}"));
                    self.kill(id, generation);
                }
                return Ok(());
            }
            Err(_) => {
                self.log(id, "no Hello within 10 seconds".into());
                self.kill(id, generation);
                return Ok(());
            }
        };
        {
            let mut inner = self.lock();
            let Some(run) = inner.exts.get_mut(id).filter(|e| e.generation == generation).and_then(|e| e.run.as_mut()) else { return Ok(()) };
            run.conn = Some(conn);
        }
        // Hello within 10 seconds of starting.
        tokio::time::sleep_until(started + HELLO_TIMEOUT).await;
        let late = {
            let inner = self.lock();
            inner.exts.get(id).filter(|e| e.generation == generation).and_then(|e| e.run.as_ref()).is_some_and(|r| !r.said_hello && !r.stopping)
        };
        if late {
            self.log(id, "no Hello within 10 seconds".into());
            self.kill(id, generation);
        }
        Ok(())
    }

    /// Run `found` over `stream` without a process (tests): as if its
    /// program had connected.
    #[doc(hidden)]
    pub async fn attach(&self, found: Found, stream: std::os::unix::net::UnixStream) -> zbus::Result<()> {
        let id = found.id.clone();
        let settings = found.manifest.as_ref().map(|m| store::read_settings(m, &store::settings_path(&m.id))).unwrap_or_default();
        let generation = 1;
        {
            let mut inner = self.lock();
            let run = Run { pid: 0, conn: None, said_hello: false, granted: Vec::new(), inhibitors: HashMap::new(), next_handle: 1, stopping: false, timeouts: 0 };
            inner.exts.insert(id.clone(), Ext { found, state: State::Starting, log: Log::default(), run: Some(run), generation, crashes: Vec::new(), settings, props: HashMap::new(), dev_stamp: None, in_flight: false, counted: 0, pending: Vec::new() });
        }
        stream.set_nonblocking(true)?;
        let stream = tokio::net::UnixStream::from_std(stream)?;
        let host_iface = HostIface { host: self.clone(), id: id.clone(), generation };
        let conn = zbus::connection::Builder::unix_stream(stream).server(zbus::Guid::generate())?.p2p().serve_at(HOST_PATH, host_iface)?.build().await?;
        if let Some(run) = self.lock().exts.get_mut(&id).and_then(|e| e.run.as_mut()) {
            run.conn = Some(conn);
        }
        Ok(())
    }

    /// The shell's side of a running extension's connection (tests).
    #[doc(hidden)]
    pub fn connection_of(&self, id: &str) -> Option<Connection> {
        self.lock().exts.get(id).and_then(|e| e.run.as_ref()).and_then(|r| r.conn.clone())
    }

    fn kill(&self, id: &str, generation: u64) {
        let pid = self.lock().exts.get(id).filter(|e| e.generation == generation).and_then(|e| e.run.as_ref()).map(|r| r.pid);
        if let Some(pid) = pid.filter(|p| *p > 0) {
            // SAFETY: signalling our own child.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }

    /// Its process ended.
    fn exited(&self, id: &str, generation: u64, status: String) {
        let expected = {
            let inner = self.lock();
            let Some(ext) = inner.exts.get(id).filter(|e| e.generation == generation) else { return };
            ext.run.as_ref().is_some_and(|r| r.stopping)
        };
        self.log(id, format!("exited: {status}"));
        if expected {
            self.end_run(id);
        } else {
            self.crashed(id, generation);
        }
    }

    /// Release what a run held.
    fn end_run(&self, id: &str) {
        let (run, session) = {
            let mut inner = self.lock();
            let session = inner.session.clone();
            let Some(ext) = inner.exts.get_mut(id) else { return };
            ext.props.clear();
            (ext.run.take(), session)
        };
        if let (Some(run), Some(session)) = (run, session) {
            for cookie in run.inhibitors.into_values() {
                let session = session.clone();
                tokio::spawn(async move { uninhibit(&session, cookie).await });
            }
        }
        self.publish();
        self.publish_list();
    }

    /// It crashed: restart it later, or give up for the session.
    fn crashed(&self, id: &str, generation: u64) {
        let (give_up, delay, name, last) = {
            let mut inner = self.lock();
            let Some(ext) = inner.exts.get_mut(id).filter(|e| e.generation == generation && e.counted != generation) else { return };
            ext.counted = generation;
            let now = Instant::now();
            ext.crashes.retain(|t| now.duration_since(*t) < CRASH_WINDOW);
            ext.crashes.push(now);
            let n = ext.crashes.len();
            // Its own last words, not the host's notes about it.
            let ours = ["started (", "exited: ", "stopping", "said hello"];
            let last = ext.log.lines.iter().rev().find(|l| !ours.iter().any(|o| l.starts_with(o))).or(ext.log.lines.back()).cloned().unwrap_or_default();
            let name = ext.manifest().map_or_else(|| id.to_owned(), |m| m.name.clone());
            if n >= CRASH_LIMIT {
                ext.state = State::Crashed(last.clone());
                (true, 0, name, last)
            } else {
                ext.state = State::Starting;
                (false, RESTART_DELAYS[(n - 1).min(RESTART_DELAYS.len() - 1)], name, last)
            }
        };
        self.end_run(id);
        if give_up {
            tracing::warn!(id, "extension crashed too often; not restarted");
            let center = self.lock().center.clone();
            if let Some(center) = center {
                center.notice("Extensions", &format!("{name} stopped working"), &last, "dialog-warning-symbolic", false);
            }
            self.publish_list();
            return;
        }
        tracing::info!(id, delay, "extension crashed; restarting later");
        let host = self.clone();
        let id = id.to_owned();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(delay)).await;
            let still = host.lock().exts.get(&id).is_some_and(|e| e.generation == generation && e.run.is_none() && e.state == State::Starting);
            if still {
                host.reconcile_one(&id);
            }
        });
    }

    fn reconcile_one(&self, id: &str) {
        let wants = {
            let inner = self.lock();
            inner.exts.get(id).is_some_and(|e| inner.keys.is_enabled(&e.found) && inner.keys.is_approved(&e.found) && e.found.manifest.is_ok())
        };
        if wants {
            self.start(id);
        }
    }

    /// Stop extension `id`, ending in `state`.
    fn stop(&self, id: &str, state: State) {
        let (pid, conn, generation) = {
            let mut inner = self.lock();
            let Some(ext) = inner.exts.get_mut(id) else { return };
            ext.state = state;
            let generation = ext.generation;
            let Some(run) = ext.run.as_mut() else { return };
            if run.stopping {
                return;
            }
            run.stopping = true;
            (run.pid, run.conn.take(), generation)
        };
        self.log(id, "stopping".into());
        let host = self.clone();
        let id = id.to_owned();
        tokio::spawn(async move {
            if let Some(conn) = conn {
                let _ = conn.close().await;
            }
            if pid > 0 {
                // SAFETY: signalling our own child.
                unsafe { libc::kill(pid, libc::SIGTERM) };
                tokio::time::sleep(Duration::from_secs(2)).await;
                let alive = host.lock().exts.get(&id).is_some_and(|e| e.generation == generation && e.run.is_some());
                if alive {
                    // SAFETY: as above.
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                }
            }
        });
        self.publish();
    }

    /// Stop everything (the session ends).
    pub fn stop_all(&self) {
        let ids: Vec<String> = self.lock().exts.iter().filter(|(_, e)| e.run.is_some()).map(|(id, _)| id.clone()).collect();
        for id in ids {
            self.stop(&id, State::Stopped);
        }
    }

    pub fn log(&self, id: &str, line: String) {
        tracing::info!(extension = id, "{line}");
        {
            let mut inner = self.lock();
            if let Some(ext) = inner.exts.get_mut(id) {
                ext.log.push(line);
            }
        }
    }

    /// `Hello` came: follow its items.
    fn said_hello(&self, id: &str, generation: u64) -> Option<Vec<String>> {
        let id_owned = id.to_owned();
        let (granted, conn, paths) = {
            let mut inner = self.lock();
            let keys = inner.keys.clone();
            let ext = inner.exts.get_mut(id).filter(|e| e.generation == generation)?;
            let m = ext.manifest()?.clone();
            let granted: Vec<String> = if ext.found.kind == Kind::Development {
                m.permissions.clone()
            } else {
                m.permissions.iter().filter(|p| keys.approved.get(id).is_some_and(|a| a.contains(p))).cloned().collect()
            };
            ext.state = State::Running;
            let run = ext.run.as_mut()?;
            run.said_hello = true;
            run.granted = granted.clone();
            let paths: Vec<(String, &'static str)> = m
                .panel_items
                .iter()
                .map(|p| (item_path("panel_item", &p.id), IFACE_PANEL))
                .chain(m.toggles.iter().map(|t| (item_path("toggle", &t.id), IFACE_TOGGLE)))
                .chain(m.sliders.iter().map(|s| (item_path("slider", &s.id), IFACE_SLIDER)))
                .chain(m.dock_items.iter().map(|d| (item_path("dock_item", &d.id), IFACE_DOCK)))
                .collect();
            (granted, run.conn.clone(), paths)
        };
        if let Some(conn) = conn {
            let host = self.clone();
            let id = id.to_owned();
            let follow_conn = conn.clone();
            tokio::spawn(async move { host.follow(follow_conn, id, generation, paths).await });
            // Commands that started it.
            let pending = self.lock().exts.get_mut(&id_owned).map(|e| std::mem::take(&mut e.pending)).unwrap_or_default();
            for command in pending {
                let host = self.clone();
                let conn = conn.clone();
                let id = id_owned.clone();
                tokio::spawn(async move {
                    let _ = host.call(Target { conn: &conn, id: &id, generation }, COMMANDS_PATH, IFACE_COMMANDS, "Run", &(command.as_str(),)).await;
                });
            }
        }
        self.publish_list();
        Some(granted)
    }

    /// Read the items' properties, then follow their changes.
    async fn follow(&self, conn: Connection, id: String, generation: u64, paths: Vec<(String, &'static str)>) {
        // Listen first, so no change is missed.
        let mut messages = zbus::MessageStream::from(&conn);
        for (path, iface) in &paths {
            let body = (*iface,);
            let call = conn.call_method(None::<&str>, path.as_str(), Some("org.freedesktop.DBus.Properties"), "GetAll", &body);
            match tokio::time::timeout(Duration::from_secs(2), call).await {
                Ok(Ok(reply)) => {
                    if let Ok(props) = reply.body().deserialize::<HashMap<String, OwnedValue>>() {
                        self.set_props(&id, generation, path, props, true);
                    }
                }
                _ => self.log(&id, format!("its item {path} could not be read")),
            }
        }
        self.publish();
        while let Some(Ok(msg)) = messages.next().await {
            let header = msg.header();
            if header.message_type() != zbus::message::Type::Signal || header.member().is_none_or(|m| m.as_str() != "PropertiesChanged") {
                continue;
            }
            let Some(path) = header.path().map(|p| p.to_string()) else { continue };
            if !paths.iter().any(|(p, _)| *p == path) {
                continue;
            }
            if let Ok((_iface, changed, invalidated)) = msg.body().deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>() {
                self.set_props(&id, generation, &path, changed, false);
                if !invalidated.is_empty() {
                    let iface = paths.iter().find(|(p, _)| *p == path).map(|(_, i)| *i).unwrap_or(IFACE_PANEL);
                    for name in invalidated {
                        let body = (iface, name.as_str());
                        let call = conn.call_method(None::<&str>, path.as_str(), Some("org.freedesktop.DBus.Properties"), "Get", &body);
                        if let Ok(Ok(reply)) = tokio::time::timeout(CALL_TIMEOUT, call).await {
                            if let Ok(value) = reply.body().deserialize::<OwnedValue>() {
                                self.set_props(&id, generation, &path, HashMap::from([(name, value)]), false);
                            }
                        }
                    }
                }
                self.publish();
            }
        }
    }

    fn set_props(&self, id: &str, generation: u64, path: &str, props: HashMap<String, OwnedValue>, replace: bool) {
        let mut inner = self.lock();
        let Some(ext) = inner.exts.get_mut(id).filter(|e| e.generation == generation) else { return };
        let entry = ext.props.entry(path.to_owned()).or_default();
        if replace {
            entry.clear();
        }
        entry.extend(props);
    }

    /// Send the UI a new snapshot, at most once per frame.
    fn publish(&self) {
        {
            let mut inner = self.lock();
            if inner.flush_pending {
                return;
            }
            inner.flush_pending = true;
        }
        let host = self.clone();
        let flush = async move {
            tokio::time::sleep(Duration::from_millis(16)).await;
            let snapshot = {
                let mut inner = host.lock();
                inner.flush_pending = false;
                snapshot(&inner)
            };
            let _ = host.ui.send(UiEvent::Extensions(snapshot));
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(flush);
            }
            Err(_) => self.lock().flush_pending = false,
        }
    }

    /// The Extensions dialog's list changed.
    fn publish_list(&self) {
        let list = self.list();
        let _ = self.ui.send(UiEvent::ExtensionList(list.clone()));
        let session = self.lock().session.clone();
        if let Some(session) = session {
            tokio::spawn(async move {
                if let Ok(emitter) = SignalEmitter::new(&session, super::manager::OBJECT_PATH) {
                    let _ = super::manager::Manager::changed(&emitter, "").await;
                }
            });
        }
    }

    pub fn list(&self) -> Vec<Info> {
        let inner = self.lock();
        let mut out: Vec<Info> = inner
            .exts
            .values()
            .map(|e| {
                let m = e.manifest();
                let error = match &e.state {
                    State::Invalid(r) | State::Crashed(r) => r.clone(),
                    _ => e.log.lines.back().cloned().unwrap_or_default(),
                };
                Info {
                    id: e.found.id.clone(),
                    name: m.map_or_else(|| e.found.id.clone(), |m| m.name.clone()),
                    description: m.map(|m| m.description.clone()).unwrap_or_default(),
                    version: m.map(|m| m.version.clone()).unwrap_or_default(),
                    path: e.found.dir.clone(),
                    kind: e.found.kind,
                    enabled: inner.keys.is_enabled(&e.found),
                    approved: inner.keys.is_approved(&e.found),
                    state: e.state.clone(),
                    permissions: m.map(|m| m.permissions.clone()).unwrap_or_default(),
                    error,
                    settings: m.map(|m| m.settings.iter().map(|d| (d.clone(), e.settings.get(&d.key).cloned().unwrap_or_else(|| d.default.clone()))).collect()).unwrap_or_default(),
                    commands: m
                        .map(|m| m.commands.iter().map(|c| (c.id.clone(), c.title.clone(), inner.keys.command_keys.get(&format!("{}/{}", m.id, c.id)).cloned().unwrap_or_default())).collect())
                        .unwrap_or_default(),
                }
            })
            .collect();
        out.sort_by_key(|i| (i.kind == Kind::Development, i.name.to_lowercase()));
        out
    }

    pub fn enable(&self, id: &str, approve: bool) -> Result<(), String> {
        let found = self.lock().exts.get(id).map(|e| e.found.clone()).ok_or_else(|| format!("no extension {id}"))?;
        found.manifest.as_ref().map_err(|e| e.clone())?;
        // A fresh start, even after it crashed (before the keys change:
        // that change starts it).
        if let Some(ext) = self.lock().exts.get_mut(id) {
            ext.crashes.clear();
            if matches!(ext.state, State::Crashed(_)) {
                ext.state = State::Stopped;
            }
        }
        let mut keys = store::Keys::read();
        keys.enable(&found, approve);
        keys.write();
        self.reconcile();
        Ok(())
    }

    pub fn disable(&self, id: &str) -> Result<(), String> {
        let found = self.lock().exts.get(id).map(|e| e.found.clone()).ok_or_else(|| format!("no extension {id}"))?;
        let mut keys = store::Keys::read();
        keys.disable(&found);
        keys.write();
        self.reconcile();
        Ok(())
    }

    pub fn restart(&self, id: &str) -> Result<(), String> {
        let running = self.lock().exts.get(id).map(|e| e.run.is_some()).ok_or_else(|| format!("no extension {id}"))?;
        if running {
            self.stop(id, State::Stopped);
        }
        if let Some(ext) = self.lock().exts.get_mut(id) {
            ext.crashes.clear();
            ext.state = State::Stopped;
        }
        let host = self.clone();
        let id = id.to_owned();
        tokio::spawn(async move {
            // Let the old process go first.
            for _ in 0..30 {
                if host.lock().exts.get(&id).is_none_or(|e| e.run.is_none()) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            host.reconcile_one(&id);
        });
        Ok(())
    }

    pub fn log_lines(&self, id: &str) -> Option<Vec<String>> {
        self.lock().exts.get(id).map(|e| e.log.lines.iter().cloned().collect())
    }

    /// Set a setting (the dialog, the tools, or the extension itself).
    pub fn set_setting(&self, id: &str, key: &str, value: SettingValue) -> Result<(), String> {
        let (values, conn) = {
            let mut inner = self.lock();
            let ext = inner.exts.get_mut(id).ok_or_else(|| format!("no extension {id}"))?;
            let m = ext.manifest().ok_or("the extension is invalid")?;
            let decl = m.settings.iter().find(|d| d.key == key).ok_or_else(|| format!("no setting {key}"))?;
            let value = decl.check(value)?;
            ext.settings.insert(key.to_owned(), value);
            (ext.settings.clone(), ext.run.as_ref().and_then(|r| r.conn.clone()))
        };
        store::write_settings(&values, &store::settings_path(id)).map_err(|e| e.to_string())?;
        if let Some(conn) = conn {
            let value = values.get(key).map(SettingValue::to_value);
            let key = key.to_owned();
            tokio::spawn(async move {
                if let (Ok(emitter), Some(value)) = (SignalEmitter::new(&conn, HOST_PATH), value) {
                    let _ = HostIface::setting_changed(&emitter, &key, &value).await;
                }
            });
        }
        self.publish_list();
        Ok(())
    }

    /// Run directory `path` as a development extension owned by `owner`.
    pub fn run_development(&self, path: &Path, owner: String) -> Result<String, String> {
        let found = store::load(path, Kind::Development);
        let m = found.manifest.clone()?;
        let id = m.id.clone();
        if let Some(old) = self.lock().exts.get(&id).filter(|e| e.found.kind != Kind::Development && e.run.is_some()) {
            let _ = old;
            return Err(format!("{id} is installed and running; disable it first"));
        }
        self.stop(&id, State::Stopped);
        let settings = store::read_settings(&m, &store::settings_path(&id));
        let stamp = newest(path);
        {
            let mut inner = self.lock();
            let generation = inner.exts.get(&id).map_or(0, |e| e.generation);
            inner.exts.insert(id.clone(), Ext { found, state: State::Stopped, log: Log::for_id(&id), run: None, generation, crashes: Vec::new(), settings, props: HashMap::new(), dev_stamp: stamp, in_flight: false, counted: 0, pending: Vec::new() });
        }
        // Stop it when its owner leaves the bus.
        let session = self.lock().session.clone();
        if let Some(session) = session {
            let host = self.clone();
            let id = id.clone();
            tokio::spawn(async move {
                let Ok(dbus) = zbus::fdo::DBusProxy::new(&session).await else { return };
                let Ok(mut changes) = dbus.receive_name_owner_changed_with_args(&[(0, owner.as_str())]).await else { return };
                while let Some(change) = changes.next().await {
                    if change.args().is_ok_and(|a| a.new_owner().is_none()) {
                        break;
                    }
                }
                let _ = host.stop_development(&id);
            });
        }
        self.restart(&id)?;
        Ok(id)
    }

    pub fn stop_development(&self, id: &str) -> Result<(), String> {
        let is_dev = self.lock().exts.get(id).is_some_and(|e| e.found.kind == Kind::Development);
        if !is_dev {
            return Err(format!("{id} is not a development extension"));
        }
        self.stop(id, State::Stopped);
        let host = self.clone();
        let id = id.to_owned();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(2500)).await;
            host.lock().exts.remove(&id);
            // An installed copy comes back.
            host.rescan();
        });
        Ok(())
    }

    /// Restart development extensions whose files changed.
    fn check_development(&self) {
        let changed: Vec<String> = {
            let mut inner = self.lock();
            inner
                .exts
                .iter_mut()
                .filter(|(_, e)| e.found.kind == Kind::Development)
                .filter_map(|(id, e)| {
                    let now = newest(&e.found.dir);
                    (now != e.dev_stamp).then(|| {
                        e.dev_stamp = now;
                        e.found = store::load(&e.found.dir, Kind::Development);
                        id.clone()
                    })
                })
                .collect()
        };
        for id in changed {
            self.log(&id, "files changed; restarting".into());
            let _ = self.restart(&id);
        }
    }

    /// Quick settings opened: start `onVisible` extensions with toggles.
    pub fn quick_settings_opened(&self) {
        let first = !std::mem::replace(&mut self.lock().quick_opened, true);
        if first {
            self.reconcile();
        }
    }

    /// Carry out a user action on item `full_id`.
    pub async fn act(&self, full_id: &str, action: Action) {
        let Some((id, item)) = full_id.split_once('/') else { return };
        let (conn, generation, has_menu) = {
            let inner = self.lock();
            let Some(ext) = inner.exts.get(id) else { return };
            let Some(conn) = ext.run.as_ref().filter(|r| r.said_hello).and_then(|r| r.conn.clone()) else { return };
            let menu = |kind: &str, prop: &str| ext.props.get(&item_path(kind, item)).and_then(|p| p.get(prop)).and_then(|v| OwnedObjectPath::try_from(v.clone()).ok()).filter(|p| p.as_str() != "/");
            let has_menu = match action {
                Action::Details { .. } => menu("toggle", "Details"),
                Action::DockMenu { .. } => menu("dock_item", "Menu"),
                _ => menu("panel_item", "Menu"),
            };
            (conn, ext.generation, has_menu)
        };
        let panel = item_path("panel_item", item);
        let result: Result<(), String> = match &action {
            Action::Primary { x, y } => self.call(Target { conn: &conn, id, generation }, &panel, IFACE_PANEL, "Activate", &(*x, *y)).await,
            Action::Secondary { .. } => Err("menu".into()),
            Action::Middle { x, y } => self.call(Target { conn: &conn, id, generation }, &panel, IFACE_PANEL, "SecondaryActivate", &(*x, *y)).await,
            Action::Scroll { steps, horizontal } => {
                let orientation = if *horizontal { "horizontal" } else { "vertical" };
                self.call(Target { conn: &conn, id, generation }, &panel, IFACE_PANEL, "Scroll", &(steps * 120, orientation)).await
            }
            Action::Toggle => {
                let path = item_path("toggle", item);
                let (active, sensitive) = {
                    let inner = self.lock();
                    let p = inner.exts.get(id).and_then(|e| e.props.get(&path));
                    let flag = |k: &str, d: bool| p.and_then(|p| p.get(k)).and_then(|v| bool::try_from(v.clone()).ok()).unwrap_or(d);
                    (flag("Active", false), flag("Sensitive", true))
                };
                if sensitive {
                    self.call(Target { conn: &conn, id, generation }, &path, IFACE_TOGGLE, "SetActive", &(!active,)).await
                } else {
                    Ok(())
                }
            }
            Action::Details { .. } => Err("menu".into()),
            Action::SetValue(v) => self.call(Target { conn: &conn, id, generation }, &item_path("slider", item), IFACE_SLIDER, "SetValue", &(v.clamp(0.0, 1.0),)).await,
            Action::MenuChosen { menu, id: entry } => self.call(Target { conn: &conn, id, generation }, menu, IFACE_MENU, "Activated", &(*entry,)).await,
            Action::Popover { x, y, output } => self.call(Target { conn: &conn, id, generation }, &panel, IFACE_PANEL, "TogglePopover", &(*x, *y, output.as_str())).await,
            Action::DockActivate => self.call(Target { conn: &conn, id, generation }, &item_path("dock_item", item), IFACE_DOCK, "Activate", &()).await,
            Action::DockMenu { .. } => Err("menu".into()),
            Action::DockDrop(uris) => self.call(Target { conn: &conn, id, generation }, &item_path("dock_item", item), IFACE_DOCK, "Drop", &(uris,)).await,
        };
        let x = match action {
            Action::Primary { x, .. } | Action::Secondary { x, .. } | Action::Details { x } | Action::DockMenu { x, .. } => Some(x),
            _ => None,
        };
        // A failed primary click, a secondary click, or a toggle's "›":
        // the menu.
        if let (Err(_), Some(x), Some(menu)) = (&result, x, has_menu) {
            let read = conn.call_method(None::<&str>, menu.as_str(), Some("org.freedesktop.DBus.Properties"), "Get", &(IFACE_MENU, "Layout"));
            if let Ok(Ok(reply)) = tokio::time::timeout(CALL_TIMEOUT, read).await {
                if let Ok(value) = reply.body().deserialize::<OwnedValue>() {
                    let entries = menu_entries(value);
                    if !entries.is_empty() {
                        let _ = self.ui.send(UiEvent::ExtMenu { full_id: full_id.to_owned(), menu: menu.to_string(), x, entries });
                    }
                }
            }
        }
    }

    /// Call an item's method with the deadline; timeouts count.
    async fn call<B>(&self, target: Target<'_>, path: &str, iface: &str, method: &str, body: &B) -> Result<(), String>
    where
        B: serde::ser::Serialize + zbus::zvariant::DynamicType,
    {
        let Target { conn, id, generation } = target;
        let call = conn.call_method(None::<&str>, path, Some(iface), method, body);
        match tokio::time::timeout(CALL_TIMEOUT, call).await {
            Ok(result) => {
                self.timed_out(id, generation, false);
                result.map(|_| ()).map_err(|e| e.to_string())
            }
            Err(_) => {
                self.timed_out(id, generation, true);
                Err("timed out".into())
            }
        }
    }

    fn timed_out(&self, id: &str, generation: u64, late: bool) {
        let hung = {
            let mut inner = self.lock();
            let Some(run) = inner.exts.get_mut(id).filter(|e| e.generation == generation).and_then(|e| e.run.as_mut()) else { return };
            run.timeouts = if late { run.timeouts + 1 } else { 0 };
            run.timeouts >= 3
        };
        if hung {
            self.log(id, "did not answer three times in a row; stopped".into());
            self.kill(id, generation);
        }
    }

    fn granted(&self, id: &str, generation: u64, permission: &str) -> zbus::fdo::Result<()> {
        let inner = self.lock();
        let run = inner.exts.get(id).filter(|e| e.generation == generation).and_then(|e| e.run.as_ref());
        match run {
            Some(r) if !r.said_hello => Err(zbus::fdo::Error::AccessDenied("call Hello first".into())),
            Some(r) if permission.is_empty() || r.granted.iter().any(|p| p == permission) => Ok(()),
            Some(_) => Err(zbus::fdo::Error::AccessDenied(format!("needs the {permission} permission"))),
            None => Err(zbus::fdo::Error::AccessDenied("not running".into())),
        }
    }
}

/// Which extension run a call is for.
#[derive(Clone, Copy)]
struct Target<'a> {
    conn: &'a Connection,
    id: &'a str,
    generation: u64,
}

/// The newest change in `dir` (development extensions).
fn newest(dir: &Path) -> Option<std::time::SystemTime> {
    fn walk(dir: &Path, depth: usize, best: &mut Option<std::time::SystemTime>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let Ok(meta) = e.metadata() else { continue };
            if let Ok(t) = meta.modified() {
                if best.is_none_or(|b| t > b) {
                    *best = Some(t);
                }
            }
            if meta.is_dir() && depth > 0 {
                walk(&e.path(), depth - 1, best);
            }
        }
    }
    let mut best = None;
    walk(dir, 3, &mut best);
    best
}

/// A menu's `Layout` value, checked against the limits.
pub fn menu_entries(value: OwnedValue) -> Vec<MenuEntry> {
    let Ok(raw) = <Vec<(i32, i32, String, String, bool, bool)>>::try_from(value) else { return Vec::new() };
    let mut out: Vec<MenuEntry> = Vec::new();
    for (id, parent, kind, label, enabled, checked) in raw.into_iter().take(MENU_MAX) {
        if id <= 0 || out.iter().any(|e| e.id == id) {
            continue;
        }
        // Two levels: a top-level entry or a top-level submenu's child.
        let level_ok = parent == 0 || out.iter().any(|e| e.id == parent && e.parent == 0 && e.kind == "submenu");
        let siblings = out.iter().filter(|e| e.parent == parent).count();
        if !level_ok || siblings >= MENU_LEVEL_MAX {
            continue;
        }
        if !["item", "check", "radio", "separator", "heading", "submenu"].contains(&kind.as_str()) {
            continue;
        }
        out.push(MenuEntry { id, parent, kind, label: cut(&label, LABEL_MAX), enabled, checked });
    }
    out
}

fn snapshot(inner: &Inner) -> Snapshot {
    let mut snap = Snapshot::default();
    for (id, ext) in &inner.exts {
        let Ok(m) = &ext.found.manifest else { continue };
        let development = ext.found.kind == Kind::Development;
        if !development && !(inner.keys.is_enabled(&ext.found) && inner.keys.is_approved(&ext.found)) {
            continue;
        }
        let running = ext.run.as_ref().is_some_and(|r| r.said_hello);
        let dir = ext.found.dir.clone();
        let props = |kind: &str, item: &str| if running { ext.props.get(&item_path(kind, item)).cloned().unwrap_or_default() } else { HashMap::new() };
        let s = |p: &HashMap<String, OwnedValue>, k: &str, d: &str| p.get(k).and_then(|v| String::try_from(v.clone()).ok()).unwrap_or_else(|| d.to_owned());
        let b = |p: &HashMap<String, OwnedValue>, k: &str, d: bool| p.get(k).and_then(|v| bool::try_from(v.clone()).ok()).unwrap_or(d);
        let f = |p: &HashMap<String, OwnedValue>, k: &str, d: f64| p.get(k).and_then(|v| f64::try_from(v.clone()).ok()).filter(|v| v.is_finite()).unwrap_or(d);
        let has_path = |p: &HashMap<String, OwnedValue>, k: &str| p.get(k).and_then(|v| OwnedObjectPath::try_from(v.clone()).ok()).is_some_and(|o| o.as_str() != "/");
        for decl in &m.panel_items {
            let p = props("panel_item", &decl.id);
            let tooltip: String = s(&p, "Tooltip", "").lines().take(TOOLTIP_LINES).collect::<Vec<_>>().join("\n");
            let mut graph: Vec<f64> = p.get("Graph").and_then(|v| <Vec<f64>>::try_from(v.clone()).ok()).unwrap_or_default();
            if graph.len() > GRAPH_MAX {
                graph.drain(..graph.len() - GRAPH_MAX);
            }
            let meter = f(&p, "Meter", -1.0);
            snap.panel.push(PanelItemView {
                full_id: format!("{id}/{}", decl.id),
                name: decl.name.clone(),
                zones: decl.zones.clone(),
                dir: dir.clone(),
                icon: s(&p, "Icon", &decl.icon),
                icon_data: p.get("IconData").and_then(|v| <Vec<u8>>::try_from(v.clone()).ok()).unwrap_or_default(),
                label: cut(&s(&p, "Label", &decl.label), PANEL_LABEL_MAX),
                tooltip: cut(&tooltip, TOOLTIP_MAX),
                accessible_name: cut(&s(&p, "AccessibleName", &decl.name), LABEL_MAX),
                visible: b(&p, "Visible", true),
                attention: b(&p, "Attention", false),
                meter: if meter < 0.0 { -1.0 } else { meter.min(1.0) },
                graph: graph.into_iter().map(|v| if v.is_finite() { v.clamp(0.0, 1.0) } else { 0.0 }).collect(),
                has_menu: has_path(&p, "Menu"),
                running,
                development,
                popover: decl.popover,
            });
        }
        for decl in &m.dock_items {
            let p = props("dock_item", &decl.id);
            let progress = f(&p, "Progress", -1.0);
            snap.dock.push(DockItemView {
                full_id: format!("{id}/{}", decl.id),
                name: decl.name.clone(),
                dir: dir.clone(),
                icon: s(&p, "Icon", &decl.icon),
                icon_data: p.get("IconData").and_then(|v| <Vec<u8>>::try_from(v.clone()).ok()).unwrap_or_default(),
                label: cut(&s(&p, "Label", ""), LABEL_MAX),
                dot: b(&p, "Running", false),
                count: p.get("Count").and_then(|v| i64::try_from(v.clone()).ok()).unwrap_or(0).max(0),
                progress: if progress < 0.0 { -1.0 } else { progress.min(1.0) },
                urgent: b(&p, "Urgent", false),
                has_menu: has_path(&p, "Menu"),
                accepts_drops: decl.accepts_drops,
                running,
            });
        }
        for c in &m.commands {
            let full_id = format!("{id}/{}", c.id);
            let accelerator = inner.keys.command_keys.get(&full_id).cloned().unwrap_or_default();
            snap.commands.push(CommandView { full_id, title: c.title.clone(), accelerator });
        }
        for decl in &m.toggles {
            let p = props("toggle", &decl.id);
            snap.toggles.push(ToggleView {
                full_id: format!("{id}/{}", decl.id),
                dir: dir.clone(),
                title: cut(&s(&p, "Title", &decl.title), LABEL_MAX),
                subtitle: cut(&s(&p, "Subtitle", ""), LABEL_MAX),
                icon: s(&p, "Icon", &decl.icon),
                active: b(&p, "Active", false),
                sensitive: running && b(&p, "Sensitive", true),
                has_details: has_path(&p, "Details"),
                running,
                development,
            });
        }
        for decl in &m.sliders {
            let p = props("slider", &decl.id);
            snap.sliders.push(SliderView {
                full_id: format!("{id}/{}", decl.id),
                dir: dir.clone(),
                title: cut(&s(&p, "Title", &decl.title), LABEL_MAX),
                icon: s(&p, "Icon", &decl.icon),
                value: f(&p, "Value", 0.0).clamp(0.0, 1.0),
                sensitive: running && b(&p, "Sensitive", true),
                running,
                development,
            });
        }
    }
    snap
}

async fn uninhibit(session: &Connection, cookie: u32) {
    let _ = session.call_method(Some("org.gnome.SessionManager"), "/org/gnome/SessionManager", Some("org.gnome.SessionManager"), "Uninhibit", &(cookie,)).await;
}

/// `org.helix.Extension1.Host`, on one extension's connection.
pub struct HostIface {
    host: ExtHost,
    id: String,
    generation: u64,
}

#[zbus::interface(name = "org.helix.Extension1.Host")]
impl HostIface {
    fn hello(&self, api_version: String) -> zbus::fdo::Result<(String, Vec<String>, HashMap<String, OwnedValue>)> {
        let minor = match api_version.split_once('.') {
            Some(("1", m)) => m.parse::<u32>().ok(),
            _ => None,
        };
        if minor.is_none_or(|m| m > manifest::API_MINOR) {
            return Err(zbus::fdo::Error::NotSupported(format!("API {api_version} is not supported; this Helix supports 1.0–1.{}", manifest::API_MINOR)));
        }
        let granted = self.host.said_hello(&self.id, self.generation).ok_or_else(|| zbus::fdo::Error::AccessDenied("not running".into()))?;
        let value = |s: &str| OwnedValue::try_from(Value::from(s.to_owned())).expect("text converts");
        let locale = std::env::var("LC_ALL").or_else(|_| std::env::var("LANG")).unwrap_or_else(|_| "C".into());
        let info = HashMap::from([("helix-version".to_owned(), value(env!("CARGO_PKG_VERSION"))), ("locale".to_owned(), value(&locale))]);
        self.host.log(&self.id, format!("said hello (API {api_version}); granted {granted:?}"));
        Ok((api_version, granted, info))
    }

    fn get_setting(&self, key: String) -> zbus::fdo::Result<OwnedValue> {
        self.host.granted(&self.id, self.generation, "")?;
        let inner = self.host.lock();
        let ext = inner.exts.get(&self.id).ok_or_else(|| zbus::fdo::Error::Failed("gone".into()))?;
        let decl = ext.manifest().and_then(|m| m.settings.iter().find(|d| d.key == key)).ok_or_else(|| zbus::fdo::Error::InvalidArgs(format!("no setting {key}")))?;
        Ok(ext.settings.get(&key).unwrap_or(&decl.default).to_value())
    }

    fn set_setting(&self, key: String, value: OwnedValue) -> zbus::fdo::Result<()> {
        self.host.granted(&self.id, self.generation, "")?;
        let decl = {
            let inner = self.host.lock();
            inner.exts.get(&self.id).and_then(|e| e.manifest()).and_then(|m| m.settings.iter().find(|d| d.key == key).cloned())
        };
        let decl = decl.ok_or_else(|| zbus::fdo::Error::InvalidArgs(format!("no setting {key}")))?;
        let value = decl.from_dbus(&value).map_err(zbus::fdo::Error::InvalidArgs)?;
        self.host.set_setting(&self.id, &key, value).map_err(zbus::fdo::Error::InvalidArgs)
    }

    fn log(&self, level: u32, message: String) -> zbus::fdo::Result<()> {
        self.host.granted(&self.id, self.generation, "")?;
        let level = ["error", "warning", "info", "debug"].get(level as usize).copied().unwrap_or("debug");
        self.host.log(&self.id, format!("{level}: {}", cut(&message, 1000)));
        Ok(())
    }

    fn show_notice(&self, title: String, body: String, icon: String) -> zbus::fdo::Result<()> {
        self.host.granted(&self.id, self.generation, "notifications")?;
        let (center, name) = {
            let inner = self.host.lock();
            (inner.center.clone(), inner.exts.get(&self.id).and_then(|e| e.manifest()).map(|m| m.name.clone()).unwrap_or_default())
        };
        if let Some(center) = center {
            center.notice(&name, &cut(&title, LABEL_MAX), &cut(&body, TOOLTIP_MAX), &icon, false);
        }
        Ok(())
    }

    async fn inhibit_idle(&self, reason: String) -> zbus::fdo::Result<u32> {
        self.host.granted(&self.id, self.generation, "inhibit-idle")?;
        let session = self.host.lock().session.clone().ok_or_else(|| zbus::fdo::Error::Failed("no session".into()))?;
        let reply = session
            .call_method(Some("org.gnome.SessionManager"), "/org/gnome/SessionManager", Some("org.gnome.SessionManager"), "Inhibit", &(format!("helix-extension:{}", self.id), 0u32, cut(&reason, LABEL_MAX), 8u32))
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        let (cookie,): (u32,) = reply.body().deserialize().map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        let mut inner = self.host.lock();
        let run = inner.exts.get_mut(&self.id).filter(|e| e.generation == self.generation).and_then(|e| e.run.as_mut()).ok_or_else(|| zbus::fdo::Error::Failed("gone".into()))?;
        let handle = run.next_handle;
        run.next_handle += 1;
        run.inhibitors.insert(handle, cookie);
        Ok(handle)
    }

    async fn uninhibit(&self, handle: u32) -> zbus::fdo::Result<()> {
        self.host.granted(&self.id, self.generation, "")?;
        let (cookie, session) = {
            let mut inner = self.host.lock();
            let session = inner.session.clone();
            let cookie = inner.exts.get_mut(&self.id).and_then(|e| e.run.as_mut()).and_then(|r| r.inhibitors.remove(&handle));
            (cookie, session)
        };
        if let (Some(cookie), Some(session)) = (cookie, session) {
            uninhibit(&session, cookie).await;
        }
        Ok(())
    }

    async fn launch(&self, desktop_id: String, uris: Vec<String>) -> zbus::fdo::Result<()> {
        self.host.granted(&self.id, self.generation, "launch-apps")?;
        tokio::task::spawn_blocking(move || {
            use gio::prelude::*;
            let info = gio::AppInfo::all().into_iter().find(|a| a.id().is_some_and(|i| i == desktop_id)).ok_or_else(|| format!("no app {desktop_id}"))?;
            let uris: Vec<&str> = uris.iter().map(String::as_str).collect();
            info.launch_uris(&uris, None::<&gio::AppLaunchContext>).map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?
        .map_err(zbus::fdo::Error::Failed)
    }

    #[zbus(signal)]
    pub async fn setting_changed(emitter: &SignalEmitter<'_>, key: &str, value: &OwnedValue) -> zbus::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menus_are_limited() {
        let v = |entries: Vec<(i32, i32, &str, &str, bool, bool)>| OwnedValue::try_from(Value::from(entries.into_iter().map(|(a, b, c, d, e, f)| (a, b, c.to_owned(), d.to_owned(), e, f)).collect::<Vec<_>>())).unwrap();
        let menu = menu_entries(v(vec![(1, 0, "item", "One", true, false), (2, 0, "submenu", "More", true, false), (3, 2, "item", "Inner", true, false), (4, 3, "item", "Too deep", true, false), (1, 0, "item", "Same id", true, false), (5, 0, "bogus", "Kind", true, false), (6, 9, "item", "No parent", true, false)]));
        assert_eq!(menu.iter().map(|e| e.id).collect::<Vec<_>>(), [1, 2, 3]);
        let many: Vec<(i32, i32, &str, &str, bool, bool)> = (1..=100).map(|i| (i, 0, "item", "x", true, false)).collect();
        assert_eq!(menu_entries(v(many)).len(), MENU_LEVEL_MAX);
        assert_eq!(cut("abcdef", 4), "abc…");
    }
}
