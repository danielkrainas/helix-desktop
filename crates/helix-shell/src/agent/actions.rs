//! What agents do at the *Control the desktop* level (helixdesktop/spec
//! `contracts/agent-access/README.md`, "Control the desktop methods";
//! helixdesktop/desktop#147): open files and addresses and launch apps
//! through the shell's own launcher (so a `.desktop` file launches only
//! when trusted, as on the desktop), act on windows and workspaces
//! through the compositor's window list (as the window menu and the
//! overview do), change quick settings, and show notifications labelled
//! with the agent.
//!
//! Window actions travel to the UI thread, which holds the window list,
//! as [`UiEvent::AgentWindow`](crate::UiEvent::AgentWindow).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::oneshot;
use zbus::zvariant::OwnedValue;

use super::desktop::{Snapshot, WindowInfo, FULLSCREEN, MAXIMIZED, TILED_LEFT, TILED_RIGHT};
use super::policy::{AgentError, SETTINGS_APP_ID};
use crate::windows::WindowAction;

/// The Settings app's program: an app that runs it counts as Settings
/// (README.md, "Hard exclusions"), whatever its desktop file is called.
pub const SETTINGS_PROGRAM: &str = "helix-settings-app";

/// A string option, `None` when absent or empty.
pub fn text_option(options: &HashMap<String, OwnedValue>, key: &str) -> Result<Option<String>, AgentError> {
    match options.get(key) {
        None => Ok(None),
        Some(v) => String::try_from(v.clone()).map(|s| Some(s).filter(|s| !s.is_empty())).map_err(|_| AgentError::invalid_args(format!("“{key}” must be a string"))),
    }
}

fn int_option(options: &HashMap<String, OwnedValue>, key: &str) -> Result<Option<i32>, AgentError> {
    match options.get(key) {
        None => Ok(None),
        Some(v) => i32::try_from(v.clone()).map(Some).map_err(|_| AgentError::invalid_args(format!("“{key}” must be a number (i)"))),
    }
}

fn uint_option(options: &HashMap<String, OwnedValue>, key: &str) -> Result<Option<u32>, AgentError> {
    match options.get(key) {
        None => Ok(None),
        Some(v) => u32::try_from(v.clone()).map(Some).map_err(|_| AgentError::invalid_args(format!("“{key}” must be a number (u)"))),
    }
}

// ----- Opening and launching ---------------------------------------------

/// The schemes agents may open besides local files: web and mail
/// addresses. Others (`ssh:`, `smb:`, apps' own schemes that start
/// things) are refused, so an agent cannot reach an app's handler that
/// acts without asking.
pub const SCHEMES: &[&str] = &["http", "https", "mailto"];

/// What an agent asked to open: a local file or folder, or an address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Uri {
    /// An absolute path (from a path or a `file://` URI).
    File(std::path::PathBuf),
    /// An address of one of [`SCHEMES`]: the scheme (lower case) and the
    /// whole address.
    Address { scheme: String, uri: String },
}

impl Uri {
    /// An absolute path, a `file://` URI, or an address of [`SCHEMES`];
    /// `InvalidArgs` for anything else that is not a URI, `NotSupported`
    /// for other schemes.
    pub fn parse(text: &str) -> Result<Uri, AgentError> {
        if text.starts_with('/') {
            return Ok(Uri::File(text.into()));
        }
        let scheme = text.split_once(':').map(|(s, _)| s).filter(|s| s.starts_with(|c: char| c.is_ascii_alphabetic()) && s.chars().all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c)));
        let Some(scheme) = scheme.map(str::to_ascii_lowercase) else {
            return Err(AgentError::invalid_args("not an absolute path or a URI"));
        };
        if scheme == "file" {
            let path = gio::glib::filename_from_uri(text).ok().map(|(p, host)| (p, host.filter(|h| !h.is_empty() && h != "localhost")));
            return match path {
                Some((path, None)) if path.is_absolute() => Ok(Uri::File(path)),
                _ => Err(AgentError::invalid_args("not a local file URI")),
            };
        }
        if SCHEMES.contains(&scheme.as_str()) {
            Ok(Uri::Address { scheme, uri: text.to_owned() })
        } else {
            Err(AgentError::NotSupported(format!("{scheme}: only files and http, https and mailto addresses can be opened")))
        }
    }

    /// The URI to hand to an app.
    pub fn uri(&self) -> String {
        match self {
            Uri::File(path) => gio::glib::filename_to_uri(path, None).map(|u| u.to_string()).unwrap_or_default(),
            Uri::Address { uri, .. } => uri.clone(),
        }
    }

    /// What the activity log records: a file's URI, of an address only
    /// the scheme and the host (`https://example.org`, `mailto:`): its
    /// path and query can hold anything (README.md, "No secrets").
    pub fn log_target(&self) -> String {
        match self {
            Uri::File(_) => self.uri(),
            Uri::Address { scheme, uri } => match uri.split_once("://") {
                Some((_, rest)) => {
                    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
                    // Without any user name or password.
                    let host = host.rsplit('@').next().unwrap_or(host);
                    format!("{scheme}://{host}")
                }
                None => format!("{scheme}:"),
            },
        }
    }
}

/// How an `Open` or `Launch` is done, worked out first so the apps it
/// would start can be checked against the exclusions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launch {
    /// Start installed app `id` (a desktop file id) with `uris`.
    App { id: String, uris: Vec<String> },
    /// Start the application of a trusted `.desktop` file.
    DesktopFile(std::path::PathBuf),
}

/// A [`Launch`] and the app ids it starts, to check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub launch: Launch,
    pub apps: Vec<String>,
}

fn bare(id: &str) -> &str {
    id.strip_suffix(".desktop").unwrap_or(id)
}

/// The app ids to check for `info`: its id, and Settings' when it runs
/// the Settings program.
fn app_ids(info: &gio::AppInfo) -> Vec<String> {
    use gio::prelude::*;
    let mut ids: Vec<String> = info.id().map(|i| bare(&i).to_owned()).into_iter().collect();
    if runs_settings(info) {
        ids.push(SETTINGS_APP_ID.to_owned());
    }
    ids
}

/// Whether `info` starts the Settings app.
fn runs_settings(info: &gio::AppInfo) -> bool {
    use gio::prelude::*;
    // Not `executable()`: D-Bus-activatable apps may lack an Exec line.
    runs_settings_line(&info.commandline().map(|c| c.to_string_lossy().into_owned()).unwrap_or_default())
}

/// Whether command line (an `Exec` line) `line` starts the Settings app.
fn runs_settings_line(line: &str) -> bool {
    // The program, past `env` and its variables.
    let program = line.split_whitespace().find(|w| *w != "env" && !w.contains('=')).unwrap_or("");
    std::path::Path::new(program.trim_matches('"')).file_name().is_some_and(|n| n == SETTINGS_PROGRAM)
}

/// The `Exec` line of the `.desktop` file at `path`.
fn exec_line(path: &std::path::Path) -> String {
    let file = gio::glib::KeyFile::new();
    if file.load_from_file(path, gio::glib::KeyFileFlags::NONE).is_err() {
        return String::new();
    }
    file.string("Desktop Entry", "Exec").map(|e| e.to_string()).unwrap_or_default()
}

/// The installed app `id` (with or without `.desktop`) the app grid
/// shows.
fn shown_app(id: &str) -> Option<gio::AppInfo> {
    use gio::prelude::*;
    let wanted = format!("{}.desktop", bare(id));
    gio::AppInfo::all().into_iter().find(|a| a.should_show() && a.id().is_some_and(|i| i == wanted))
}

/// `Launch`: app `id` as the app grid has it, with `uris`.
pub fn plan_launch(id: &str, uris: Vec<String>) -> Result<Plan, AgentError> {
    use gio::prelude::*;
    let info = shown_app(id).ok_or_else(|| AgentError::NotFound(format!("app {id}")))?;
    let desktop_id = info.id().map(|i| i.to_string()).unwrap_or_default();
    Ok(Plan { apps: app_ids(&info), launch: Launch::App { id: desktop_id, uris } })
}

/// `Open`: with `app`, that app; a trusted `.desktop` launcher starts
/// its application, as on the desktop (an untrusted one is a plain
/// file); otherwise the default app for the file's type or the address's
/// scheme.
pub fn plan_open(uri: &Uri, app: Option<&str>) -> Result<Plan, AgentError> {
    use gio::prelude::*;
    if let Uri::File(path) = uri {
        if std::fs::symlink_metadata(path).is_err() {
            return Err(AgentError::NotFound(path.display().to_string()));
        }
    }
    if let Some(app) = app {
        return plan_launch(app, vec![uri.uri()]);
    }
    let handler = match uri {
        Uri::File(path) => {
            if path.extension().is_some_and(|e| e == "desktop") && crate::desktop_icons::trusted_launcher(path) {
                let mut apps = vec![path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()];
                if runs_settings_line(&exec_line(path)) {
                    apps.push(SETTINGS_APP_ID.to_owned());
                }
                return Ok(Plan { launch: Launch::DesktopFile(path.clone()), apps });
            }
            let info = gio::File::for_path(path).query_info("standard::content-type", gio::FileQueryInfoFlags::NONE, gio::Cancellable::NONE);
            let content_type = info.ok().and_then(|i| i.content_type()).map(|t| t.to_string()).unwrap_or_else(|| "application/octet-stream".to_owned());
            gio::AppInfo::default_for_type(&content_type, false).ok_or_else(|| AgentError::NotFound(format!("an app for {content_type}")))?
        }
        Uri::Address { scheme, .. } => gio::AppInfo::default_for_uri_scheme(scheme).ok_or_else(|| AgentError::NotFound(format!("an app for {scheme}:")))?,
    };
    let id = handler.id().map(|i| i.to_string()).ok_or_else(|| AgentError::NotFound("an app to open it".into()))?;
    Ok(Plan { apps: app_ids(&handler), launch: Launch::App { id, uris: vec![uri.uri()] } })
}

/// Do a planned launch, through the shell's launcher.
pub fn launch(launch: &Launch) -> Result<(), AgentError> {
    match launch {
        Launch::App { id, uris } => crate::launcher::launch_with(id, uris),
        Launch::DesktopFile(path) => crate::desktop_icons::launch_file(path),
    }
    .map_err(AgentError::Failed)
}

// ----- Windows and workspaces ---------------------------------------------

/// A `WindowAction` action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowCommand {
    Focus,
    Close,
    Minimize,
    Maximize,
    Unmaximize,
    /// Back to normal: unminimized, and not maximized, tiled or full
    /// screen.
    Restore,
    Fullscreen,
    Unfullscreen,
    TileLeft,
    TileRight,
    Move { x: i32, y: i32 },
    Resize { width: i32, height: i32 },
    ToWorkspace(u32),
}

/// The action names, as `WindowAction` takes them.
pub const WINDOW_ACTIONS: &[&str] =
    &["focus", "close", "minimize", "maximize", "unmaximize", "restore", "fullscreen", "unfullscreen", "tile-left", "tile-right", "move", "resize", "to-workspace"];

impl WindowCommand {
    /// `action` with its options (`x`, `y`; `width`, `height`;
    /// `workspace`).
    pub fn parse(action: &str, options: &HashMap<String, OwnedValue>) -> Result<WindowCommand, AgentError> {
        let need_int = |key: &str| int_option(options, key)?.ok_or_else(|| AgentError::invalid_args(format!("“{action}” needs “{key}”")));
        Ok(match action {
            "focus" => WindowCommand::Focus,
            "close" => WindowCommand::Close,
            "minimize" => WindowCommand::Minimize,
            "maximize" => WindowCommand::Maximize,
            "unmaximize" => WindowCommand::Unmaximize,
            "restore" => WindowCommand::Restore,
            "fullscreen" => WindowCommand::Fullscreen,
            "unfullscreen" => WindowCommand::Unfullscreen,
            "tile-left" => WindowCommand::TileLeft,
            "tile-right" => WindowCommand::TileRight,
            "move" => WindowCommand::Move { x: need_int("x")?, y: need_int("y")? },
            "resize" => {
                let (width, height) = (need_int("width")?, need_int("height")?);
                if width < 1 || height < 1 {
                    return Err(AgentError::invalid_args("“width” and “height” must be at least 1"));
                }
                WindowCommand::Resize { width, height }
            }
            "to-workspace" => WindowCommand::ToWorkspace(uint_option(options, "workspace")?.ok_or_else(|| AgentError::invalid_args("“to-workspace” needs “workspace”"))?),
            other => return Err(AgentError::NotSupported(format!("window action {other}"))),
        })
    }

    /// Its name, for the activity log.
    pub fn name(self) -> &'static str {
        match self {
            WindowCommand::Focus => "focus",
            WindowCommand::Close => "close",
            WindowCommand::Minimize => "minimize",
            WindowCommand::Maximize => "maximize",
            WindowCommand::Unmaximize => "unmaximize",
            WindowCommand::Restore => "restore",
            WindowCommand::Fullscreen => "fullscreen",
            WindowCommand::Unfullscreen => "unfullscreen",
            WindowCommand::TileLeft => "tile-left",
            WindowCommand::TileRight => "tile-right",
            WindowCommand::Move { .. } => "move",
            WindowCommand::Resize { .. } => "resize",
            WindowCommand::ToWorkspace(_) => "to-workspace",
        }
    }
}

/// Whether `action` is sensitive by itself: closing may lose what the
/// window holds (README.md, "Methods").
pub fn window_action_sensitive(action: &str) -> bool {
    action == "close"
}

/// A request to the compositor's window list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowOp {
    /// Focus (raising, unminimizing and switching to its workspace).
    Activate(u32),
    Action(u32, WindowAction),
    MoveResize { id: u32, x: i32, y: i32, width: i32, height: i32 },
    ToWorkspace { id: u32, workspace: u32 },
    SwitchWorkspace(u32),
}

impl WindowOp {
    /// The window list version the request needs.
    pub fn since(self) -> u32 {
        match self {
            WindowOp::Activate(_) => 1,
            WindowOp::ToWorkspace { .. } | WindowOp::SwitchWorkspace(_) => 2,
            WindowOp::Action(..) => 4,
            WindowOp::MoveResize { .. } => 12,
        }
    }
}

/// Whether workspace `index` exists.
pub fn workspace_exists(snapshot: &Snapshot, index: u32) -> bool {
    index < snapshot.workspaces.max(1)
}

/// The requests that do `command` to `w`: none when it is so already.
pub fn plan_window(command: WindowCommand, w: &WindowInfo, snapshot: &Snapshot) -> Result<Vec<WindowOp>, AgentError> {
    let id = w.id;
    let act = |a: WindowAction| vec![WindowOp::Action(id, a)];
    let placed = w.state & (MAXIMIZED | FULLSCREEN | TILED_LEFT | TILED_RIGHT) != 0;
    let shown = || -> Result<(), AgentError> {
        if w.minimized {
            return Err(AgentError::Failed("the window is minimized".into()));
        }
        if w.workspace != super::desktop::ALL_WORKSPACES && w.workspace != snapshot.active {
            return Err(AgentError::Failed("the window is on another workspace".into()));
        }
        Ok(())
    };
    let (x, y, width, height) = w.geometry;
    Ok(match command {
        WindowCommand::Focus => vec![WindowOp::Activate(id)],
        WindowCommand::Close => act(WindowAction::Close),
        WindowCommand::Minimize if w.minimized => Vec::new(),
        WindowCommand::Minimize => act(WindowAction::Minimize),
        WindowCommand::Maximize if w.state & MAXIMIZED != 0 => Vec::new(),
        WindowCommand::Maximize => act(WindowAction::Maximize),
        WindowCommand::Unmaximize if w.state & MAXIMIZED == 0 => Vec::new(),
        WindowCommand::Unmaximize => act(WindowAction::Unmaximize),
        WindowCommand::Restore => {
            let mut ops = Vec::new();
            if w.minimized {
                ops.push(WindowOp::Activate(id));
            }
            if placed {
                ops.push(WindowOp::Action(id, WindowAction::Unmaximize));
            }
            ops
        }
        WindowCommand::Fullscreen if w.state & FULLSCREEN != 0 => Vec::new(),
        WindowCommand::Unfullscreen if w.state & FULLSCREEN == 0 => Vec::new(),
        WindowCommand::Fullscreen | WindowCommand::Unfullscreen => act(WindowAction::ToggleFullscreen),
        WindowCommand::TileLeft if w.state & TILED_LEFT != 0 => Vec::new(),
        WindowCommand::TileLeft => act(WindowAction::ToggleTiledLeft),
        WindowCommand::TileRight if w.state & TILED_RIGHT != 0 => Vec::new(),
        WindowCommand::TileRight => act(WindowAction::ToggleTiledRight),
        WindowCommand::Move { x, y } => {
            shown()?;
            vec![WindowOp::MoveResize { id, x, y, width, height }]
        }
        WindowCommand::Resize { width, height } => {
            shown()?;
            vec![WindowOp::MoveResize { id, x, y, width, height }]
        }
        WindowCommand::ToWorkspace(workspace) => {
            if !workspace_exists(snapshot, workspace) {
                return Err(AgentError::NotFound(format!("workspace {workspace}")));
            }
            if w.workspace == workspace {
                Vec::new()
            } else {
                vec![WindowOp::ToWorkspace { id, workspace }]
            }
        }
    })
}

/// Whether `snapshot` shows `command` done to window `id`. A window may
/// end up elsewhere than asked (kept on its monitor) or at another size
/// (its app's choice), or ask before closing: callers wait for this a
/// little, not for ever.
pub fn window_done(command: WindowCommand, id: u32, snapshot: &Snapshot) -> bool {
    let Some(w) = snapshot.windows.iter().find(|w| w.id == id) else { return true };
    match command {
        WindowCommand::Focus => snapshot.focused == id,
        WindowCommand::Close => false,
        WindowCommand::Minimize => w.minimized,
        WindowCommand::Maximize => w.state & MAXIMIZED != 0,
        WindowCommand::Unmaximize => w.state & MAXIMIZED == 0,
        WindowCommand::Restore => !w.minimized && w.state & (MAXIMIZED | FULLSCREEN | TILED_LEFT | TILED_RIGHT) == 0,
        WindowCommand::Fullscreen => w.state & FULLSCREEN != 0,
        WindowCommand::Unfullscreen => w.state & FULLSCREEN == 0,
        WindowCommand::TileLeft => w.state & TILED_LEFT != 0,
        WindowCommand::TileRight => w.state & TILED_RIGHT != 0,
        WindowCommand::Move { x, y } => (w.geometry.0, w.geometry.1) == (x, y),
        WindowCommand::Resize { width, height } => (w.geometry.2, w.geometry.3) == (width, height),
        WindowCommand::ToWorkspace(workspace) => w.workspace == workspace,
    }
}

/// Requests for the window list, and where the answer goes.
#[derive(Debug)]
pub struct WindowRequest {
    pub ops: Vec<WindowOp>,
    /// `Err` when the compositor cannot do one of them (an older window
    /// list), in which case none is done.
    pub reply: oneshot::Sender<Result<(), String>>,
}

/// A [`WindowRequest`] that can travel in a `UiEvent`.
#[derive(Debug, Clone)]
pub struct WindowSlot(Arc<Mutex<Option<WindowRequest>>>);

impl WindowSlot {
    pub fn new(request: WindowRequest) -> Self {
        Self(Arc::new(Mutex::new(Some(request))))
    }

    pub fn take(&self) -> Option<WindowRequest> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

impl PartialEq for WindowSlot {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

// ----- Quick settings -----------------------------------------------------

/// A quick setting `SetToggle` turns on or off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toggle {
    DarkStyle,
    DoNotDisturb,
    NightLight,
    Wifi,
    AirplaneMode,
    Mute,
}

impl Toggle {
    pub const ALL: [Toggle; 6] = [Toggle::DarkStyle, Toggle::DoNotDisturb, Toggle::NightLight, Toggle::Wifi, Toggle::AirplaneMode, Toggle::Mute];

    pub fn name(self) -> &'static str {
        match self {
            Toggle::DarkStyle => "dark-style",
            Toggle::DoNotDisturb => "do-not-disturb",
            Toggle::NightLight => "night-light",
            Toggle::Wifi => "wifi",
            Toggle::AirplaneMode => "airplane-mode",
            Toggle::Mute => "mute",
        }
    }

    pub fn from_name(name: &str) -> Option<Toggle> {
        Toggle::ALL.into_iter().find(|t| t.name() == name)
    }

    /// Network changes are sensitive (README.md, "Methods").
    pub fn sensitive(self) -> bool {
        matches!(self, Toggle::Wifi | Toggle::AirplaneMode)
    }

    /// What the sensitive-action prompt says it does (its action token).
    pub fn action(self, on: bool) -> Option<&'static str> {
        match (self, on) {
            (Toggle::Wifi, true) => Some("wifi-on"),
            (Toggle::Wifi, false) => Some("wifi-off"),
            (Toggle::AirplaneMode, true) => Some("airplane-mode-on"),
            (Toggle::AirplaneMode, false) => Some("airplane-mode-off"),
            _ => None,
        }
    }
}

/// A value `SetValue` sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slider {
    Volume,
    Brightness,
}

impl Slider {
    pub fn name(self) -> &'static str {
        match self {
            Slider::Volume => "volume",
            Slider::Brightness => "brightness",
        }
    }

    pub fn from_name(name: &str) -> Option<Slider> {
        [Slider::Volume, Slider::Brightness].into_iter().find(|l| l.name() == name)
    }
}

/// `value` (0–1, as the State record has it; the volume up to `max`,
/// 1.5 with over-amplification) as a whole percentage.
pub fn percent(value: f64, max: f64) -> Result<u32, AgentError> {
    if !value.is_finite() || value < 0.0 || value > max + 1e-9 {
        return Err(AgentError::invalid_args(format!("the value must be from 0 to {max}")));
    }
    Ok((value * 100.0).round() as u32)
}

// ----- Notifications -------------------------------------------------------

/// How many notifications an agent may show in [`NOTIFY_WINDOW`].
pub const NOTIFY_MAX: usize = 5;
pub const NOTIFY_WINDOW: Duration = Duration::from_secs(60);

/// Notifications shown per agent, for the rate limit.
#[derive(Debug, Default)]
pub struct NotifyLimit {
    shown: HashMap<String, VecDeque<Instant>>,
}

impl NotifyLimit {
    /// Count one for `agent` at `now`, unless it showed [`NOTIFY_MAX`]
    /// in the last [`NOTIFY_WINDOW`].
    pub fn allow(&mut self, agent: &str, now: Instant) -> bool {
        self.shown.retain(|_, times| {
            while times.front().is_some_and(|t| now.duration_since(*t) >= NOTIFY_WINDOW) {
                times.pop_front();
            }
            !times.is_empty()
        });
        let times = self.shown.entry(agent.to_owned()).or_default();
        if times.len() >= NOTIFY_MAX {
            return false;
        }
        times.push_back(now);
        true
    }
}

/// An icon name an agent may give: a theme icon's name, not a path.
pub fn icon_name(name: &str) -> Result<&str, AgentError> {
    let ok = !name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) && !name.starts_with('.');
    if ok { Ok(name) } else { Err(AgentError::invalid_args("“icon” must be an icon name")) }
}

/// How a notification names the agent: its display name and the
/// executable's file name, or the id's file names ("python3 bot.py"), as
/// the indicator does.
pub fn agent_label(agent: &super::identity::Agent) -> String {
    super::prompt::Active { agent: agent.id.clone(), executable: agent.executable.clone(), name: agent.name.clone(), level: super::Level::Desktop }.label()
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

    #[test]
    fn uris() {
        assert_eq!(Uri::parse("/home/me/a b.txt").unwrap(), Uri::File("/home/me/a b.txt".into()));
        assert_eq!(Uri::parse("file:///home/me/a%20b.txt").unwrap(), Uri::File("/home/me/a b.txt".into()));
        assert_eq!(Uri::parse("file:///home/me/a%20b.txt").unwrap().uri(), "file:///home/me/a%20b.txt");
        assert_eq!(Uri::parse("HTTPS://example.org/x").unwrap(), Uri::Address { scheme: "https".into(), uri: "HTTPS://example.org/x".into() });
        assert!(matches!(Uri::parse("mailto:a@example.org").unwrap(), Uri::Address { .. }));
        for refused in ["ssh://host", "smb://host/share", "x-helix-settings:agent-access", "steam://run/1", "javascript:alert(1)"] {
            assert_eq!(kind(Uri::parse(refused)), "NotSupported", "{refused}");
        }
        for bad in ["notes.txt", "./notes.txt", "", "file://otherhost/etc/passwd", ":nothing"] {
            assert_eq!(kind(Uri::parse(bad)), "InvalidArgs", "{bad}");
        }
    }

    #[test]
    fn log_targets() {
        let t = |s: &str| Uri::parse(s).unwrap().log_target();
        assert_eq!(t("/home/me/report.pdf"), "file:///home/me/report.pdf");
        assert_eq!(t("https://user:secret@example.org:8080/reset?token=abc#x"), "https://example.org:8080");
        assert_eq!(t("http://example.org"), "http://example.org");
        assert_eq!(t("mailto:lawyer@example.org?subject=private"), "mailto:");
    }

    #[test]
    fn window_commands() {
        assert_eq!(WindowCommand::parse("focus", &opts(&[])).unwrap(), WindowCommand::Focus);
        assert_eq!(WindowCommand::parse("move", &opts(&[("x", Value::from(10)), ("y", Value::from(-5))])).unwrap(), WindowCommand::Move { x: 10, y: -5 });
        assert_eq!(kind(WindowCommand::parse("move", &opts(&[("x", Value::from(10))]))), "InvalidArgs");
        assert_eq!(kind(WindowCommand::parse("resize", &opts(&[("width", Value::from(0)), ("height", Value::from(5))]))), "InvalidArgs");
        assert_eq!(WindowCommand::parse("to-workspace", &opts(&[("workspace", Value::from(1u32))])).unwrap(), WindowCommand::ToWorkspace(1));
        assert_eq!(kind(WindowCommand::parse("to-workspace", &opts(&[("workspace", Value::from("1"))]))), "InvalidArgs");
        assert_eq!(kind(WindowCommand::parse("shake", &opts(&[]))), "NotSupported");
        for name in WINDOW_ACTIONS {
            let options = opts(&[("x", Value::from(1)), ("y", Value::from(1)), ("width", Value::from(1)), ("height", Value::from(1)), ("workspace", Value::from(0u32))]);
            assert_eq!(WindowCommand::parse(name, &options).unwrap().name(), *name);
        }
        assert!(window_action_sensitive("close") && !window_action_sensitive("minimize"));
    }

    #[test]
    fn settings_by_program() {
        assert!(runs_settings_line("helix-settings-app"));
        assert!(runs_settings_line("/usr/bin/helix-settings-app agent-access"));
        assert!(runs_settings_line("env GTK_DEBUG=1 helix-settings-app %U"));
        assert!(runs_settings_line("\"/opt/helix/helix-settings-app\""));
        assert!(!runs_settings_line("gnome-text-editor %U"));
        assert!(!runs_settings_line(""));
    }

    fn snap(w: WindowInfo) -> Snapshot {
        Snapshot { focused: 0, workspaces: 2, active: 0, windows: vec![w], ..Default::default() }
    }

    #[test]
    fn window_plans() {
        let w = WindowInfo { id: 7, app_id: "a".into(), geometry: (10, 20, 300, 200), ..Default::default() };
        let s = snap(w.clone());
        assert_eq!(plan_window(WindowCommand::Focus, &w, &s).unwrap(), [WindowOp::Activate(7)]);
        assert_eq!(plan_window(WindowCommand::TileLeft, &w, &s).unwrap(), [WindowOp::Action(7, WindowAction::ToggleTiledLeft)]);
        assert_eq!(plan_window(WindowCommand::Move { x: 1, y: 2 }, &w, &s).unwrap(), [WindowOp::MoveResize { id: 7, x: 1, y: 2, width: 300, height: 200 }]);
        assert_eq!(plan_window(WindowCommand::Resize { width: 5, height: 6 }, &w, &s).unwrap(), [WindowOp::MoveResize { id: 7, x: 10, y: 20, width: 5, height: 6 }]);
        assert_eq!(plan_window(WindowCommand::ToWorkspace(1), &w, &s).unwrap(), [WindowOp::ToWorkspace { id: 7, workspace: 1 }]);
        assert_eq!(kind(plan_window(WindowCommand::ToWorkspace(2), &w, &s)), "NotFound");
        // Already so: nothing to do (toggles are not flipped back).
        let tiled = WindowInfo { state: TILED_LEFT, ..w.clone() };
        assert!(plan_window(WindowCommand::TileLeft, &tiled, &s).unwrap().is_empty());
        assert_eq!(plan_window(WindowCommand::TileRight, &tiled, &s).unwrap(), [WindowOp::Action(7, WindowAction::ToggleTiledRight)]);
        assert!(plan_window(WindowCommand::Unfullscreen, &w, &s).unwrap().is_empty());
        assert!(plan_window(WindowCommand::ToWorkspace(0), &w, &s).unwrap().is_empty());
        // Restore undoes whatever it is.
        let both = WindowInfo { minimized: true, state: MAXIMIZED, ..w.clone() };
        assert_eq!(plan_window(WindowCommand::Restore, &both, &s).unwrap(), [WindowOp::Activate(7), WindowOp::Action(7, WindowAction::Unmaximize)]);
        assert!(plan_window(WindowCommand::Restore, &w, &s).unwrap().is_empty());
        // Moving needs it shown.
        assert_eq!(kind(plan_window(WindowCommand::Move { x: 0, y: 0 }, &both, &s)), "Failed");
        let away = WindowInfo { workspace: 1, ..w.clone() };
        assert_eq!(kind(plan_window(WindowCommand::Resize { width: 9, height: 9 }, &away, &s)), "Failed");
        assert_eq!(WindowOp::MoveResize { id: 1, x: 0, y: 0, width: 1, height: 1 }.since(), 12);
    }

    #[test]
    fn window_results() {
        let w = WindowInfo { id: 7, geometry: (10, 20, 300, 200), state: TILED_LEFT, ..Default::default() };
        let mut s = snap(w);
        assert!(window_done(WindowCommand::TileLeft, 7, &s));
        assert!(window_done(WindowCommand::Move { x: 10, y: 20 }, 7, &s));
        assert!(!window_done(WindowCommand::Focus, 7, &s));
        s.focused = 7;
        assert!(window_done(WindowCommand::Focus, 7, &s));
        assert!(!window_done(WindowCommand::Restore, 7, &s));
        assert!(window_done(WindowCommand::Close, 8, &s), "gone");
    }

    #[test]
    fn toggles_and_values() {
        for t in Toggle::ALL {
            assert_eq!(Toggle::from_name(t.name()), Some(t));
        }
        assert!(Toggle::Wifi.sensitive() && Toggle::AirplaneMode.sensitive() && !Toggle::DarkStyle.sensitive());
        assert_eq!(Toggle::Wifi.action(false), Some("wifi-off"));
        assert_eq!(Toggle::from_name("bluetooth"), None);
        assert_eq!(percent(0.42, 1.0).unwrap(), 42);
        assert_eq!(percent(1.5, 1.5).unwrap(), 150);
        assert_eq!(kind(percent(1.2, 1.0)), "InvalidArgs");
        assert_eq!(kind(percent(-0.1, 1.0)), "InvalidArgs");
        assert_eq!(kind(percent(f64::NAN, 1.0)), "InvalidArgs");
    }

    #[test]
    fn notification_limit() {
        let mut limit = NotifyLimit::default();
        let t0 = Instant::now();
        for _ in 0..NOTIFY_MAX {
            assert!(limit.allow("a", t0));
        }
        assert!(!limit.allow("a", t0 + Duration::from_secs(30)), "the sixth within the minute");
        assert!(limit.allow("b", t0), "per agent");
        assert!(limit.allow("a", t0 + NOTIFY_WINDOW), "a minute later");
        assert!(icon_name("dialog-information-symbolic").is_ok());
        for bad in ["/etc/passwd", "../x", "", ".hidden", "a b"] {
            assert!(icon_name(bad).is_err(), "{bad}");
        }
    }
}
