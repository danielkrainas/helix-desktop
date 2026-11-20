//! Agent Access's scoped actions, batch 2, over a private bus
//! (helixdesktop/spec `contracts/agent-access/README.md`, "Changing
//! settings", "Notifications, files and monitors" and "Changes, undo and
//! restoring"; helixdesktop/desktop#157):
//!
//! - `SetAccent`, `SetToggle`, `SetWallpaper`, `SetPowerMode`, `StarFile`
//!   and `SetFavorite` return Change records (`SetToggle` nothing) and
//!   log old and new values; a call that finds the value set changes
//!   nothing;
//! - temporary changes (`until`) go back on their timer, on *Stop*, on
//!   Settings' `Revoke`, when the level is lowered, when the shell stops
//!   and at the next start after a crash; a restore is skipped when the
//!   user changed the setting since (`changed`) or the old value is gone
//!   (`unavailable`);
//! - Settings' `ListChanges`, `Undo` and `ChangesChanged`;
//! - the indicator's rows of pending restores;
//! - the background's file rules (hidden folders, symlinks, not a
//!   picture) and the copy;
//! - the rate limit;
//! - notifications from a fake app with buttons: `actions` only with the
//!   `notifications` switch, `NotificationAction` and its sensitive
//!   buttons with `confirm-sensitive`, `DismissNotification`, excluded
//!   apps' notifications not there;
//! - `RevealFile` with a fake file manager, `WindowAction` `to-monitor`.
//!
//! Nothing on the host is touched: scratch XDG directories, in-memory
//! settings, private buses for the session and the system (fake
//! power-profiles-daemon).

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use gio::prelude::*;
use helix_shell::agent::actions::WindowOp;
use helix_shell::agent::desktop::{MonitorInfo, Snapshot, WindowInfo};
use helix_shell::agent::grants::Answer;
use helix_shell::agent::prompt::Active;
use helix_shell::agent::{self, Agent1, AgentAccess, AgentAccessSettings, Confirm, Desktop, Setup, OBJECT_PATH};
use helix_shell::notifications::{Center, FdoNotifications};
use helix_shell::screensaver::SharedStatus;
use helix_shell::windows::WindowAction;
use helix_shell::UiEvent;
use helix_test_support::conformance::{assert_conformant, check, introspect, load_dbus_contract};
use helix_test_support::upower;
use helix_test_support::TestBus;
use serde_json::Value as Json;
use zbus::zvariant::{OwnedValue, Value};
use zbus::Connection;

const IFACE: &str = "org.helixdesktop.Agent1";
const SETTINGS_IFACE: &str = "org.helixdesktop.AgentAccess1";

type Record = HashMap<String, OwnedValue>;
type Opts = HashMap<&'static str, Value<'static>>;

/// What the sensitive-action prompt was asked: method, action, the
/// control's name, until.
type Asked = Arc<Mutex<Vec<(String, Option<String>, Option<String>, Option<i64>)>>>;

#[derive(Clone, Default)]
struct FakeConfirm {
    answers: Arc<Mutex<VecDeque<Option<bool>>>>,
    asked: Asked,
}

impl FakeConfirm {
    fn hook(&self) -> Confirm {
        let me = self.clone();
        Arc::new(move |r| {
            me.asked.lock().unwrap().push((r.method.to_owned(), r.action.map(str::to_owned), r.control.as_ref().map(|c| c.name.clone()), r.until));
            let answer = me.answers.lock().unwrap().pop_front().flatten();
            Box::pin(async move { answer })
        })
    }

    fn queue(&self, answer: Option<bool>) {
        self.answers.lock().unwrap().push_back(answer);
    }

    fn count(&self) -> usize {
        self.asked.lock().unwrap().len()
    }

    fn last(&self) -> (String, Option<String>, Option<String>, Option<i64>) {
        self.asked.lock().unwrap().last().cloned().unwrap()
    }
}

fn short(result: &zbus::Result<zbus::Message>) -> String {
    match result {
        Err(zbus::Error::MethodError(name, _, _)) => name.as_str().trim_start_matches("org.helixdesktop.Agent1.Error.").to_owned(),
        Err(other) => format!("{other:?}"),
        Ok(_) => "ok".to_owned(),
    }
}

fn message(result: &zbus::Result<zbus::Message>) -> String {
    match result {
        Err(zbus::Error::MethodError(_, Some(m), _)) => m.clone(),
        other => format!("{other:?}"),
    }
}

async fn call(conn: &Connection, method: &str, body: &(impl serde::Serialize + zbus::zvariant::DynamicType)) -> zbus::Result<zbus::Message> {
    conn.call_method(Some("org.gnome.Shell"), OBJECT_PATH, Some(IFACE), method, body).await
}

async fn settings_call(conn: &Connection, method: &str, body: &(impl serde::Serialize + zbus::zvariant::DynamicType)) -> zbus::Result<zbus::Message> {
    conn.call_method(Some("org.gnome.Shell"), OBJECT_PATH, Some(SETTINGS_IFACE), method, body).await
}

/// A method's Change record.
async fn change(conn: &Connection, method: &str, body: &(impl serde::Serialize + zbus::zvariant::DynamicType)) -> Record {
    let reply = call(conn, method, body).await.unwrap_or_else(|e| panic!("{method}: {e:?}"));
    reply.body().deserialize().unwrap()
}

async fn list_changes(conn: &Connection) -> Vec<Record> {
    settings_call(conn, "ListChanges", &()).await.unwrap().body().deserialize().unwrap()
}

fn opts(pairs: &[(&'static str, Value<'static>)]) -> Opts {
    pairs.iter().cloned().collect()
}

fn none() -> Opts {
    opts(&[])
}

fn until(secs: i64) -> Opts {
    opts(&[("until", Value::from(now() + secs))])
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

fn s(r: &Record, key: &str) -> String {
    String::try_from(r[key].clone()).unwrap_or_else(|_| panic!("{key} in {r:?}"))
}

fn b(r: &Record, key: &str) -> bool {
    bool::try_from(r[key].clone()).unwrap_or_else(|_| panic!("{key} in {r:?}"))
}

fn x(r: &Record, key: &str) -> i64 {
    i64::try_from(r[key].clone()).unwrap_or_else(|_| panic!("{key} in {r:?}"))
}

fn desktop_file(dir: &Path, id: &str, name: &str, exec: &str) {
    std::fs::write(dir.join(format!("{id}.desktop")), format!("[Desktop Entry]\nType=Application\nName={name}\nExec={exec}\n")).unwrap();
}

/// The activity log's lines.
fn activity(dir: &Path) -> Vec<Json> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let text = std::fs::read_to_string(entry.path()).unwrap_or_default();
        out.extend(text.lines().filter_map(|l| serde_json::from_str::<Json>(l).ok()));
    }
    out
}

/// The last log line for which `f` holds, waiting a little (an allowed
/// call is written when it ends).
async fn logged(dir: &Path, what: &str, f: impl Fn(&Json) -> bool) -> Json {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(line) = activity(dir).into_iter().rev().find(|l| f(l)) {
            return line;
        }
        assert!(Instant::now() < deadline, "{what}: not in the log:\n{:#?}", activity(dir));
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Wait (at most `secs`) until `f` holds.
async fn eventually(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !f() {
        assert!(Instant::now() < deadline, "{what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn accent() -> String {
    gio::Settings::new("org.gnome.desktop.interface").string("accent-color").to_string()
}

/// A tiny PNG.
fn png() -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, 2, 2);
        enc.set_color(png::ColorType::Rgba);
        let mut w = enc.write_header().unwrap();
        w.write_image_data(&[200; 16]).unwrap();
    }
    out
}

/// Move window `id` to the monitor next to its own the way `action` says,
/// as the compositor's Super+Shift+arrow does.
fn move_to_monitor(s: &mut Snapshot, id: u32, action: WindowAction) {
    let monitors = s.monitors.clone();
    let Some(w) = s.windows.iter_mut().find(|w| w.id == id) else { return };
    let (x, y, _, _) = w.geometry;
    let Some(from) = monitors.iter().find(|m| x >= m.area.0 && x < m.area.0 + m.area.2 && y >= m.area.1 && y < m.area.1 + m.area.3) else { return };
    let next = monitors.iter().find(|m| match action {
        WindowAction::MoveToMonitorRight => m.area.0 == from.area.0 + from.area.2,
        WindowAction::MoveToMonitorLeft => m.area.0 + m.area.2 == from.area.0,
        WindowAction::MoveToMonitorDown => m.area.1 == from.area.1 + from.area.3,
        WindowAction::MoveToMonitorUp => m.area.1 + m.area.3 == from.area.1,
        _ => false,
    });
    if let Some(to) = next {
        w.geometry.0 += to.area.0 - from.area.0;
        w.geometry.1 += to.area.1 - from.area.1;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn changes_over_the_bus() {
    let tmp = std::env::temp_dir().join(format!("helix-agent-changes-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    // A session bus that starts only what this test puts in `services`
    // (never the host's file manager).
    let services = tmp.join("services");
    std::fs::create_dir_all(&services).unwrap();
    let Some(bus) = TestBus::start_with_services(&services) else { return };
    let Some(system) = TestBus::start() else { return };
    let schemas = tmp.join("schemas");
    std::fs::create_dir_all(&schemas).unwrap();
    let data = concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/org.helixdesktop.agent-access.gschema.xml");
    std::fs::copy(data, schemas.join("org.helixdesktop.agent-access.gschema.xml")).unwrap();
    // GNOME's schemas, where the host has them (read only).
    let gnome = [
        "org.gnome.desktop.interface.gschema.xml",
        "org.gnome.desktop.background.gschema.xml",
        "org.gnome.desktop.notifications.gschema.xml",
        "org.gnome.settings-daemon.plugins.color.gschema.xml",
        "org.gnome.shell.gschema.xml",
        "org.gnome.desktop.enums.xml",
        "org.gnome.settings-daemon.enums.xml",
    ];
    for file in gnome {
        if std::fs::copy(Path::new("/usr/share/glib-2.0/schemas").join(file), schemas.join(file)).is_err() {
            eprintln!("GNOME's schemas are not installed ({file}); skipping");
            return;
        }
    }
    let Ok(compiled) = std::process::Command::new("glib-compile-schemas").arg(&schemas).status() else {
        eprintln!("glib-compile-schemas is not installed; skipping");
        return;
    };
    if !compiled.success() {
        eprintln!("GNOME's schemas do not compile alone here; skipping");
        return;
    }
    let home = tmp.join("home");
    let data_home = tmp.join("data");
    let apps = data_home.join("applications");
    let sys = tmp.join("sys");
    for dir in [&apps, &sys.join("backgrounds"), &home.join("Pictures"), &home.join("Documents"), &home.join(".config/app"), &tmp.join("config"), &tmp.join("run")] {
        std::fs::create_dir_all(dir).unwrap();
    }
    if Path::new("/usr/share/mime/mime.cache").exists() {
        std::os::unix::fs::symlink("/usr/share/mime", sys.join("mime")).unwrap();
    }
    desktop_file(&apps, "org.gnome.TextEditor", "Text Editor", "true %U");
    desktop_file(&apps, "org.gnome.Calculator", "Calculator", "true");
    desktop_file(&apps, "org.example.Vault", "Secret Vault", "true");
    desktop_file(&apps, "org.helixdesktop.Settings", "Settings", "helix-settings-app");
    // The default app for folders: an excluded one (for RevealFile).
    std::fs::write(tmp.join("config/mimeapps.list"), "[Default Applications]\ninode/directory=org.example.Vault.desktop\n").unwrap();
    // SAFETY: this test binary runs this one test, before any GLib use.
    unsafe {
        std::env::set_var("GSETTINGS_SCHEMA_DIR", &schemas);
        std::env::set_var("GSETTINGS_BACKEND", "memory");
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_CONFIG_HOME", tmp.join("config"));
        std::env::set_var("XDG_STATE_HOME", tmp.join("state"));
        std::env::set_var("XDG_CACHE_HOME", tmp.join("cache"));
        std::env::set_var("XDG_DATA_HOME", &data_home);
        std::env::set_var("XDG_DATA_DIRS", &sys);
        std::env::set_var("XDG_CONFIG_DIRS", tmp.join("sysconfig"));
        std::env::set_var("XDG_RUNTIME_DIR", tmp.join("run"));
        std::env::set_var("GIO_USE_VFS", "local");
        std::env::set_var("PULSE_SERVER", format!("unix:{}", tmp.join("no-pulse").display()));
        // The stand-in system bus: a fake power-profiles-daemon.
        std::env::set_var("DBUS_SYSTEM_BUS_ADDRESS", system.address());
        std::env::set_var("DBUS_SESSION_BUS_ADDRESS", bus.address());
    }
    let mut ppd = upower::serve(&system, None, Some(("balanced", &["power-saver", "balanced", "performance"]))).await;
    let system_conn = system.connect().await;

    // The UI: snapshots (three monitors side by side and above), window
    // requests done to them, and the indicator's agents.
    let monitor = |c: &str, area| MonitorInfo { connector: c.into(), name: c.into(), area, scale: 1.0 };
    let snapshot = Arc::new(Mutex::new(Snapshot {
        windows: vec![
            WindowInfo { id: 11, app_id: "org.gnome.TextEditor".into(), title: "notes".into(), geometry: (100, 100, 400, 300), ..Default::default() },
            WindowInfo { id: 12, app_id: "org.example.Vault".into(), title: "vault".into(), geometry: (200, 200, 400, 300), ..Default::default() },
        ],
        focused: 11,
        workspaces: 1,
        monitors: vec![monitor("eDP-1", (0, 0, 1920, 1080)), monitor("HDMI-1", (1920, 0, 1920, 1080)), monitor("DP-2", (3840, 0, 1280, 1024))],
        primary: (0, 0, 1920, 1080),
        ..Default::default()
    }));
    let indicator: Arc<Mutex<Vec<Active>>> = Default::default();
    let (ui, mut ui_rx) = tokio::sync::mpsc::unbounded_channel::<UiEvent>();
    let (shown, shown_agents) = (snapshot.clone(), indicator.clone());
    tokio::spawn(async move {
        while let Some(event) = ui_rx.recv().await {
            match event {
                UiEvent::AgentSnapshot(slot) => {
                    if let Some(reply) = slot.take() {
                        let _ = reply.send(shown.lock().unwrap().clone());
                    }
                }
                UiEvent::AgentWindow(slot) => {
                    if let Some(request) = slot.take() {
                        let mut s = shown.lock().unwrap();
                        for op in &request.ops {
                            if let WindowOp::Action(id, action) = op {
                                move_to_monitor(&mut s, *id, *action);
                            }
                        }
                        let _ = request.reply.send(Ok(()));
                    }
                }
                UiEvent::AgentsActive(agents) => *shown_agents.lock().unwrap() = agents,
                _ => {}
            }
        }
    });

    let settings = gio::Settings::new("org.helixdesktop.agent-access");
    let lock: SharedStatus = Default::default();
    let center = Center::new(ui.clone(), lock.clone());
    let consent: agent::Consent = Arc::new(|_| Box::pin(async { Answer::Session }));
    let confirm = FakeConfirm::default();
    let me = std::env::current_exe().unwrap();
    let agent_id = me.to_string_lossy().into_owned();
    let log = tmp.join("state/helix/agent-activity");
    let changes_path = tmp.join("state/helix/agent-changes.json");
    let setup = |consent: agent::Consent, confirm: Confirm| Setup {
        grants_path: None,
        consent,
        confirm,
        proc_root: "/proc".into(),
        settings_exes: vec![me.clone()],
        log_dir: Some(log.clone()),
        reminders_path: None,
        changes_path: Some(changes_path.clone()),
    };
    let access = AgentAccess::new(lock.clone(), setup(consent.clone(), confirm.hook()));
    access.set_desktop(Desktop { ui: ui.clone(), notifications: Some(center.clone()) });
    access.set_indicator(ui.clone());
    let server = bus
        .builder()
        .serve_at(OBJECT_PATH, Agent1 { access: access.clone() })
        .unwrap()
        .serve_at(OBJECT_PATH, AgentAccessSettings { access: access.clone() })
        .unwrap()
        .name("org.gnome.Shell")
        .unwrap()
        .build()
        .await
        .unwrap();
    access.attach(&server);
    let notifications = bus
        .builder()
        .serve_at("/org/freedesktop/Notifications", FdoNotifications { center: center.clone() })
        .unwrap()
        .name("org.freedesktop.Notifications")
        .unwrap()
        .build()
        .await
        .unwrap();
    access.set_public(&notifications);
    let conn = bus.connect().await;
    // Settings' signal.
    let changes_changed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    {
        let rule = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).interface(SETTINGS_IFACE).unwrap().member("ChangesChanged").unwrap().build();
        let mut stream = zbus::MessageStream::for_match_rule(rule, &conn, None).await.unwrap();
        let counted = changes_changed.clone();
        tokio::spawn(async move {
            while stream.next().await.is_some() {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });
    }
    let signals = || changes_changed.load(std::sync::atomic::Ordering::SeqCst);

    // Every member is there, as the contracts have them.
    let actual = introspect(&conn, "org.gnome.Shell", OBJECT_PATH).await;
    assert_conformant(IFACE, &check(IFACE, &load_dbus_contract("../agent-access/org.helixdesktop.Agent1.xml"), &actual, &[]));
    assert_conformant(SETTINGS_IFACE, &check(SETTINGS_IFACE, &load_dbus_contract("../agent-access/org.helixdesktop.AgentAccess1.xml"), &actual, &[]));

    // Off: refused, arguments not looked at.
    for (method, result) in [
        ("SetAccent", call(&conn, "SetAccent", &("nonsense", none())).await),
        ("SetWallpaper", call(&conn, "SetWallpaper", &("http://x", none())).await),
        ("SetPowerMode", call(&conn, "SetPowerMode", &("x", none())).await),
        ("StarFile", call(&conn, "StarFile", &("/nowhere", true, none())).await),
        ("SetFavorite", call(&conn, "SetFavorite", &("x", true, none())).await),
        ("RevealFile", call(&conn, "RevealFile", &("/nowhere", none())).await),
        ("DismissNotification", call(&conn, "DismissNotification", &(1u32, none())).await),
        ("NotificationAction", call(&conn, "NotificationAction", &(1u32, "x", none())).await),
    ] {
        assert_eq!(short(&result), "AccessOff", "{method}");
    }
    settings.set_boolean("enabled", true).unwrap();
    settings.set_boolean("allow-remote", true).unwrap();
    settings.set_string("access-level", "desktop").unwrap();
    settings.set_strv("excluded-apps", ["org.example.Vault.desktop"]).unwrap();

    // ----- The accent color: a change, nothing changed, malformed ----------
    assert_eq!(accent(), "blue");
    let before = signals();
    let red = change(&conn, "SetAccent", &("red", none())).await;
    assert!(b(&red, "changed"));
    assert_eq!((s(&red, "name"), s(&red, "old"), s(&red, "new")), ("accent".into(), "blue".into(), "red".into()));
    assert_eq!(s(&red, "id").len(), 16);
    assert!(!red.contains_key("until") && !red.contains_key("target"));
    assert_eq!(accent(), "red");
    let state: Record = call(&conn, "GetState", &(none(),)).await.unwrap().body().deserialize().unwrap();
    assert_eq!(s(&state, "accent"), "red", "the State record's accent");
    let line = logged(&log, "SetAccent", |l| l["method"] == "SetAccent" && l["change"] == s(&red, "id")).await;
    assert_eq!((line["target"].clone(), line["old"].clone(), line["new"].clone(), line["outcome"].clone()), ("accent".into(), "blue".into(), "red".into(), "allowed".into()));
    assert!(line.get("restore-at").is_none());
    eventually("ChangesChanged after a change", 5, || signals() > before).await;
    let again = change(&conn, "SetAccent", &("red", none())).await;
    assert!(!b(&again, "changed") && !again.contains_key("id"));
    assert_eq!((s(&again, "old"), s(&again, "new")), ("red".into(), "red".into()));
    let line = logged(&log, "unchanged", |l| l["method"] == "SetAccent" && l["old"] == "red" && l["new"] == "red").await;
    assert!(line.get("change").is_none());
    let bad = call(&conn, "SetAccent", &("magenta", none())).await;
    assert_eq!(short(&bad), "NotSupported");
    for secs in [5, 25 * 3600, -100] {
        let r = call(&conn, "SetAccent", &("teal", until(secs))).await;
        assert_eq!(short(&r), "org.freedesktop.DBus.Error.InvalidArgs", "until {secs}: {}", message(&r));
    }
    let r = call(&conn, "SetAccent", &("teal", opts(&[("until", Value::from("soon"))]))).await;
    assert_eq!(short(&r), "org.freedesktop.DBus.Error.InvalidArgs");
    assert_eq!(accent(), "red", "nothing changed by refused calls");

    // ----- Temporary: listed, in the indicator, back on its timer -----------
    let green = change(&conn, "SetAccent", &("green", until(10))).await;
    let id = s(&green, "id");
    let at = x(&green, "until");
    assert!((now() + 9..=now() + 10).contains(&at));
    let line = logged(&log, "restore-at", |l| l["change"] == id.as_str()).await;
    assert_eq!(line["restore-at"], at);
    let listed = list_changes(&conn).await;
    assert_eq!(s(&listed[0], "id"), id, "newest first: {listed:?}");
    assert_eq!((s(&listed[0], "setting"), s(&listed[0], "old"), s(&listed[0], "new"), x(&listed[0], "until")), ("accent".into(), "red".into(), "green".into(), at));
    assert_eq!((s(&listed[0], "agent"), s(&listed[0], "executable")), (agent_id.clone(), agent_id.clone()));
    // The earlier change of the setting is superseded.
    assert!(!listed.iter().any(|r| s(r, "id") == s(&red, "id")));
    eventually("the indicator shows the pending restore", 10, || indicator.lock().unwrap().iter().any(|a| a.agent == agent_id && a.restores.iter().any(|r| r.id == id))).await;
    let row = indicator.lock().unwrap().iter().flat_map(|a| a.restores.clone()).find(|r| r.id == id).unwrap();
    assert_eq!(helix_shell::agent::changes::row_label(&row, now()), "Accent Color back to Red in less than a minute");
    eventually("put back on its timer", 20, || accent() == "red").await;
    let line = logged(&log, "the restore", |l| l["method"] == "Restore" && l["change"] == id.as_str()).await;
    assert_eq!((line["old"].clone(), line["new"].clone(), line["cause"].clone(), line["outcome"].clone(), line["target"].clone()), ("green".into(), "red".into(), "expired".into(), "allowed".into(), "accent".into()));
    assert!(line.get("level").is_none());
    assert!(list_changes(&conn).await.iter().all(|r| s(r, "id") != id), "restored: not listed");
    eventually("the indicator's row goes", 10, || !indicator.lock().unwrap().iter().any(|a| a.restores.iter().any(|r| r.id == id))).await;
    let r = settings_call(&conn, "Undo", &(id.as_str(),)).await;
    assert_eq!(short(&r), "NotFound", "already restored");

    // ----- A newer temporary change keeps the user's value -----------------
    let one = change(&conn, "SetAccent", &("teal", until(3600))).await;
    let two = change(&conn, "SetAccent", &("pink", until(1800))).await;
    assert_eq!(s(&two, "old"), "teal", "the Change record says what it replaced");
    assert_eq!(short(&settings_call(&conn, "Undo", &(s(&one, "id").as_str(),)).await), "NotFound", "superseded");
    access.restore(&s(&two, "id"), helix_shell::agent::changes::Cause::RestoreNow).await.unwrap();
    assert_eq!(accent(), "red", "the first change's old value: the user's");
    let line = logged(&log, "restore now", |l| l["method"] == "Restore" && l["change"] == s(&two, "id")).await;
    assert_eq!((line["new"].clone(), line["cause"].clone()), ("red".into(), "restore-now".into()));

    // ----- The user changed it since: skipped -------------------------------
    let purple = change(&conn, "SetAccent", &("purple", until(3600))).await;
    gio::Settings::new("org.gnome.desktop.interface").set_string("accent-color", "orange").unwrap();
    assert!(list_changes(&conn).await.iter().all(|r| s(r, "id") != s(&purple, "id")), "not undoable once the user changed it");
    let r = settings_call(&conn, "Undo", &(s(&purple, "id").as_str(),)).await;
    assert_eq!((short(&r), message(&r)), ("Failed".into(), "changed".into()));
    access.stop(&agent_id);
    let line = logged(&log, "skipped", |l| l["method"] == "Restore" && l["change"] == s(&purple, "id")).await;
    assert_eq!((line["outcome"].clone(), line["reason"].clone(), line["cause"].clone()), ("cancelled".into(), "changed".into(), "stopped".into()));
    assert_eq!(accent(), "orange", "the user's later choice wins");
    let line = logged(&log, "Undo refused", |l| l["method"] == "Undo" && l["change"] == s(&purple, "id")).await;
    assert_eq!((line["error"].clone(), line["agent"].clone()), ("Failed".into(), Json::from(agent_id.clone())));

    // ----- Quick settings with until: restored on Stop ----------------------
    // Marked sensitive: the prompt says until when.
    settings.set_boolean("confirm-sensitive", true).unwrap();
    confirm.queue(Some(true));
    let at = now() + 3600;
    call(&conn, "SetToggle", &("dark-style", true, opts(&[("until", Value::from(at)), ("sensitive", Value::from(true))]))).await.unwrap();
    assert_eq!(confirm.last(), ("SetToggle".into(), None, None, Some(at)));
    settings.set_boolean("confirm-sensitive", false).unwrap();
    let interface = gio::Settings::new("org.gnome.desktop.interface");
    assert_eq!(interface.string("color-scheme"), "prefer-dark");
    let line = logged(&log, "SetToggle", |l| l["method"] == "SetToggle" && l["target"] == "dark-style" && l.get("change").is_some()).await;
    assert_eq!((line["state"].clone(), line["old"].clone(), line["new"].clone(), line["restore-at"].clone()), ("on".into(), "off".into(), "on".into(), Json::from(at)));
    let toggle_change = line["change"].as_str().unwrap().to_owned();
    assert!(list_changes(&conn).await.iter().any(|r| s(r, "id") == toggle_change && s(r, "setting") == "dark-style"));
    // Already on: nothing changed, the pending restore stays.
    call(&conn, "SetToggle", &("dark-style", true, until(60))).await.unwrap();
    assert!(list_changes(&conn).await.iter().any(|r| s(r, "id") == toggle_change && x(r, "until") == at));
    access.stop(&agent_id);
    eventually("Stop puts it back", 5, || interface.string("color-scheme") == "default").await;
    let line = logged(&log, "Stop's restore", |l| l["method"] == "Restore" && l["change"] == toggle_change.as_str()).await;
    assert_eq!((line["cause"].clone(), line["old"].clone(), line["new"].clone()), ("stopped".into(), "on".into(), "off".into()));
    logged(&log, "Stop", |l| l["method"] == "Stop").await;

    // ----- Power mode: Settings' Revoke puts it back --------------------------
    let fast = change(&conn, "SetPowerMode", &("performance", until(3600))).await;
    assert_eq!((s(&fast, "name"), s(&fast, "old"), s(&fast, "new")), ("power-mode".into(), "balanced".into(), "performance".into()));
    assert_eq!(upower::active_profile(&system_conn).await.as_deref(), Some("performance"));
    assert_eq!(short(&call(&conn, "SetPowerMode", &("turbo", none())).await), "NotSupported");
    settings_call(&conn, "Revoke", &(agent_id.as_str(),)).await.unwrap();
    eventually_async("Revoke puts it back", || async { upower::active_profile(&system_conn).await.as_deref() == Some("balanced") }).await;
    let line = logged(&log, "Revoke's restore", |l| l["method"] == "Restore" && l["change"] == s(&fast, "id")).await;
    assert_eq!(line["cause"], "stopped");

    // ----- Lowering the level puts it back -----------------------------------
    let saver = change(&conn, "SetPowerMode", &("power-saver", until(3600))).await;
    settings.set_string("access-level", "read-only").unwrap();
    access.settings_changed(&helix_shell::agent::Config::read());
    eventually_async("a lower level puts it back", || async { upower::active_profile(&system_conn).await.as_deref() == Some("balanced") }).await;
    logged(&log, "the level's restore", |l| l["method"] == "Restore" && l["change"] == s(&saver, "id") && l["cause"] == "stopped").await;
    settings.set_string("access-level", "desktop").unwrap();

    // ----- Undo from Settings; unavailable ----------------------------------
    let lasting = change(&conn, "SetPowerMode", &("performance", none())).await;
    let listed = list_changes(&conn).await;
    let entry = listed.iter().find(|r| s(r, "id") == s(&lasting, "id")).expect("listed");
    assert!(!entry.contains_key("until"));
    settings_call(&conn, "Undo", &(s(&lasting, "id").as_str(),)).await.unwrap();
    assert_eq!(upower::active_profile(&system_conn).await.as_deref(), Some("balanced"));
    let line = logged(&log, "Undo", |l| l["method"] == "Undo" && l["change"] == s(&lasting, "id")).await;
    assert_eq!((line["target"].clone(), line["old"].clone(), line["new"].clone(), line["outcome"].clone(), line["agent"].clone()), ("power-mode".into(), "performance".into(), "balanced".into(), "allowed".into(), Json::from(agent_id.clone())));
    assert!(line.get("error").is_none() && line.get("level").is_none());
    assert_eq!(short(&settings_call(&conn, "Undo", &(s(&lasting, "id").as_str(),)).await), "NotFound", "already undone");
    assert_eq!(short(&settings_call(&conn, "Undo", &("0000000000000000",)).await), "NotFound");
    // The old profile no longer offered: unavailable, nothing done.
    let again = change(&conn, "SetPowerMode", &("performance", none())).await;
    ppd = reserve(&system, &system_conn, ppd, ("performance", &["power-saver", "performance"])).await;
    let r = settings_call(&conn, "Undo", &(s(&again, "id").as_str(),)).await;
    assert_eq!((short(&r), message(&r)), ("Failed".into(), "unavailable".into()));
    assert_eq!(upower::active_profile(&system_conn).await.as_deref(), Some("performance"));
    // And a pending restore of a gone profile is skipped as unavailable.
    let saver = change(&conn, "SetPowerMode", &("power-saver", until(3600))).await;
    ppd = reserve(&system, &system_conn, ppd, ("power-saver", &["power-saver"])).await;
    access.stop(&agent_id);
    let line = logged(&log, "unavailable", |l| l["method"] == "Restore" && l["change"] == s(&saver, "id")).await;
    assert_eq!((line["outcome"].clone(), line["reason"].clone()), ("cancelled".into(), "unavailable".into()));
    let _ppd = reserve(&system, &system_conn, ppd, ("balanced", &["power-saver", "balanced", "performance"])).await;

    // ----- The background ------------------------------------------------------
    let background = gio::Settings::new("org.gnome.desktop.background");
    background.set_string("picture-uri", "").unwrap();
    background.set_string("picture-uri-dark", "").unwrap();
    background.set_string("picture-options", "none").unwrap();
    let sea = home.join("Pictures/sea.png");
    std::fs::write(&sea, png()).unwrap();
    let set = change(&conn, "SetWallpaper", &(sea.to_str().unwrap(), none())).await;
    let copy_uri = s(&set, "new");
    assert!(copy_uri.starts_with(&format!("file://{}/backgrounds/", data_home.display())) && copy_uri.ends_with("-sea.png"), "{copy_uri}");
    assert_eq!((s(&set, "old"), s(&set, "new-dark"), s(&set, "old-fit"), s(&set, "new-fit")), (String::new(), copy_uri.clone(), "none".into(), "zoom".into()));
    assert_eq!((background.string("picture-uri").to_string(), background.string("picture-uri-dark").to_string(), background.string("picture-options").to_string()), (copy_uri.clone(), copy_uri.clone(), "zoom".into()));
    let copy = gio::File::for_uri(&copy_uri).path().unwrap();
    assert_eq!(std::fs::read(&copy).unwrap(), png());
    // Moving the original does not change the background.
    std::fs::remove_file(&sea).unwrap();
    assert!(copy.exists());
    let line = logged(&log, "SetWallpaper", |l| l["method"] == "SetWallpaper" && l["change"] == s(&set, "id")).await;
    assert_eq!((line["variant"].clone(), line["target"].clone(), line["new-fit"].clone()), ("both".into(), "wallpaper".into(), "zoom".into()));
    // Light only, from a file:// URI, with a fit and until.
    let dune = home.join("Pictures/dune 1.png");
    std::fs::write(&dune, png()).unwrap();
    let dune_uri = gio::File::for_path(&dune).uri().to_string();
    let light = change(&conn, "SetWallpaper", &(dune_uri.as_str(), opts(&[("variant", Value::from("light")), ("fit", Value::from("centered")), ("until", Value::from(now() + 3600))]))).await;
    assert_eq!((s(&light, "old"), s(&light, "old-dark"), s(&light, "new-dark")), (copy_uri.clone(), copy_uri.clone(), copy_uri.clone()));
    assert_eq!(background.string("picture-uri-dark").to_string(), copy_uri, "dark untouched");
    assert_eq!(background.string("picture-options").to_string(), "centered");
    // Stop: put back (the first picture still there).
    access.stop(&agent_id);
    eventually("the background back", 5, || background.string("picture-uri") == copy_uri.as_str() && background.string("picture-options") == "zoom").await;
    // The old picture gone: unavailable.
    let third = change(&conn, "SetWallpaper", &(dune.to_str().unwrap(), until(3600))).await;
    std::fs::remove_file(&copy).unwrap();
    access.stop(&agent_id);
    let line = logged(&log, "picture gone", |l| l["method"] == "Restore" && l["change"] == s(&third, "id")).await;
    assert_eq!(line["reason"], "unavailable");
    // Where pictures may not come from: hidden folders, through a symlink;
    // not pictures; not local files; not there.
    let secret = home.join(".config/app/secret.png");
    std::fs::write(&secret, png()).unwrap();
    std::os::unix::fs::symlink(&secret, home.join("Pictures/innocent.png")).unwrap();
    for (path, err) in [
        (secret.to_string_lossy().into_owned(), "Excluded"),
        (home.join("Pictures/innocent.png").to_string_lossy().into_owned(), "Excluded"),
        ("/etc/hostname".to_owned(), "Excluded"),
        (home.join("Pictures/missing.png").to_string_lossy().into_owned(), "NotFound"),
        ("https://example.org/a.png".to_owned(), "NotSupported"),
        ("pictures/relative.png".to_owned(), "org.freedesktop.DBus.Error.InvalidArgs"),
    ] {
        let r = call(&conn, "SetWallpaper", &(path.as_str(), none())).await;
        assert_eq!(short(&r), err, "{path}: {}", message(&r));
    }
    std::fs::write(home.join("Pictures/notes.png"), b"not a picture").unwrap();
    let r = call(&conn, "SetWallpaper", &(home.join("Pictures/notes.png").to_str().unwrap(), none())).await;
    assert_eq!((short(&r), message(&r)), ("NotSupported".into(), "not a picture".into()));
    let r = call(&conn, "SetWallpaper", &(dune.to_str().unwrap(), opts(&[("variant", Value::from("sideways"))]))).await;
    assert_eq!(short(&r), "org.freedesktop.DBus.Error.InvalidArgs");
    let line = logged(&log, "private-file", |l| l["method"] == "SetWallpaper" && l["reason"] == "private-file").await;
    assert_eq!((line["outcome"].clone(), line["variant"].clone()), ("refused:Excluded".into(), "both".into()));
    // A picture in /tmp of the user's.
    let tmp_pic = Path::new("/tmp").join(format!("helix-agent-changes-{}.png", std::process::id()));
    std::fs::write(&tmp_pic, png()).unwrap();
    assert!(b(&change(&conn, "SetWallpaper", &(tmp_pic.to_str().unwrap(), none())).await, "changed"));
    let _ = std::fs::remove_file(&tmp_pic);

    // ----- The dock's favorites, and the rate limit -------------------------
    let shell = gio::Settings::new("org.gnome.shell");
    shell.set_strv("favorite-apps", ["org.gnome.Calculator.desktop"]).unwrap();
    let pinned = change(&conn, "SetFavorite", &("org.gnome.TextEditor.desktop", true, none())).await;
    assert_eq!((s(&pinned, "name"), s(&pinned, "target"), b(&pinned, "old"), b(&pinned, "new")), ("favorite".into(), "org.gnome.TextEditor".into(), false, true));
    assert_eq!(favorites(&shell), ["org.gnome.Calculator.desktop", "org.gnome.TextEditor.desktop"], "at the end");
    assert_eq!(short(&call(&conn, "SetFavorite", &("org.example.Vault", true, none())).await), "Excluded");
    assert_eq!(short(&call(&conn, "SetFavorite", &("org.helixdesktop.Settings", true, none())).await), "Excluded");
    assert_eq!(short(&call(&conn, "SetFavorite", &("org.example.Nope", true, none())).await), "NotFound");
    // Undone from Settings.
    settings_call(&conn, "Undo", &(s(&pinned, "id").as_str(),)).await.unwrap();
    assert_eq!(favorites(&shell), ["org.gnome.Calculator.desktop"]);
    // Ten changes a minute with one method; one more fails; one that
    // changes nothing still answers (and does not count).
    for i in 0..8 {
        change(&conn, "SetFavorite", &("org.gnome.TextEditor", i % 2 == 0, none())).await;
    }
    // Two made above (pinning, and the undo is Settings', not counted).
    change(&conn, "SetFavorite", &("org.gnome.Calculator", false, none())).await;
    let same = change(&conn, "SetFavorite", &("org.gnome.Calculator", false, none())).await;
    assert!(!b(&same, "changed"));
    let r = call(&conn, "SetFavorite", &("org.gnome.Calculator", true, none())).await;
    assert_eq!(short(&r), "Failed", "the eleventh change: {}", message(&r));
    assert!(message(&r).contains("too many"));
    assert!(change(&conn, "SetFavorite", &("org.gnome.Calculator", false, none())).await.get("changed").is_some_and(|v| !bool::try_from(v.clone()).unwrap()));
    // Another method has its own count.
    assert!(b(&change(&conn, "SetAccent", &("slate", none())).await, "changed"));

    // ----- Stars ---------------------------------------------------------------
    let report = home.join("Documents/report.pdf");
    std::fs::write(&report, b"%PDF").unwrap();
    let r = call(&conn, "StarFile", &(report.to_str().unwrap(), true, none())).await;
    match short(&r).as_str() {
        "ok" => {
            let star: Record = r.unwrap().body().deserialize().unwrap();
            let uri = gio::File::for_path(&report).uri().to_string();
            assert_eq!((s(&star, "name"), s(&star, "target"), b(&star, "old"), b(&star, "new")), ("star".into(), uri.clone(), false, true));
            let starred = helix_settings::stars::Store::open(&data_home, false).starred().unwrap();
            assert!(starred.contains(&uri));
            settings_call(&conn, "Undo", &(s(&star, "id").as_str(),)).await.unwrap();
            assert!(!helix_settings::stars::Store::open(&data_home, false).starred().unwrap().contains(&uri));
            logged(&log, "StarFile", |l| l["method"] == "StarFile" && l["target"] == uri.as_str() && l["old"] == "off" && l["new"] == "on").await;
        }
        other => {
            assert_eq!(other, "NotSupported", "{}", message(&r));
            eprintln!("TinySPARQL is not installed; stars cannot be written");
        }
    }
    assert_eq!(short(&call(&conn, "StarFile", &(home.join("Documents/none.pdf").to_str().unwrap(), true, none())).await), "NotFound");
    assert_eq!(short(&call(&conn, "StarFile", &("/etc/hostname", true, none())).await), "NotSupported", "outside the home folder");
    assert_eq!(short(&call(&conn, "StarFile", &(home.to_str().unwrap(), true, none())).await), "NotSupported", "the home folder itself");

    // ----- A crash, and the next start ---------------------------------------
    let crashed = change(&conn, "SetAccent", &("yellow", until(3600))).await;
    assert_eq!(accent(), "yellow");
    // A new shell reads what the last one kept and puts it back.
    let next = AgentAccess::new(lock.clone(), setup(consent.clone(), agent::no_confirm()));
    next.restore_leftovers().await;
    assert_eq!(accent(), "slate");
    let line = logged(&log, "the next start's restore", |l| l["method"] == "Restore" && l["change"] == s(&crashed, "id")).await;
    assert_eq!(line["cause"], "session-ended");
    // A shell stopping normally puts back first.
    let pending = change(&conn, "SetAccent", &("teal", until(3600))).await;
    access.session_ending().await;
    assert_eq!(accent(), "slate");
    logged(&log, "session end", |l| l["method"] == "Restore" && l["change"] == s(&pending, "id") && l["cause"] == "session-ended").await;

    // ----- Notifications: a fake app with buttons -----------------------------
    let app = bus.connect().await;
    let notify = |summary: &'static str, entry: &'static str, actions: Vec<&'static str>| {
        let app = app.clone();
        async move {
            let hints: HashMap<&str, Value<'_>> = HashMap::from([("desktop-entry", Value::from(entry))]);
            let reply = app
                .call_method(Some("org.freedesktop.Notifications"), "/org/freedesktop/Notifications", Some("org.freedesktop.Notifications"), "Notify", &("Chat", 0u32, "", summary, "body", actions, hints, -1i32))
                .await
                .unwrap();
            reply.body().deserialize::<u32>().unwrap()
        }
    };
    let invoked: Arc<Mutex<Vec<(String, u32, String)>>> = Default::default();
    {
        let rule = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).interface("org.freedesktop.Notifications").unwrap().build();
        let mut stream = zbus::MessageStream::for_match_rule(rule, &app, None).await.unwrap();
        let seen = invoked.clone();
        tokio::spawn(async move {
            while let Some(Ok(m)) = stream.next().await {
                let member = m.header().member().map(|m| m.to_string()).unwrap_or_default();
                let entry = match member.as_str() {
                    "ActionInvoked" => m.body().deserialize::<(u32, String)>().ok().map(|(id, key)| (member.clone(), id, key)),
                    "NotificationClosed" => m.body().deserialize::<(u32, u32)>().ok().map(|(id, why)| (member.clone(), id, why.to_string())),
                    _ => None,
                };
                if let Some(e) = entry {
                    seen.lock().unwrap().push(e);
                }
            }
        });
    }
    let chat = notify("Anna wrote", "org.gnome.Fractal", vec!["default", "Open", "open", "Show", "reply", "Reply", "delete", "Delete"]).await;
    let plain = notify("Build done", "org.gnome.Builder", vec!["show", "Show"]).await;
    let vault = notify("Vault secret", "org.example.Vault", vec!["default", "Open"]).await;
    let listed: Vec<Record> = call(&conn, "ListNotifications", &(none(),)).await.unwrap().body().deserialize().unwrap();
    let rec = listed.iter().find(|r| u32::try_from(r["id"].clone()).unwrap() == chat).expect("listed");
    assert!(!rec.contains_key("actions") && !rec.contains_key("default-action"), "not without the switch: {rec:?}");
    assert!(!listed.iter().any(|r| u32::try_from(r["id"].clone()).unwrap() == vault));
    // Without the switch: refused.
    let r = call(&conn, "NotificationAction", &(chat, "open", none())).await;
    assert_eq!(short(&r), "FeatureOff");
    assert_eq!(message(&r), "notifications");
    logged(&log, "FeatureOff", |l| l["method"] == "NotificationAction" && l["reason"] == "notifications" && l["outcome"] == "refused:FeatureOff").await;
    settings.set_boolean("notifications", true).unwrap();
    let listed: Vec<Record> = call(&conn, "ListNotifications", &(none(),)).await.unwrap().body().deserialize().unwrap();
    let rec = listed.iter().find(|r| u32::try_from(r["id"].clone()).unwrap() == chat).unwrap();
    let buttons: Vec<Record> = Vec::try_from(rec["actions"].clone()).unwrap();
    let pairs: Vec<(String, String)> = buttons.iter().map(|b| (s(b, "id"), s(b, "label"))).collect();
    assert_eq!(pairs, [("open".into(), "Show".into()), ("reply".into(), "Reply".into()), ("delete".into(), "Delete".into())]);
    assert!(b(rec, "default-action"));
    let rec = listed.iter().find(|r| u32::try_from(r["id"].clone()).unwrap() == plain).unwrap();
    assert!(!b(rec, "default-action"));
    // A plain button: done at once, as the user's click.
    settings.set_boolean("confirm-sensitive", true).unwrap();
    let asked = confirm.count();
    let r = call(&conn, "NotificationAction", &(plain, "show", none())).await;
    assert_eq!(short(&r), "ok", "{}", message(&r));
    assert_eq!(confirm.count(), asked, "not sensitive");
    eventually("ActionInvoked show", 5, || invoked.lock().unwrap().contains(&("ActionInvoked".into(), plain, "show".into()))).await;
    // Reply and Delete are sensitive by their labels; the prompt names the
    // button. Declined: nothing pressed.
    confirm.queue(Some(false));
    let r = call(&conn, "NotificationAction", &(chat, "delete", none())).await;
    assert_eq!(short(&r), "DeniedByUser");
    assert_eq!(confirm.last(), ("NotificationAction".into(), None, Some("Delete".into()), None));
    assert!(!invoked.lock().unwrap().iter().any(|(_, id, key)| *id == chat && key == "delete"));
    // Without asking: NeedsConfirmation.
    let r = call(&conn, "NotificationAction", &(chat, "reply", opts(&[("interactive", Value::from(false))]))).await;
    assert_eq!(short(&r), "NeedsConfirmation");
    // Clicking the notification itself is sensitive too (it has such
    // buttons); allowed.
    confirm.queue(Some(true));
    call(&conn, "NotificationAction", &(chat, "default", none())).await.unwrap();
    assert_eq!(confirm.last().0, "NotificationAction");
    eventually("ActionInvoked default", 5, || invoked.lock().unwrap().contains(&("ActionInvoked".into(), chat, "default".into()))).await;
    settings.set_boolean("confirm-sensitive", false).unwrap();
    // Unknown buttons and notifications, an excluded app's: NotFound.
    let plain2 = notify("Another", "org.gnome.Builder", vec!["show", "Show"]).await;
    assert_eq!(short(&call(&conn, "NotificationAction", &(plain2, "nope", none())).await), "NotFound");
    assert_eq!(short(&call(&conn, "NotificationAction", &(plain2, "default", none())).await), "NotFound", "no default action");
    assert_eq!(short(&call(&conn, "NotificationAction", &(vault, "default", none())).await), "NotFound");
    assert_eq!(short(&call(&conn, "DismissNotification", &(vault, none())).await), "NotFound");
    assert_eq!(short(&call(&conn, "DismissNotification", &(99_999u32, none())).await), "NotFound");
    // Dismissing: as the user's ×.
    call(&conn, "DismissNotification", &(plain2, none())).await.unwrap();
    eventually("NotificationClosed 2", 5, || invoked.lock().unwrap().contains(&("NotificationClosed".into(), plain2, "2".into()))).await;
    let line = logged(&log, "NotificationAction", |l| l["method"] == "NotificationAction" && l["outcome"] == "allowed").await;
    let all = serde_json::to_string(&activity(&log)).unwrap();
    for secret in ["Anna wrote", "Reply\"", "Vault secret", "Delete\""] {
        assert!(!all.contains(secret), "{secret} in the log");
    }
    assert!(line["target"].as_str().unwrap().starts_with("notification:"));

    // ----- RevealFile with a fake file manager --------------------------------
    let doc = home.join("Documents/report.pdf");
    assert_eq!(short(&call(&conn, "RevealFile", &(doc.to_str().unwrap(), none())).await), "NotSupported", "no file manager");
    // Only activatable: the default app for folders is checked, and it is
    // excluded; nothing is started.
    std::fs::write(services.join("org.freedesktop.FileManager1.service"), "[D-BUS Service]\nName=org.freedesktop.FileManager1\nExec=/bin/false\n").unwrap();
    let dbus = zbus::fdo::DBusProxy::new(&conn).await.unwrap();
    eventually_async("the file manager is activatable", || async { dbus.list_activatable_names().await.unwrap().iter().any(|n| n.as_str() == "org.freedesktop.FileManager1") }).await;
    let r = call(&conn, "RevealFile", &(doc.to_str().unwrap(), none())).await;
    assert_eq!((short(&r), message(&r)), ("Excluded".into(), "app:org.example.Vault".into()));
    std::fs::remove_file(services.join("org.freedesktop.FileManager1.service")).unwrap();
    let (_files, revealed) = helix_test_support::file_manager::serve(&bus).await;
    call(&conn, "RevealFile", &(doc.to_str().unwrap(), none())).await.unwrap();
    let doc_uri = gio::File::for_path(&doc).uri().to_string();
    assert_eq!(revealed.lock().unwrap().last().map(|(u, _)| u.clone()), Some(vec![doc_uri.clone()]));
    assert_eq!(short(&call(&conn, "RevealFile", &(home.join("gone.txt").to_str().unwrap(), none())).await), "NotFound");
    // Hidden files may be revealed (nothing is read).
    call(&conn, "RevealFile", &(secret.to_str().unwrap(), none())).await.unwrap();
    // The file manager is an excluded app (its process shows the Vault's
    // window here): refused before anything is shown.
    snapshot.lock().unwrap().windows.push(WindowInfo { id: 30, app_id: "org.example.Vault".into(), pid: std::process::id(), geometry: (0, 0, 10, 10), ..Default::default() });
    let before = revealed.lock().unwrap().len();
    assert_eq!(short(&call(&conn, "RevealFile", &(doc.to_str().unwrap(), none())).await), "Excluded");
    assert_eq!(revealed.lock().unwrap().len(), before);
    snapshot.lock().unwrap().windows.retain(|w| w.id != 30);
    logged(&log, "RevealFile", |l| l["method"] == "RevealFile" && l["target"] == doc_uri.as_str()).await;

    // ----- To another monitor ---------------------------------------------------
    call(&conn, "WindowAction", &(11u32, "to-monitor", opts(&[("monitor", Value::from("DP-2"))]))).await.unwrap();
    let geometry = snapshot.lock().unwrap().windows.iter().find(|w| w.id == 11).unwrap().geometry;
    assert_eq!((geometry.0, geometry.1), (3840 + 100, 100), "two monitors to the right");
    call(&conn, "WindowAction", &(11u32, "to-monitor", opts(&[("monitor", Value::from("DP-2"))]))).await.unwrap();
    assert_eq!(short(&call(&conn, "WindowAction", &(11u32, "to-monitor", opts(&[("monitor", Value::from("VGA-7"))]))).await), "NotFound");
    assert_eq!(short(&call(&conn, "WindowAction", &(11u32, "to-monitor", none())).await), "org.freedesktop.DBus.Error.InvalidArgs");
    assert_eq!(short(&call(&conn, "WindowAction", &(12u32, "to-monitor", opts(&[("monitor", Value::from("eDP-1"))]))).await), "Excluded");
    let line = logged(&log, "to-monitor", |l| l["method"] == "WindowAction" && l["action"] == "to-monitor" && l["outcome"] == "allowed" && l.get("error").is_none()).await;
    assert_eq!(line["target"], "window:11 (org.gnome.TextEditor)");

    // ----- ClearLog: the changes go, pending restores stay ---------------------
    let kept = change(&conn, "SetAccent", &("teal", until(3600))).await;
    settings_call(&conn, "ClearLog", &()).await.unwrap();
    let listed = list_changes(&conn).await;
    assert_eq!(listed.iter().map(|r| s(r, "id")).collect::<Vec<_>>(), [s(&kept, "id")]);
    // The file: 0600, the shell's own.
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&changes_path).unwrap().permissions().mode() & 0o777, 0o600);
    access.stop(&agent_id);
    eventually("teal back", 5, || accent() == "slate").await;

    drop((notifications, server));
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Wait (at most 10 s) until `f` gives true.
async fn eventually_async<F: std::future::Future<Output = bool>>(what: &str, mut f: impl FnMut() -> F) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f().await {
        assert!(Instant::now() < deadline, "{what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Stop the fake power-profiles-daemon `old` and start another offering
/// `profiles` (once the old one's name is gone).
async fn reserve(system: &TestBus, client: &Connection, old: Connection, profiles: (&str, &[&str])) -> Connection {
    let _ = old.release_name("org.freedesktop.UPower.PowerProfiles").await;
    drop(old);
    let dbus = zbus::fdo::DBusProxy::new(client).await.unwrap();
    let name = zbus::names::BusName::from_static_str("org.freedesktop.UPower.PowerProfiles").unwrap();
    eventually_async("the old power-profiles-daemon goes", || async { !dbus.name_has_owner(name.clone()).await.unwrap_or(true) }).await;
    upower::serve(system, None, Some(profiles)).await
}

fn favorites(shell: &gio::Settings) -> Vec<String> {
    shell.strv("favorite-apps").iter().map(|s| s.to_string()).collect()
}
