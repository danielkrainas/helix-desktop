//! What agents read about the desktop at the *Read only* level
//! (helixdesktop/spec `contracts/agent-access/README.md`, "Records";
//! helixdesktop/desktop#146): windows, apps, workspaces and monitors as
//! the UI knows them ([`Snapshot`], asked for with
//! [`UiEvent::AgentSnapshot`](crate::UiEvent::AgentSnapshot)), made into
//! the records `Agent1` returns. Excluded apps' windows are left out here
//! (README.md, "Hard exclusions").

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;
use zbus::zvariant::OwnedValue;

use super::policy::{self, Config};
use super::value;

/// A window's workspace when it is on every workspace (as the compositor
/// says it).
pub const ALL_WORKSPACES: u32 = u32::MAX;

/// `helix_window_list_v1` `window_state` bits.
pub const MAXIMIZED: u32 = 1;
pub const FULLSCREEN: u32 = 2;
pub const TILED_LEFT: u32 = 4;
pub const TILED_RIGHT: u32 = 8;

/// An open window.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WindowInfo {
    pub id: u32,
    pub app_id: String,
    pub title: String,
    /// From 0, or [`ALL_WORKSPACES`].
    pub workspace: u32,
    /// x, y, width, height (logical pixels).
    pub geometry: (i32, i32, i32, i32),
    pub minimized: bool,
    /// `window_state` bits.
    pub state: u32,
}

/// A monitor.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MonitorInfo {
    pub connector: String,
    pub name: String,
    /// x, y, width, height (logical pixels).
    pub area: (i32, i32, i32, i32),
    pub scale: f64,
}

/// The windows, workspaces and monitors as the UI knows them now.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    /// Most recently used first.
    pub windows: Vec<WindowInfo>,
    /// 0 for none.
    pub focused: u32,
    /// How many workspaces, and the active one.
    pub workspaces: u32,
    pub active: u32,
    pub monitors: Vec<MonitorInfo>,
    /// The primary monitor's area.
    pub primary: (i32, i32, i32, i32),
}

/// A request for a [`Snapshot`] that can travel in a `UiEvent`.
#[derive(Debug, Clone)]
pub struct SnapshotSlot(Arc<Mutex<Option<oneshot::Sender<Snapshot>>>>);

impl SnapshotSlot {
    pub fn new(reply: oneshot::Sender<Snapshot>) -> Self {
        Self(Arc::new(Mutex::new(Some(reply))))
    }

    pub fn take(&self) -> Option<oneshot::Sender<Snapshot>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

impl PartialEq for SnapshotSlot {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// A record (`a{sv}`).
pub type Record = HashMap<String, OwnedValue>;

/// App ids compared as the contract says: without a `.desktop` suffix,
/// and (X11 classes differ in case) ignoring ASCII case.
pub fn same_app(a: &str, b: &str) -> bool {
    let bare = |s: &str| s.strip_suffix(".desktop").unwrap_or(s).to_owned();
    !a.is_empty() && bare(a).eq_ignore_ascii_case(&bare(b))
}

/// The windows agents may see: every window but excluded apps'.
pub fn visible<'a>(snapshot: &'a Snapshot, config: &Config) -> Vec<&'a WindowInfo> {
    snapshot.windows.iter().filter(|w| !policy::hidden(config, &w.app_id)).collect()
}

/// The `state` of a Window record.
pub fn state_name(w: &WindowInfo) -> &'static str {
    if w.minimized {
        "minimized"
    } else if w.state & FULLSCREEN != 0 {
        "fullscreen"
    } else if w.state & MAXIMIZED != 0 {
        "maximized"
    } else if w.state & TILED_LEFT != 0 {
        "tiled-left"
    } else if w.state & TILED_RIGHT != 0 {
        "tiled-right"
    } else {
        "normal"
    }
}

fn overlap(a: (i32, i32, i32, i32), b: (i32, i32, i32, i32)) -> i64 {
    let w = (a.0 + a.2).min(b.0 + b.2) - a.0.max(b.0);
    let h = (a.1 + a.3).min(b.1 + b.3) - a.1.max(b.1);
    if w > 0 && h > 0 { i64::from(w) * i64::from(h) } else { 0 }
}

/// The monitor showing most of `area` (the primary one when it is on
/// none).
pub fn monitor_of(snapshot: &Snapshot, area: (i32, i32, i32, i32)) -> Option<&MonitorInfo> {
    let best = snapshot.monitors.iter().map(|m| (overlap(m.area, area), m)).filter(|(o, _)| *o > 0).max_by_key(|(o, _)| *o).map(|(_, m)| m);
    best.or_else(|| primary(snapshot)).or(snapshot.monitors.first())
}

/// The primary monitor.
pub fn primary(snapshot: &Snapshot) -> Option<&MonitorInfo> {
    snapshot.monitors.iter().find(|m| (m.area.0, m.area.1) == (snapshot.primary.0, snapshot.primary.1))
}

/// A Window record; `document` when the app tells it.
pub fn window_record(snapshot: &Snapshot, w: &WindowInfo, document: Option<&str>) -> Record {
    let all = w.workspace == ALL_WORKSPACES;
    let (x, y, width, height) = w.geometry;
    let mut r = Record::from([
        ("id".to_owned(), value(w.id)),
        ("app-id".to_owned(), value(w.app_id.as_str())),
        ("title".to_owned(), value(w.title.as_str())),
        ("workspace".to_owned(), value(if all { snapshot.active } else { w.workspace })),
        ("all-workspaces".to_owned(), value(all)),
        ("monitor".to_owned(), value(monitor_of(snapshot, w.geometry).map_or("", |m| m.connector.as_str()))),
        ("focused".to_owned(), value(w.id != 0 && w.id == snapshot.focused)),
        ("x".to_owned(), value(x)),
        ("y".to_owned(), value(y)),
        ("width".to_owned(), value(width)),
        ("height".to_owned(), value(height)),
        ("state".to_owned(), value(state_name(w))),
    ]);
    if let Some(doc) = document.filter(|d| !d.is_empty()) {
        r.insert("document".to_owned(), value(doc));
    }
    r
}

/// What `ListWindows` and `WaitForWindow` match on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Match {
    pub app: Option<String>,
    /// Contained in the title, ignoring case.
    pub title: Option<String>,
    pub focused: bool,
}

impl Match {
    pub fn from_dict(options: &HashMap<String, OwnedValue>) -> Result<Match, super::AgentError> {
        let text = |key: &str| -> Result<Option<String>, super::AgentError> {
            match options.get(key) {
                None => Ok(None),
                Some(v) => String::try_from(v.clone()).map(Some).map_err(|_| super::AgentError::invalid_args(format!("“{key}” must be a string"))),
            }
        };
        let focused = match options.get("focused") {
            None => false,
            Some(v) => bool::try_from(v.clone()).map_err(|_| super::AgentError::invalid_args("“focused” must be a boolean"))?,
        };
        Ok(Match { app: text("app")?.filter(|a| !a.is_empty()), title: text("title")?.filter(|t| !t.is_empty()), focused })
    }

    /// Check the app asked for against the exclusions, and record it (the
    /// title only by its length) in the activity log.
    pub fn record(&self, caller: &super::Caller) -> Result<(), super::AgentError> {
        if let Some(title) = &self.title {
            caller.detail("title-length", super::activity::Detail::Count(title.chars().count() as u64));
        }
        match &self.app {
            Some(app) => caller.check_target(super::Target::App(app)),
            None => Ok(()),
        }
    }

    pub fn matches(&self, snapshot: &Snapshot, w: &WindowInfo) -> bool {
        self.app.as_deref().is_none_or(|a| same_app(&w.app_id, a))
            && self.title.as_deref().is_none_or(|t| w.title.to_lowercase().contains(&t.to_lowercase()))
            && (!self.focused || w.id == snapshot.focused)
    }
}

/// How many of `windows` are on workspace `index` (windows on every
/// workspace count on each).
fn on_workspace(windows: &[&WindowInfo], index: u32) -> u32 {
    windows.iter().filter(|w| w.workspace == index || w.workspace == ALL_WORKSPACES).count() as u32
}

/// The Workspace records; only windows agents may see are counted.
pub fn workspace_records(snapshot: &Snapshot, windows: &[&WindowInfo]) -> Vec<Record> {
    (0..snapshot.workspaces.max(1))
        .map(|i| {
            Record::from([
                ("index".to_owned(), value(i)),
                ("active".to_owned(), value(i == snapshot.active)),
                ("windows".to_owned(), value(on_workspace(windows, i))),
            ])
        })
        .collect()
}

/// The Monitor records.
pub fn monitor_records(snapshot: &Snapshot) -> Vec<Record> {
    let primary = primary(snapshot).map(|m| m.connector.clone());
    snapshot
        .monitors
        .iter()
        .map(|m| {
            Record::from([
                ("connector".to_owned(), value(m.connector.as_str())),
                ("name".to_owned(), value(m.name.as_str())),
                ("x".to_owned(), value(m.area.0)),
                ("y".to_owned(), value(m.area.1)),
                ("width".to_owned(), value(m.area.2)),
                ("height".to_owned(), value(m.area.3)),
                ("scale".to_owned(), value(m.scale)),
                ("primary".to_owned(), value(primary.as_deref() == Some(m.connector.as_str()))),
            ])
        })
        .collect()
}

/// An installed app: its desktop file id without `.desktop`, its name,
/// and the window class it may use (`StartupWMClass`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Installed {
    pub id: String,
    pub name: String,
    pub wm_class: Option<String>,
    /// Its executable's file name.
    pub exec: Option<String>,
}

/// The apps shown in the app grid.
pub fn installed() -> Vec<Installed> {
    use gio::prelude::*;
    gio::AppInfo::all()
        .into_iter()
        .filter(|a| a.should_show())
        .filter_map(|a| {
            let id = a.id()?.to_string();
            let id = id.strip_suffix(".desktop").unwrap_or(&id).to_owned();
            let wm_class = crate::dock::desktop_key(&format!("{id}.desktop"), "StartupWMClass").filter(|c| !c.is_empty());
            let exec = a.executable().file_name().map(|n| n.to_string_lossy().into_owned()).filter(|n| !n.is_empty());
            Some(Installed { id, name: a.name().to_string(), wm_class, exec })
        })
        .collect()
}

/// Whether window `w` belongs to installed app `app`.
pub fn window_of(app: &Installed, w: &WindowInfo) -> bool {
    same_app(&w.app_id, &app.id) || app.wm_class.as_deref().is_some_and(|c| same_app(&w.app_id, c))
}

/// The App records, by name: installed apps (and apps with windows but no
/// desktop file), excluded ones left out; only running ones with
/// `running_only`.
pub fn app_records(apps: &[Installed], windows: &[&WindowInfo], config: &Config, running_only: bool) -> Vec<Record> {
    let mut out: Vec<(String, String, u32)> = Vec::new();
    for app in apps.iter().filter(|a| !policy::hidden(config, &a.id)) {
        let n = windows.iter().filter(|w| window_of(app, w)).count() as u32;
        if !running_only || n > 0 {
            out.push((app.id.clone(), app.name.clone(), n));
        }
    }
    // Windows of apps without a desktop file.
    for w in windows.iter().filter(|w| !w.app_id.is_empty() && !apps.iter().any(|a| window_of(a, w))) {
        match out.iter_mut().find(|(id, ..)| same_app(id, &w.app_id)) {
            Some(entry) => entry.2 += 1,
            None => out.push((w.app_id.clone(), w.app_id.clone(), 1)),
        }
    }
    out.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()).then_with(|| a.0.cmp(&b.0)));
    out.into_iter()
        .map(|(id, name, n)| {
            Record::from([
                ("id".to_owned(), value(id.as_str())),
                ("name".to_owned(), value(name.as_str())),
                ("running".to_owned(), value(n > 0)),
                ("windows".to_owned(), value(n)),
            ])
        })
        .collect()
}

/// A notification in the list, as agents may see it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NoteInfo {
    pub id: u32,
    pub app_id: String,
    pub app_name: String,
    /// When it arrived, Unix seconds.
    pub time: i64,
    pub summary: String,
    /// Plain text.
    pub body: String,
    /// The window it is about (the shell's "… is ready").
    pub window: Option<u32>,
}

/// Whether notification `n` is left out: from an excluded app (by its app
/// id; about one of its windows; or, without an app id, by its name).
pub fn note_hidden(n: &NoteInfo, snapshot: &Snapshot, installed: &[Installed], config: &Config) -> bool {
    if !n.app_id.is_empty() && policy::hidden(config, &n.app_id) {
        return true;
    }
    if n.window.and_then(|id| snapshot.windows.iter().find(|w| w.id == id)).is_some_and(|w| policy::hidden(config, &w.app_id)) {
        return true;
    }
    n.app_id.is_empty() && !n.app_name.is_empty() && installed.iter().any(|a| policy::hidden(config, &a.id) && a.name == n.app_name)
}

/// A Notification record: its title and body only with `contents` (the
/// `notifications` switch), else `redacted`.
pub fn note_record(n: &NoteInfo, contents: bool) -> Record {
    let mut r = Record::from([
        ("id".to_owned(), value(n.id)),
        ("app-id".to_owned(), value(n.app_id.as_str())),
        ("app-name".to_owned(), value(n.app_name.as_str())),
        ("time".to_owned(), value(n.time)),
    ]);
    if contents {
        r.insert("summary".to_owned(), value(n.summary.as_str()));
        r.insert("body".to_owned(), value(n.body.as_str()));
    } else {
        r.insert("redacted".to_owned(), value(true));
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(id: u32, app: &str, title: &str, workspace: u32, geometry: (i32, i32, i32, i32)) -> WindowInfo {
        WindowInfo { id, app_id: app.into(), title: title.into(), workspace, geometry, ..Default::default() }
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            windows: vec![
                win(3, "org.gnome.TextEditor", "notes.txt", 0, (10, 10, 400, 300)),
                win(5, "org.helixdesktop.Settings", "Settings", 0, (0, 0, 800, 600)),
                win(7, "org.keepassxc.KeePassXC", "Passwords", 1, (1300, 0, 400, 300)),
                win(9, "foot", "~", ALL_WORKSPACES, (1500, 100, 300, 200)),
            ],
            focused: 3,
            workspaces: 2,
            active: 0,
            monitors: vec![
                MonitorInfo { connector: "eDP-1".into(), name: "Built-in display".into(), area: (0, 0, 1280, 800), scale: 1.5 },
                MonitorInfo { connector: "HDMI-A-1".into(), name: "Dell".into(), area: (1280, 0, 1920, 1080), scale: 1.0 },
            ],
            primary: (0, 0, 1280, 800),
        }
    }

    fn config() -> Config {
        Config { enabled: true, excluded_apps: vec!["org.keepassxc.KeePassXC.desktop".into()], ..Config::default() }
    }

    fn get<T: TryFrom<OwnedValue>>(r: &Record, key: &str) -> T
    where
        T::Error: std::fmt::Debug,
    {
        T::try_from(r[key].clone()).unwrap()
    }

    #[test]
    fn excluded_windows_are_left_out() {
        let s = snapshot();
        let ids: Vec<u32> = visible(&s, &config()).iter().map(|w| w.id).collect();
        assert_eq!(ids, [3, 9], "Settings and the user's exclusion are hidden");
    }

    #[test]
    fn window_records() {
        let s = snapshot();
        let r = window_record(&s, &s.windows[0], Some("file:///home/u/notes.txt"));
        assert_eq!(get::<u32>(&r, "id"), 3);
        assert_eq!(get::<String>(&r, "app-id"), "org.gnome.TextEditor");
        assert_eq!(get::<String>(&r, "monitor"), "eDP-1");
        assert!(get::<bool>(&r, "focused"));
        assert_eq!((get::<i32>(&r, "x"), get::<i32>(&r, "width")), (10, 400));
        assert_eq!(get::<String>(&r, "state"), "normal");
        assert_eq!(get::<String>(&r, "document"), "file:///home/u/notes.txt");
        // On every workspace: the active one, flagged.
        let r = window_record(&s, &s.windows[3], None);
        assert_eq!((get::<u32>(&r, "workspace"), get::<bool>(&r, "all-workspaces")), (0, true));
        assert_eq!(get::<String>(&r, "monitor"), "HDMI-A-1");
        assert!(!r.contains_key("document"));
        assert!(!get::<bool>(&r, "focused"));
    }

    #[test]
    fn states() {
        let mut w = WindowInfo::default();
        assert_eq!(state_name(&w), "normal");
        w.state = MAXIMIZED;
        assert_eq!(state_name(&w), "maximized");
        w.state = MAXIMIZED | FULLSCREEN;
        assert_eq!(state_name(&w), "fullscreen");
        w.state = TILED_RIGHT;
        assert_eq!(state_name(&w), "tiled-right");
        w.minimized = true;
        assert_eq!(state_name(&w), "minimized");
    }

    #[test]
    fn monitors_and_workspaces() {
        let s = snapshot();
        let m = monitor_records(&s);
        assert_eq!(m.len(), 2);
        assert!(get::<bool>(&m[0], "primary") && !get::<bool>(&m[1], "primary"));
        assert_eq!(get::<f64>(&m[0], "scale"), 1.5);
        // Off every monitor: the primary.
        assert_eq!(monitor_of(&s, (-5000, -5000, 10, 10)).unwrap().connector, "eDP-1");
        let shown = visible(&s, &config());
        let ws = workspace_records(&s, &shown);
        assert_eq!(ws.len(), 2);
        assert_eq!((get::<u32>(&ws[0], "windows"), get::<bool>(&ws[0], "active")), (2, true));
        assert_eq!(get::<u32>(&ws[1], "windows"), 1, "the excluded window is not counted; the sticky one is");
    }

    #[test]
    fn matching() {
        let s = snapshot();
        let m = Match { app: Some("org.gnome.TextEditor.desktop".into()), title: Some("NOTES".into()), focused: true };
        assert!(m.matches(&s, &s.windows[0]));
        assert!(!Match { focused: true, ..Default::default() }.matches(&s, &s.windows[3]));
        assert!(Match::default().matches(&s, &s.windows[3]));
        let opts = HashMap::from([("app".to_owned(), value("foot")), ("focused".to_owned(), value(false))]);
        assert_eq!(Match::from_dict(&opts).unwrap().app.as_deref(), Some("foot"));
        let bad = HashMap::from([("focused".to_owned(), value("yes"))]);
        assert_eq!(Match::from_dict(&bad).unwrap_err().kind(), "InvalidArgs");
    }

    #[test]
    fn apps() {
        let s = snapshot();
        let installed = vec![
            Installed { id: "org.gnome.TextEditor".into(), name: "Text Editor".into(), wm_class: None, exec: None },
            Installed { id: "org.gnome.Calculator".into(), name: "Calculator".into(), wm_class: None, exec: None },
            Installed { id: "org.keepassxc.KeePassXC".into(), name: "KeePassXC".into(), wm_class: None, exec: None },
            Installed { id: "org.helixdesktop.Settings".into(), name: "Settings".into(), wm_class: None, exec: None },
            Installed { id: "terminal".into(), name: "Terminal".into(), wm_class: Some("Foot".into()), exec: None },
        ];
        let shown = visible(&s, &config());
        let all = app_records(&installed, &shown, &config(), false);
        let ids: Vec<String> = all.iter().map(|r| get::<String>(r, "id")).collect();
        assert_eq!(ids, ["org.gnome.Calculator", "terminal", "org.gnome.TextEditor"], "by name, excluded left out");
        assert_eq!(get::<u32>(&all[1], "windows"), 1, "matched by StartupWMClass");
        let running = app_records(&installed, &shown, &config(), true);
        assert_eq!(running.len(), 2);
        // A window without a desktop file is an app of its own.
        let only_editor = &installed[..1];
        let r = app_records(only_editor, &shown, &config(), true);
        assert_eq!(r.iter().map(|r| get::<String>(r, "id")).collect::<Vec<_>>(), ["foot", "org.gnome.TextEditor"]);
    }

    #[test]
    fn notifications() {
        let s = snapshot();
        let installed = vec![Installed { id: "org.keepassxc.KeePassXC".into(), name: "KeePassXC".into(), wm_class: None, exec: None }];
        let note = |app_id: &str, app_name: &str, window: Option<u32>| NoteInfo { id: 1, app_id: app_id.into(), app_name: app_name.into(), time: 99, summary: "Hi".into(), body: "there".into(), window };
        assert!(!note_hidden(&note("org.gnome.Calendar", "Calendar", None), &s, &installed, &config()));
        assert!(note_hidden(&note("org.keepassxc.KeePassXC", "KeePassXC", None), &s, &installed, &config()));
        assert!(note_hidden(&note("org.helixdesktop.Settings", "Settings", None), &s, &installed, &config()));
        assert!(note_hidden(&note("", "Passwords is ready", Some(7)), &s, &installed, &config()), "about an excluded app's window");
        assert!(note_hidden(&note("", "KeePassXC", None), &s, &installed, &config()), "by name without an app id");
        let full = note_record(&note("a", "A", None), true);
        assert_eq!((get::<String>(&full, "summary"), get::<String>(&full, "body")), ("Hi".to_owned(), "there".to_owned()));
        assert!(!full.contains_key("redacted"));
        let redacted = note_record(&note("a", "A", None), false);
        assert!(get::<bool>(&redacted, "redacted"));
        assert!(!redacted.contains_key("summary") && !redacted.contains_key("body"));
        assert_eq!(get::<i64>(&redacted, "time"), 99);
    }
}
