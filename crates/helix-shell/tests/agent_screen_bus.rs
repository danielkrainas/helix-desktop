//! Agent Access's *Screen and input* methods over a private bus
//! (helixdesktop/spec `contracts/agent-access/README.md`, "Screen and
//! input methods"; helixdesktop/desktop#150), against a stand-in UI whose
//! compositor takes pictures and carries out input as the real one
//! answers: what the compositor is told may be shown (excluded apps,
//! Settings, a system prompt's program and a process showing an excluded
//! window left out; only the plain shell surfaces), window, monitor and
//! area screenshots, every refusal (access off, the level, locked, a
//! system prompt, protected windows, unknown windows and monitors, bad
//! areas), input steps in order into the focused or a named window
//! (focused first), every way the compositor stops a run (the user's
//! input, the cooldown, focus, a password field, no key, the window gone,
//! the lock, a prompt), *Stop* cutting a run short, the sensitive-action
//! prompt, the indicator's "controlling" state, and a log of counts and
//! kinds only. Nothing on the host is touched: scratch XDG directories,
//! in-memory settings, a private bus.

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gio::prelude::*;
use helix_shell::agent::desktop::{MonitorInfo, Snapshot, WindowInfo};
use helix_shell::agent::grants::Answer;
use helix_shell::agent::screen::{Request, Shot, ShotError, Step, Stopped, SHOWN_LAYERS};
use helix_shell::agent::{self, AgentAccess, Agent1, Confirm, Desktop, Setup, OBJECT_PATH};
use helix_shell::screensaver::SharedStatus;
use helix_shell::UiEvent;
use helix_test_support::conformance::{assert_conformant, check, introspect, load_dbus_contract};
use helix_test_support::TestBus;
use zbus::zvariant::{OwnedValue, Value};
use zbus::Connection;

const IFACE: &str = "org.helixdesktop.Agent1";

#[derive(Clone, Default)]
struct FakeConfirm {
    answers: Arc<Mutex<VecDeque<Option<bool>>>>,
    asked: Arc<Mutex<Vec<String>>>,
}

impl FakeConfirm {
    fn hook(&self) -> Confirm {
        let me = self.clone();
        Arc::new(move |request| {
            me.asked.lock().unwrap().push(request.method.to_owned());
            let answer = me.answers.lock().unwrap().pop_front().flatten();
            Box::pin(async move { answer })
        })
    }
}

/// A capture as asked for: window, area, shown windows, shown layers.
type Asked = (u32, Option<(i32, i32, i32, i32)>, Vec<u32>, Vec<String>);

/// What the stand-in compositor was asked, and how it answers.
#[derive(Default)]
struct Comp {
    /// The last capture's request: window, area, shown, layers.
    capture: Option<Asked>,
    /// The next capture fails so.
    capture_fails: Option<ShotError>,
    /// Steps received, by run, in order.
    steps: Vec<Step>,
    /// Stop the run at the step with this index (from 0, counting every
    /// step received) with this reason.
    stop_at: Option<(usize, Stopped)>,
    /// Never answer steps (a run that hangs until cancelled).
    hang: bool,
    /// Runs started (window), cancelled, ended.
    started: Vec<u32>,
    cancelled: usize,
    ended: usize,
    /// Replies held back while hanging.
    held: Vec<tokio::sync::oneshot::Sender<Result<(), Stopped>>>,
    /// The indicator's agents as last sent.
    controlling: Vec<bool>,
}

/// A PNG header of `w`×`h` and a little more.
fn png(w: u32, h: u32) -> Vec<u8> {
    let mut out = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    out.extend_from_slice(&w.to_be_bytes());
    out.extend_from_slice(&h.to_be_bytes());
    out.extend_from_slice(b"\x08\x06\0\0\0rest of the picture");
    out
}

fn short(result: &zbus::Result<zbus::Message>) -> String {
    match result {
        Err(zbus::Error::MethodError(name, _, _)) => name.as_str().trim_start_matches("org.helixdesktop.Agent1.Error.").trim_start_matches("org.freedesktop.DBus.Error.").to_owned(),
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

fn refusal(result: &zbus::Result<zbus::Message>) -> (String, String) {
    (short(result), message(result))
}

type Opts = HashMap<&'static str, Value<'static>>;

fn opts(pairs: &[(&'static str, Value<'static>)]) -> Opts {
    pairs.iter().cloned().collect()
}

async fn call(conn: &Connection, method: &str, body: &(impl serde::Serialize + zbus::zvariant::DynamicType)) -> zbus::Result<zbus::Message> {
    conn.call_method(Some("org.gnome.Shell"), OBJECT_PATH, Some(IFACE), method, body).await
}

async fn shot(conn: &Connection, o: Opts) -> zbus::Result<zbus::Message> {
    call(conn, "Screenshot", &(o,)).await
}

async fn input(conn: &Connection, events: Vec<Opts>, o: Opts) -> zbus::Result<zbus::Message> {
    call(conn, "Input", &(events, o)).await
}

fn ev(kind: &'static str, pairs: &[(&'static str, Value<'static>)]) -> Opts {
    let mut e = opts(pairs);
    e.insert("type", Value::from(kind));
    e
}

type Record = HashMap<String, OwnedValue>;

fn record(r: zbus::Result<zbus::Message>) -> Record {
    r.unwrap_or_else(|e| panic!("{e:?}")).body().deserialize().unwrap()
}

fn int(r: &Record, key: &str) -> i64 {
    let v = r[key].clone();
    i32::try_from(v.clone()).map(i64::from).or_else(|_| u32::try_from(v).map(i64::from)).unwrap()
}

fn win(id: u32, app: &str, pid: u32) -> WindowInfo {
    WindowInfo { id, app_id: app.into(), title: format!("{app} window"), geometry: (100, 100, 640, 480), pid, ..Default::default() }
}

fn activity(dir: &Path) -> Vec<serde_json::Value> {
    let mut days: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "jsonl")).collect();
    days.sort();
    days.iter().flat_map(|d| std::fs::read_to_string(d).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect::<Vec<_>>()).collect()
}

/// A process whose program is called `name`: a copy of Python that waits.
fn program(bin: &Path, name: &str) -> Option<std::process::Child> {
    let python = std::fs::canonicalize("/usr/bin/python3").ok()?;
    let path = bin.join(name);
    std::fs::copy(python, &path).ok()?;
    std::process::Command::new(&path).args(["-c", "import time; time.sleep(60)"]).env("PYTHONHOME", "/usr").spawn().ok()
}

#[tokio::test(flavor = "multi_thread")]
async fn screen_and_input_over_the_bus() {
    let Some(bus) = TestBus::start() else { return };
    let tmp = std::env::temp_dir().join(format!("helix-agent-screen-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let schemas = tmp.join("schemas");
    let bin = tmp.join("bin");
    for dir in [&schemas, &bin, &tmp.join("home"), &tmp.join("data"), &tmp.join("config"), &tmp.join("run")] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let data = concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/org.helixdesktop.agent-access.gschema.xml");
    std::fs::copy(data, schemas.join("org.helixdesktop.agent-access.gschema.xml")).unwrap();
    let Ok(compiled) = std::process::Command::new("glib-compile-schemas").arg(&schemas).status() else {
        eprintln!("glib-compile-schemas is not installed; skipping");
        return;
    };
    assert!(compiled.success());
    // SAFETY: this test binary runs this one test, before any GLib use.
    unsafe {
        std::env::set_var("GSETTINGS_SCHEMA_DIR", &schemas);
        std::env::set_var("GSETTINGS_BACKEND", "memory");
        std::env::set_var("HOME", tmp.join("home"));
        std::env::set_var("XDG_CONFIG_HOME", tmp.join("config"));
        std::env::set_var("XDG_STATE_HOME", tmp.join("state"));
        std::env::set_var("XDG_CACHE_HOME", tmp.join("cache"));
        std::env::set_var("XDG_DATA_HOME", tmp.join("data"));
        std::env::set_var("XDG_DATA_DIRS", tmp.join("sys"));
        std::env::set_var("XDG_CONFIG_DIRS", tmp.join("sysconfig"));
        std::env::set_var("XDG_RUNTIME_DIR", tmp.join("run"));
        std::env::set_var("GIO_USE_VFS", "local");
        std::env::set_var("DBUS_SESSION_BUS_ADDRESS", bus.address());
        std::env::set_var("DBUS_SYSTEM_BUS_ADDRESS", bus.address());
    }
    let (Some(mut pinentry), Some(mut two_windows)) = (program(&bin, "pinentry-gnome3"), program(&bin, "two-windows")) else {
        eprintln!("python3 is not installed; skipping");
        return;
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::fs::read_link(format!("/proc/{}/exe", pinentry.id())).map_or(true, |e| !e.ends_with("pinentry-gnome3")) {
        assert!(Instant::now() < deadline, "the stand-in prompt program did not start");
        std::thread::sleep(Duration::from_millis(20));
    }

    // Windows: an editor (focused), notes and a vault of one process, the
    // vault excluded, Settings, a pin entry program's window.
    let snapshot = Arc::new(Mutex::new(Snapshot {
        windows: vec![
            win(11, "org.gnome.TextEditor", 0),
            win(12, "org.example.Notes", two_windows.id()),
            win(13, "org.example.Vault", two_windows.id()),
            win(14, "org.helixdesktop.Settings", 0),
            win(15, "org.example.Pin", pinentry.id()),
            win(16, "org.gnome.Calculator", 0),
        ],
        focused: 11,
        workspaces: 1,
        monitors: vec![MonitorInfo { connector: "HEADLESS-1".into(), name: "Monitor".into(), area: (0, 0, 1280, 720), scale: 1.0 }, MonitorInfo { connector: "HEADLESS-2".into(), name: "Second".into(), area: (1280, 0, 1280, 720), scale: 1.0 }],
        ..Default::default()
    }));
    let comp = Arc::new(Mutex::new(Comp::default()));
    let (ui, mut ui_rx) = tokio::sync::mpsc::unbounded_channel::<UiEvent>();
    let (shown, compositor, picture_dir) = (snapshot.clone(), comp.clone(), tmp.clone());
    tokio::spawn(async move {
        let mut n = 0;
        while let Some(event) = ui_rx.recv().await {
            match event {
                UiEvent::AgentSnapshot(slot) => {
                    if let Some(reply) = slot.take() {
                        let _ = reply.send(shown.lock().unwrap().clone());
                    }
                }
                UiEvent::AgentWindow(slot) => {
                    if let Some(request) = slot.take() {
                        for op in &request.ops {
                            if let helix_shell::agent::actions::WindowOp::Activate(id) = op {
                                shown.lock().unwrap().focused = *id;
                            }
                        }
                        let _ = request.reply.send(Ok(()));
                    }
                }
                UiEvent::AgentsActive(agents) => compositor.lock().unwrap().controlling = agents.iter().map(|a| a.controlling).collect(),
                UiEvent::AgentScreen(slot) => {
                    let mut c = compositor.lock().unwrap();
                    match slot.take() {
                        Some(Request::Capture { window, area, shown, layers, reply }) => {
                            c.capture = Some((window, area, shown, layers));
                            let result = match c.capture_fails.take() {
                                Some(e) => Err(e),
                                None => {
                                    n += 1;
                                    let path = picture_dir.join(format!("shot-{n}.png"));
                                    let mut file = std::fs::OpenOptions::new().create(true).truncate(true).read(true).write(true).open(&path).unwrap();
                                    let (w, h) = area.map_or((2560, 720), |a| (a.2 as u32 * 2, a.3 as u32 * 2));
                                    file.write_all(&png(w, h)).unwrap();
                                    let rect = area.unwrap_or((0, 0, 2560, 720));
                                    Ok(Shot { file, rect, filled: 3 })
                                }
                            };
                            let _ = reply.send(result);
                        }
                        Some(Request::Start { window, reply, .. }) => {
                            c.started.push(window);
                            let _ = reply.send(Ok(c.started.len() as u64));
                        }
                        Some(Request::Step { step, reply, .. }) => {
                            let index = c.steps.len();
                            c.steps.push(step);
                            match c.stop_at {
                                Some((at, why)) if at == index => {
                                    let _ = reply.send(Err(why));
                                }
                                _ if c.hang => c.held.push(reply),
                                _ => {
                                    let _ = reply.send(Ok(()));
                                }
                            }
                        }
                        Some(Request::Cancel { .. }) => {
                            c.cancelled += 1;
                            for reply in c.held.drain(..) {
                                let _ = reply.send(Err(Stopped::Cancelled));
                            }
                        }
                        Some(Request::End { .. }) => c.ended += 1,
                        None => {}
                    }
                }
                _ => {}
            }
        }
    });

    let settings = gio::Settings::new("org.helixdesktop.agent-access");
    let lock: SharedStatus = Default::default();
    let consent: agent::Consent = Arc::new(|_| Box::pin(async { Answer::Session }));
    let confirm = FakeConfirm::default();
    let log = tmp.join("state/helix/agent-activity");
    let access = AgentAccess::new(lock.clone(), Setup { grants_path: None, consent, confirm: confirm.hook(), proc_root: "/proc".into(), settings_exes: Vec::new(), log_dir: Some(log.clone()), reminders_path: None });
    access.set_desktop(Desktop { ui: ui.clone(), notifications: None });
    access.set_indicator(ui.clone());
    let server = bus.builder().serve_at(OBJECT_PATH, Agent1 { access: access.clone() }).unwrap().name("org.gnome.Shell").unwrap().build().await.unwrap();
    access.attach(&server);
    let conn = bus.connect().await;

    // Every method of the contract is there now.
    let actual = introspect(&conn, "org.gnome.Shell", OBJECT_PATH).await;
    assert_conformant(IFACE, &check(IFACE, &load_dbus_contract("../agent-access/org.helixdesktop.Agent1.xml"), &actual, helix_test_support::agent::AGENT1_LATER));

    let quiet = || opts(&[("interactive", Value::from(false))]);
    let typing = || vec![ev("text", &[("text", Value::from("Dear diary"))])];
    // Off; then the level too low.
    assert_eq!(short(&shot(&conn, quiet()).await), "AccessOff");
    assert_eq!(short(&input(&conn, typing(), quiet()).await), "AccessOff");
    settings.set_boolean("enabled", true).unwrap();
    settings.set_boolean("allow-remote", true).unwrap();
    settings.set_string("access-level", "apps").unwrap();
    assert_eq!(refusal(&shot(&conn, quiet()).await), ("LevelTooLow".into(), "screen-and-input".into()));
    assert_eq!(refusal(&input(&conn, typing(), quiet()).await), ("LevelTooLow".into(), "screen-and-input".into()));
    settings.set_string("access-level", "screen-and-input").unwrap();
    settings.set_strv("excluded-apps", ["org.example.Vault"]).unwrap();

    // The whole screen: the compositor is told what may be shown.
    let r = record(shot(&conn, opts(&[])).await);
    assert_eq!((int(&r, "width"), int(&r, "height"), int(&r, "filled")), (2560, 720, 3));
    assert!(<Vec<u8>>::try_from(r["png"].clone()).unwrap().starts_with(b"\x89PNG"));
    let (window, area, shown_ids, layers) = comp.lock().unwrap().capture.clone().unwrap();
    assert_eq!((window, area), (0, None));
    assert_eq!(shown_ids, [11, 16], "not the vault, nor its process's notes, Settings or the pin entry's window");
    assert_eq!(layers, SHOWN_LAYERS);
    for hidden in ["helix-agent-prompt", "helix-dialog", "helix-notification", "helix-overview", "helix-launcher", "helix-osk", "helix-switcher"] {
        assert!(!layers.iter().any(|l| l == hidden), "{hidden} is filled");
    }
    // A monitor; an area of it; an area of the screen.
    let r = record(shot(&conn, opts(&[("monitor", Value::from("HEADLESS-2"))])).await);
    assert_eq!(comp.lock().unwrap().capture.as_ref().unwrap().1, Some((1280, 0, 1280, 720)));
    assert_eq!(f64::try_from(r["scale"].clone()).unwrap(), 2.0);
    record(shot(&conn, opts(&[("monitor", Value::from("HEADLESS-2")), ("x", Value::from(10i32)), ("y", Value::from(20i32)), ("width", Value::from(30i32)), ("height", Value::from(40i32))])).await);
    assert_eq!(comp.lock().unwrap().capture.as_ref().unwrap().1, Some((1290, 20, 30, 40)));
    assert_eq!(short(&shot(&conn, opts(&[("monitor", Value::from("HEADLESS-2")), ("x", Value::from(1270i32)), ("y", Value::from(0i32)), ("width", Value::from(30i32)), ("height", Value::from(40i32))])).await), "InvalidArgs");
    assert_eq!(short(&shot(&conn, opts(&[("monitor", Value::from("DP-9"))])).await), "NotFound");
    // A window: its own picture; protected windows refused outright.
    record(shot(&conn, opts(&[("window", Value::from(16u32))])).await);
    assert_eq!(comp.lock().unwrap().capture.as_ref().unwrap().0, 16);
    for (id, token) in [(13u32, "app:org.example.Vault"), (12, "app:org.example.Vault"), (14, "settings"), (15, "system-prompt")] {
        assert_eq!(refusal(&shot(&conn, opts(&[("window", Value::from(id))])).await), ("Excluded".to_owned(), token.to_owned()), "window {id}");
    }
    assert_eq!(short(&shot(&conn, opts(&[("window", Value::from(99u32))])).await), "NotFound");
    assert_eq!(short(&shot(&conn, opts(&[("window", Value::from(16u32)), ("monitor", Value::from("HEADLESS-1"))])).await), "InvalidArgs");
    assert_eq!(short(&shot(&conn, opts(&[("x", Value::from(1i32))])).await), "InvalidArgs");
    // The compositor's refusals.
    comp.lock().unwrap().capture_fails = Some(ShotError::Invalid);
    assert_eq!(short(&shot(&conn, opts(&[])).await), "InvalidArgs");
    comp.lock().unwrap().capture_fails = Some(ShotError::Locked);
    assert_eq!(short(&shot(&conn, opts(&[])).await), "Locked");
    comp.lock().unwrap().capture_fails = Some(ShotError::NoWindow);
    assert_eq!(short(&shot(&conn, opts(&[("window", Value::from(16u32))])).await), "NotFound");
    // The indicator showed the agent as controlling the screen.
    assert!(comp.lock().unwrap().controlling.iter().any(|c| *c), "the indicator says the agent controls the screen");

    // Input into the focused window: the steps in order.
    let events = vec![
        ev("click", &[("x", Value::from(10i32)), ("y", Value::from(20i32))]),
        ev("key", &[("keys", Value::from("ctrl+s"))]),
        ev("text", &[("text", Value::from("Dear diary"))]),
        ev("scroll", &[("dy", Value::from(3i32))]),
        ev("wait", &[("ms", Value::from(10u32))]),
    ];
    input(&conn, events.clone(), opts(&[])).await.unwrap();
    {
        let c = comp.lock().unwrap();
        assert_eq!(c.started, [11]);
        use xkbcommon_keysyms::*;
        assert_eq!(
            c.steps,
            [
                Step::Motion(10.0, 20.0),
                Step::Button(0x110, true),
                Step::Button(0x110, false),
                Step::Key(KEY_CONTROL_L, true),
                Step::Key(KEY_S, true),
                Step::Key(KEY_S, false),
                Step::Key(KEY_CONTROL_L, false),
                Step::Text("Dear diary".into()),
                Step::Scroll(0, 3),
            ]
        );
        assert_eq!(c.ended, 1);
    }
    // Into a named window: focused first.
    comp.lock().unwrap().steps.clear();
    input(&conn, typing(), opts(&[("window", Value::from(16u32))])).await.unwrap();
    assert_eq!(snapshot.lock().unwrap().focused, 16);
    assert_eq!(comp.lock().unwrap().started.last(), Some(&16));
    // Not into protected windows, nor nowhere, nor outside the window.
    for (id, token) in [(13u32, "app:org.example.Vault"), (12, "app:org.example.Vault"), (14, "settings"), (15, "system-prompt")] {
        assert_eq!(refusal(&input(&conn, typing(), opts(&[("window", Value::from(id))])).await), ("Excluded".to_owned(), token.to_owned()), "window {id}");
    }
    snapshot.lock().unwrap().focused = 13;
    assert_eq!(refusal(&input(&conn, typing(), opts(&[])).await), ("Excluded".to_owned(), "app:org.example.Vault".to_owned()), "the focused window is protected");
    snapshot.lock().unwrap().focused = 0;
    assert_eq!(short(&input(&conn, typing(), opts(&[])).await), "NotFound");
    snapshot.lock().unwrap().focused = 11;
    assert_eq!(short(&input(&conn, vec![ev("move", &[("x", Value::from(700i32)), ("y", Value::from(1i32))])], opts(&[])).await), "InvalidArgs");
    assert_eq!(short(&input(&conn, vec![ev("teleport", &[])], opts(&[])).await), "NotSupported");
    assert_eq!(short(&input(&conn, vec![ev("key", &[("keys", Value::from("hyper+q"))])], opts(&[])).await), "InvalidArgs");

    // How the compositor stops a run, and what the agent hears.
    for (why, error, token) in [
        (Stopped::UserInput, "Cancelled", "user-input"),
        (Stopped::Cooldown, "Cancelled", "cooldown"),
        (Stopped::Cancelled, "Cancelled", "stopped"),
        (Stopped::Password, "Excluded", "password-field"),
        (Stopped::Prompt, "Excluded", "system-prompt"),
        (Stopped::Focus, "Failed", ""),
        (Stopped::Outside, "Failed", ""),
        (Stopped::NoKey, "NotSupported", ""),
        (Stopped::Gone, "NotFound", ""),
        (Stopped::Locked, "Locked", ""),
    ] {
        {
            let mut c = comp.lock().unwrap();
            c.steps.clear();
            c.stop_at = Some((2, why));
        }
        let r = input(&conn, events.clone(), opts(&[])).await;
        assert_eq!(short(&r), error, "{why:?}");
        if !token.is_empty() {
            assert_eq!(message(&r), token, "{why:?}");
        }
        assert_eq!(comp.lock().unwrap().steps.len(), 3, "{why:?}: nothing after the stop");
    }
    comp.lock().unwrap().stop_at = None;

    // A system prompt, the lock: nothing.
    snapshot.lock().unwrap().system_prompt = true;
    assert_eq!(refusal(&input(&conn, typing(), opts(&[])).await), ("Excluded".to_owned(), "system-prompt".to_owned()));
    assert_eq!(refusal(&shot(&conn, opts(&[])).await), ("Excluded".to_owned(), "system-prompt".to_owned()));
    snapshot.lock().unwrap().system_prompt = false;
    lock.lock().unwrap().locked_since = Some(Instant::now());
    assert_eq!(short(&input(&conn, typing(), opts(&[])).await), "Locked");
    assert_eq!(short(&shot(&conn, opts(&[])).await), "Locked");
    lock.lock().unwrap().locked_since = None;

    // Marked sensitive: the user is asked.
    confirm.answers.lock().unwrap().push_back(Some(false));
    assert_eq!(short(&input(&conn, typing(), opts(&[("sensitive", Value::from(true))])).await), "DeniedByUser");
    assert_eq!(short(&input(&conn, typing(), opts(&[("sensitive", Value::from(true)), ("interactive", Value::from(false))])).await), "NeedsConfirmation");
    assert_eq!(confirm.asked.lock().unwrap().as_slice(), ["Input"], "asked once: never without interactive");

    // *Stop* cuts a run short at once.
    let status: Record = call(&conn, "GetStatus", &()).await.unwrap().body().deserialize().unwrap();
    let agent_id = String::try_from(status["agent"].clone()).unwrap();
    comp.lock().unwrap().hang = true;
    let started = Instant::now();
    let running = {
        let conn = conn.clone();
        tokio::spawn(async move { input(&conn, vec![ev("text", &[("text", Value::from("slow"))]), ev("text", &[("text", Value::from("more"))])], opts(&[])).await })
    };
    while comp.lock().unwrap().held.is_empty() {
        assert!(started.elapsed() < Duration::from_secs(5), "the run did not start");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    access.stop(&agent_id);
    let r = running.await.unwrap();
    assert_eq!(refusal(&r), ("Cancelled".to_owned(), "stopped".to_owned()));
    assert!(started.elapsed() < Duration::from_secs(3), "stopped at once");
    assert!(comp.lock().unwrap().cancelled >= 1, "the compositor was told to stop");
    comp.lock().unwrap().hang = false;

    let _ = pinentry.kill();
    let _ = two_windows.kill();
    let _ = pinentry.wait();
    let _ = two_windows.wait();

    // The log: scopes, sizes, counts; never what was typed or pressed.
    let lines = activity(&log);
    let has = |method: &str, outcome: &str, extra: &dyn Fn(&serde_json::Value) -> bool| lines.iter().any(|l| l["method"] == method && l["outcome"] == outcome && extra(l));
    assert!(has("Screenshot", "allowed", &|l| l["level"] == "screen-and-input" && l["scope"] == "screen" && l["width"] == 2560 && l["height"] == 720 && l["filled-count"] == 3), "{lines:#?}");
    assert!(has("Screenshot", "allowed", &|l| l["scope"] == "monitor" && l["target"] == "monitor:HEADLESS-2"), "{lines:#?}");
    assert!(has("Screenshot", "allowed", &|l| l["scope"] == "window" && l["target"] == "window:16 (org.gnome.Calculator)"), "{lines:#?}");
    assert!(has("Screenshot", "refused:Excluded", &|l| l["target"] == "window:13 (org.example.Vault)" && l["reason"] == "app"), "the reason, never the excluded app's id: {lines:#?}");
    assert!(has("Screenshot", "refused:Excluded", &|l| l["reason"] == "system-prompt"), "{lines:#?}");
    assert!(has("Input", "allowed", &|l| l["target"] == "window:11 (org.gnome.TextEditor)" && l["event-count"] == 5 && l["click-count"] == 1 && l["shortcut-count"] == 1 && l["text-length"] == 10 && l["scroll-count"] == 1 && l["done-count"] == 10), "{lines:#?}");
    // Why a run stopped, as a token.
    for reason in ["user-input", "cooldown", "stopped"] {
        assert!(has("Input", "cancelled", &|l| l["done-count"] == 2 && l["reason"] == reason), "{reason}: {lines:#?}");
    }
    for reason in ["password-field", "system-prompt", "app"] {
        assert!(has("Input", "refused:Excluded", &|l| l["reason"] == reason), "{reason}: {lines:#?}");
    }
    assert!(has("Input", "refused:Excluded", &|l| l["target"] == "window:14 (org.helixdesktop.Settings)" && l["reason"] == "settings"), "{lines:#?}");
    assert!(!lines.iter().any(|l| l["outcome"] == "allowed" && l.get("reason").is_some()), "{lines:#?}");
    assert!(!lines.iter().any(|l| l["outcome"] == "refused:Locked" && l.get("reason").is_some()), "{lines:#?}");
    assert!(has("Input", "refused:DeniedByUser", &|_| true), "{lines:#?}");
    let all = lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
    for secret in ["diary", "ctrl", "slow", "Dear", "hyper"] {
        assert!(!all.contains(secret), "{secret} is in the log");
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

/// The keysyms the test expects (xkbcommon's values).
mod xkbcommon_keysyms {
    pub const KEY_CONTROL_L: u32 = 0xffe3;
    pub const KEY_S: u32 = 0x0073;
}
