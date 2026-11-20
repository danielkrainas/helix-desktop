//! Agent Access's *Control apps* methods over private buses
//! (helixdesktop/spec `contracts/agent-access/README.md`, "Control apps
//! methods"; helixdesktop/desktop#149), against a real GTK 4 window: a
//! private accessibility bus with at-spi2-core's registry (never the
//! user's), a headless compositor for the window to show on, and a
//! stand-in UI that says which windows there are. The shell's side reads
//! the window's tree, finds nodes, presses a button, fills in a field,
//! moves a slider and gives a field the focus; withholds text fields'
//! contents unless `read-text-fields` is on; and refuses every way it
//! must: access off, the level, locked, password fields (read and set,
//! the role and hidden characters), excluded apps and Settings (also as
//! another window of the same process), the shell's own process, a system
//! prompt shown, a clipboard action without the switch, ambiguous and
//! unchecked accessibility names, nodes outside the window; with the
//! sensitive-action prompt for a "Delete" button. The activity log holds
//! none of the window's words.
//!
//! Needs helix-comp built next to this test, dbus-daemon, at-spi2-core's
//! registry and GTK 4 for Python; skipped otherwise.

use std::collections::{HashMap, VecDeque};
use std::io::BufRead;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gio::prelude::*;
use helix_shell::agent::desktop::{Snapshot, WindowInfo};
use helix_shell::agent::grants::Answer;
use helix_shell::agent::{self, AgentAccess, Agent1, AgentAccessSettings, Confirm, Control, Desktop, Setup, OBJECT_PATH};
use helix_shell::screensaver::SharedStatus;
use helix_shell::UiEvent;
use helix_test_support::a11y::{gtk3_python, gtk4_python, write_gtk3_controls, write_gtk4_controls, A11yBus};
use helix_test_support::conformance::{assert_conformant, check, introspect, load_dbus_contract};
use helix_test_support::headless::{sibling_binary, HeadlessSession, Options};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::Connection;

const IFACE: &str = "org.helixdesktop.Agent1";
const APP: &str = "org.example.A11yTest";

/// What the sensitive-action prompt was asked: the method and the control.
type Asked = Arc<Mutex<Vec<(String, Option<Control>)>>>;

#[derive(Clone, Default)]
struct FakeConfirm {
    answers: Arc<Mutex<VecDeque<Option<bool>>>>,
    asked: Asked,
}

impl FakeConfirm {
    fn hook(&self) -> Confirm {
        let me = self.clone();
        Arc::new(move |request| {
            me.asked.lock().unwrap().push((request.method.to_owned(), request.control.clone()));
            let answer = me.answers.lock().unwrap().pop_front().flatten();
            Box::pin(async move { answer })
        })
    }

    fn queue(&self, answer: Option<bool>) {
        self.answers.lock().unwrap().push_back(answer);
    }

    fn asked(&self) -> usize {
        self.asked.lock().unwrap().len()
    }

    fn last(&self) -> (String, Option<Control>) {
        self.asked.lock().unwrap().last().cloned().unwrap()
    }
}

/// `org.a11y.Bus` on the session bus: where the accessibility bus is.
struct A11yBusName {
    address: String,
}

#[zbus::interface(name = "org.a11y.Bus")]
impl A11yBusName {
    fn get_address(&self) -> String {
        self.address.clone()
    }
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

async fn call(conn: &Connection, method: &str, body: &(impl serde::Serialize + zbus::zvariant::DynamicType)) -> zbus::Result<zbus::Message> {
    conn.call_method(Some("org.gnome.Shell"), OBJECT_PATH, Some(IFACE), method, body).await
}

type Opts = HashMap<&'static str, Value<'static>>;

fn opts(pairs: &[(&'static str, Value<'static>)]) -> Opts {
    pairs.iter().cloned().collect()
}

fn quiet() -> Opts {
    opts(&[("interactive", Value::from(false))])
}

type Node = HashMap<String, OwnedValue>;

fn text(n: &Node, key: &str) -> String {
    n.get(key).and_then(|v| String::try_from(v.clone()).ok()).unwrap_or_default()
}

fn states(n: &Node) -> Vec<String> {
    n.get("states").and_then(|v| <Vec<String>>::try_from(v.clone()).ok()).unwrap_or_default()
}

/// The nodes of window `window`'s tree.
async fn tree(conn: &Connection, window: u32, o: Opts) -> zbus::Result<Vec<Node>> {
    let reply = call(conn, "A11yTree", &(window, o)).await?;
    let mut root: HashMap<String, OwnedValue> = reply.body().deserialize()?;
    let nodes = root.remove("nodes").expect("nodes");
    Ok(<Vec<Node>>::try_from(nodes).expect("a list of nodes"))
}

fn find<'a>(nodes: &'a [Node], role: &str, name: &str) -> &'a Node {
    nodes.iter().find(|n| text(n, "role") == role && text(n, "name") == name).unwrap_or_else(|| {
        let outline: Vec<String> = nodes.iter().map(|n| format!("{} {:?} {}", text(n, "role"), text(n, "name"), text(n, "id"))).collect();
        panic!("no {role} {name:?} in {outline:#?}")
    })
}

/// Every line of the activity log in `dir`.
fn activity(dir: &Path) -> Vec<serde_json::Value> {
    let mut days: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "jsonl")).collect();
    days.sort();
    days.iter().flat_map(|d| std::fs::read_to_string(d).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect::<Vec<_>>()).collect()
}

fn win(id: u32, app: &str, title: &str, pid: u32) -> WindowInfo {
    WindowInfo { id, app_id: app.into(), title: title.into(), pid, geometry: (0, 0, 420, 520), ..Default::default() }
}

/// Wait until the app said `line` (on its standard output).
async fn said(out: &Arc<Mutex<Vec<String>>>, line: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if out.lock().unwrap().iter().any(|l| l == line) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread")]
async fn apps_over_the_accessibility_bus() {
    let Some(comp) = sibling_binary("helix-comp") else {
        eprintln!("helix-comp not built; skipping");
        return;
    };
    if !gtk4_python() {
        eprintln!("GTK 4 for Python is not installed; skipping");
        return;
    }
    let tmp = std::env::temp_dir().join(format!("helix-agent-a11y-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let schemas = tmp.join("schemas");
    std::fs::create_dir_all(&schemas).unwrap();
    let data = concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/org.helixdesktop.agent-access.gschema.xml");
    std::fs::copy(data, schemas.join("org.helixdesktop.agent-access.gschema.xml")).unwrap();
    let Ok(compiled) = Command::new("glib-compile-schemas").arg(&schemas).status() else {
        eprintln!("glib-compile-schemas is not installed; skipping");
        return;
    };
    assert!(compiled.success());
    let Some(a11y) = A11yBus::start() else { return };
    let Some(display) = HeadlessSession::start(Options { comp: &comp, shell: None, size: (800, 600), settings: "", env: Vec::new() }) else { return };
    let bus = display.bus();
    let home = tmp.join("home");
    for dir in [&home, &tmp.join("config"), &tmp.join("data"), &tmp.join("cache")] {
        std::fs::create_dir_all(dir).unwrap();
    }
    // SAFETY: this test binary runs this one test, before any GLib use.
    unsafe {
        std::env::set_var("GSETTINGS_SCHEMA_DIR", &schemas);
        std::env::set_var("GSETTINGS_BACKEND", "memory");
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_CONFIG_HOME", tmp.join("config"));
        std::env::set_var("XDG_STATE_HOME", tmp.join("state"));
        std::env::set_var("XDG_CACHE_HOME", tmp.join("cache"));
        std::env::set_var("XDG_DATA_HOME", tmp.join("data"));
        std::env::set_var("XDG_DATA_DIRS", tmp.join("sys"));
        std::env::set_var("DBUS_SESSION_BUS_ADDRESS", bus.address());
        std::env::set_var("DBUS_SYSTEM_BUS_ADDRESS", bus.address());
        // The shell finds the accessibility bus through org.a11y.Bus.
        std::env::remove_var("AT_SPI_BUS_ADDRESS");
    }
    let _a11y_name = bus.builder().serve_at("/org/a11y/bus", A11yBusName { address: a11y.address().to_owned() }).unwrap().name("org.a11y.Bus").unwrap().build().await.unwrap();

    // The GTK 4 window, on the headless compositor and the private
    // accessibility bus.
    let spawn_app = |program: &Path| {
        Command::new("/usr/bin/python3")
            .arg(program)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &home)
            .env("XDG_RUNTIME_DIR", display.home().join("run"))
            .env("XDG_CONFIG_HOME", tmp.join("config"))
            .env("XDG_CACHE_HOME", tmp.join("cache"))
            .env("XDG_DATA_HOME", tmp.join("data"))
            .env("WAYLAND_DISPLAY", &display.wayland_display)
            .env("GDK_BACKEND", "wayland")
            .env("GSETTINGS_BACKEND", "memory")
            .env("LANG", "C.UTF-8")
            .env("DBUS_SESSION_BUS_ADDRESS", bus.address())
            .env("AT_SPI_BUS_ADDRESS", a11y.address())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start a test window")
    };
    // Its standard output, line by line.
    let listen = |child: &mut std::process::Child| {
        let out: Arc<Mutex<Vec<String>>> = Default::default();
        let lines = out.clone();
        let stdout = child.stdout.take().unwrap();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
                lines.lock().unwrap().push(line);
            }
        });
        out
    };
    let mut app = spawn_app(&write_gtk4_controls(&tmp));
    let app_pid = app.id();
    let out = listen(&mut app);
    assert!(said(&out, "ready").await, "the GTK 4 window did not start");

    // The UI: which windows there are.
    let snapshot = Arc::new(Mutex::new(Snapshot { windows: vec![win(21, APP, "A11y Test", app_pid)], focused: 21, workspaces: 1, ..Default::default() }));
    let (ui, mut ui_rx) = tokio::sync::mpsc::unbounded_channel::<UiEvent>();
    let shown = snapshot.clone();
    tokio::spawn(async move {
        while let Some(event) = ui_rx.recv().await {
            if let UiEvent::AgentSnapshot(slot) = event {
                if let Some(reply) = slot.take() {
                    let _ = reply.send(shown.lock().unwrap().clone());
                }
            }
        }
    });
    let settings = gio::Settings::new("org.helixdesktop.agent-access");
    let lock: SharedStatus = Default::default();
    let asked_consent = Arc::new(AtomicUsize::new(0));
    let counted = asked_consent.clone();
    let consent: agent::Consent = Arc::new(move |_| {
        counted.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Answer::Session })
    });
    let confirm = FakeConfirm::default();
    let log = tmp.join("state/helix/agent-activity");
    let access = AgentAccess::new(lock.clone(), Setup { grants_path: None, consent, confirm: confirm.hook(), proc_root: "/proc".into(), settings_exes: vec![], log_dir: Some(log.clone()), reminders_path: None });
    access.set_desktop(Desktop { ui: ui.clone(), notifications: None });
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
    let conn = bus.connect().await;

    // The interface is the contract's, with the Control apps methods.
    let actual = introspect(&conn, "org.gnome.Shell", OBJECT_PATH).await;
    assert_conformant(IFACE, &check(IFACE, &load_dbus_contract("../agent-access/org.helixdesktop.Agent1.xml"), &actual, &[]));

    let every = |o: Opts| {
        let conn = conn.clone();
        async move {
            vec![
                ("A11yTree", call(&conn, "A11yTree", &(21u32, o.clone())).await),
                ("A11yFind", call(&conn, "A11yFind", &(21u32, o.clone())).await),
                ("A11yAction", call(&conn, "A11yAction", &(21u32, "x", "click", o.clone())).await),
                ("A11ySetValue", call(&conn, "A11ySetValue", &(21u32, "x", Value::from("typed"), o.clone())).await),
            ]
        }
    };
    // Off: nothing.
    for (method, r) in every(quiet()).await {
        assert_eq!(short(&r), "AccessOff", "{method}");
    }
    // On at Control the desktop: the level is too low.
    settings.set_boolean("enabled", true).unwrap();
    settings.set_boolean("allow-remote", true).unwrap();
    settings.set_string("access-level", "desktop").unwrap();
    for (method, r) in every(quiet()).await {
        assert_eq!((short(&r), message(&r)), ("LevelTooLow".to_owned(), "apps".to_owned()), "{method}");
    }
    // Control apps, but no grant and nobody to ask.
    settings.set_string("access-level", "apps").unwrap();
    for (method, r) in every(quiet()).await {
        assert_eq!(short(&r), "NotGranted", "{method}");
    }
    // Locked: nothing, granted or not.
    lock.lock().unwrap().locked_since = Some(Instant::now());
    for (method, r) in every(opts(&[])).await {
        assert_eq!(short(&r), "Locked", "{method}");
    }
    lock.lock().unwrap().locked_since = None;
    assert_eq!(asked_consent.load(Ordering::SeqCst), 0);

    // Asked, the user allows Control apps. The app shows up on the
    // accessibility bus a little after its window.
    let deadline = Instant::now() + Duration::from_secs(15);
    let nodes = loop {
        match tree(&conn, 21, opts(&[])).await {
            Ok(nodes) => break nodes,
            Err(err) => {
                assert!(Instant::now() < deadline, "the window's tree was not read: {err:?}");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    };
    assert_eq!(asked_consent.load(Ordering::SeqCst), 1);

    // The tree: the window first, its controls, short ids.
    assert_eq!((text(&nodes[0], "role"), text(&nodes[0], "name"), text(&nodes[0], "parent")), ("frame".to_owned(), "A11y Test".to_owned(), String::new()));
    let press = find(&nodes, "button", "Press me").clone();
    let press_id = text(&press, "id");
    assert!(!press_id.contains('/'), "ids are short: {press_id}");
    assert!(<Vec<String>>::try_from(press["actions"].clone()).unwrap().contains(&"click".to_owned()));
    assert!(!text(&press, "parent").is_empty());
    // Reading text fields is off (the default): the field and its label,
    // never its text, and finding by name does not look at the text.
    let name = find(&nodes, "text", "Name field").clone();
    assert!(!name.contains_key("value"), "{name:?}");
    assert!(bool::try_from(name["value-hidden"].clone()).unwrap());
    assert!(!name.contains_key("password"));
    assert!(states(&name).contains(&"editable".to_owned()));
    assert!(!format!("{nodes:?}").contains("hello"), "a field's text read with the switch off");
    let reply = call(&conn, "A11yFind", &(21u32, opts(&[("name", Value::from("hello"))]))).await.unwrap();
    assert!(reply.body().deserialize::<Vec<Node>>().unwrap().is_empty(), "found by its text");
    let reply = call(&conn, "A11yFind", &(21u32, opts(&[("role", Value::from("text")), ("name", Value::from("Name field"))]))).await.unwrap();
    let found: Vec<Node> = reply.body().deserialize().unwrap();
    assert_eq!(found.len(), 1);
    assert!(!found[0].contains_key("value") && bool::try_from(found[0]["value-hidden"].clone()).unwrap());
    // Labels and numbers are not text fields.
    assert!(find(&nodes, "label", "Clicks: 0").get("value-hidden").is_none());
    assert_eq!(f64::try_from(find(&nodes, "slider", "Level")["value"].clone()).unwrap(), 0.0);
    // On, at once: the text is read.
    settings.set_boolean("read-text-fields", true).unwrap();
    let nodes = tree(&conn, 21, opts(&[])).await.unwrap();
    let name = find(&nodes, "text", "Name field").clone();
    assert_eq!(text(&name, "value"), "hello", "a field's text is read");
    assert!(!name.contains_key("value-hidden"));
    // Password fields: no value, and marked.
    let password = find(&nodes, "password text", "Password").clone();
    let pin = find(&nodes, "text", "PIN").clone();
    for field in [&password, &pin] {
        assert!(!field.contains_key("value"), "{field:?}");
        assert!(bool::try_from(field["password"].clone()).unwrap());
    }
    let level = find(&nodes, "slider", "Level").clone();
    assert_eq!(f64::try_from(level["value"].clone()).unwrap(), 0.0);
    assert_eq!(f64::try_from(level["max"].clone()).unwrap(), 100.0);
    let all_text = format!("{nodes:?}");
    for secret in ["hunter2", "secret"] {
        assert!(!all_text.contains(secret), "{secret} read");
    }
    // Depth and size limits, and a subtree.
    let shallow = tree(&conn, 21, opts(&[("depth", Value::from(1u32))])).await.unwrap();
    assert!(shallow.len() < nodes.len());
    let limited = call(&conn, "A11yTree", &(21u32, opts(&[("max-nodes", Value::from(3u32))]))).await.unwrap();
    let limited: HashMap<String, OwnedValue> = limited.body().deserialize().unwrap();
    assert_eq!(<Vec<Node>>::try_from(limited["nodes"].clone()).unwrap().len(), 3);
    assert!(bool::try_from(limited["truncated"].clone()).unwrap());
    let sub = tree(&conn, 21, opts(&[("node", Value::from(press_id.clone()))])).await.unwrap();
    assert_eq!(text(&sub[0], "id"), press_id);
    let bounded = tree(&conn, 21, opts(&[("bounds", Value::from(true)), ("node", Value::from(press_id.clone())), ("depth", Value::from(0u32))])).await.unwrap();
    assert!(i32::try_from(bounded[0]["width"].clone()).unwrap() > 0, "{bounded:?}");
    assert_eq!(short(&call(&conn, "A11yTree", &(21u32, opts(&[("depth", Value::from("deep"))]))).await), "InvalidArgs");

    // Find: by role and name, and by state.
    let reply = call(&conn, "A11yFind", &(21u32, opts(&[("role", Value::from("button")), ("name", Value::from("press"))]))).await.unwrap();
    let found: Vec<Node> = reply.body().deserialize().unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(text(&found[0], "id"), press_id);
    let reply = call(&conn, "A11yFind", &(21u32, opts(&[("states", Value::from(vec!["editable"]))]))).await.unwrap();
    let editable: Vec<Node> = reply.body().deserialize().unwrap();
    assert!(editable.len() >= 3, "{editable:?}");
    let r = call(&conn, "A11yFind", &(21u32, opts(&[("name", Value::from("Nothing like it")), ("timeout-ms", Value::from(600u32))]))).await;
    assert_eq!(short(&r), "TimedOut");
    assert_eq!(short(&call(&conn, "A11yFind", &(21u32, opts(&[("role", Value::from("gizmo"))]))).await), "InvalidArgs");

    // Press, and the default action.
    call(&conn, "A11yAction", &(21u32, press_id.as_str(), "click", opts(&[]))).await.unwrap();
    assert!(said(&out, "clicked 1").await);
    call(&conn, "A11yAction", &(21u32, press_id.as_str(), "default", opts(&[]))).await.unwrap();
    assert!(said(&out, "clicked 2").await);
    // Wait for what it changed.
    let reply = call(&conn, "A11yFind", &(21u32, opts(&[("role", Value::from("label")), ("name", Value::from("Clicks: 2")), ("timeout-ms", Value::from(5000u32))]))).await.unwrap();
    assert_eq!(reply.body().deserialize::<Vec<Node>>().unwrap().len(), 1);
    assert_eq!(short(&call(&conn, "A11yAction", &(21u32, press_id.as_str(), "fly", opts(&[]))).await), "NotSupported");
    assert_eq!(short(&call(&conn, "A11yAction", &(21u32, press_id.as_str(), "", opts(&[]))).await), "InvalidArgs");

    // Fill in a field, move the slider (a number, or text that is one).
    let name_id = text(&name, "id");
    call(&conn, "A11ySetValue", &(21u32, name_id.as_str(), Value::from("world"), opts(&[]))).await.unwrap();
    assert!(said(&out, "name length 5").await);
    let now = tree(&conn, 21, opts(&[])).await.unwrap();
    assert_eq!(text(find(&now, "text", "Name field"), "value"), "world");
    // Off: setting a field's text still works, reading it back does not.
    settings.set_boolean("read-text-fields", false).unwrap();
    call(&conn, "A11ySetValue", &(21u32, name_id.as_str(), Value::from("planet"), opts(&[]))).await.unwrap();
    assert!(said(&out, "name length 6").await);
    let now = tree(&conn, 21, opts(&[])).await.unwrap();
    let field = find(&now, "text", "Name field");
    assert!(!field.contains_key("value") && bool::try_from(field["value-hidden"].clone()).unwrap(), "{field:?}");
    assert!(!format!("{now:?}").contains("planet"));
    let sub = tree(&conn, 21, opts(&[("node", Value::from(name_id.clone()))])).await.unwrap();
    assert!(!sub[0].contains_key("value"), "{:?}", sub[0]);
    let level_id = text(&level, "id");
    call(&conn, "A11ySetValue", &(21u32, level_id.as_str(), Value::from(42.0), opts(&[]))).await.unwrap();
    assert!(said(&out, "level 42").await);
    call(&conn, "A11ySetValue", &(21u32, level_id.as_str(), Value::from("17"), opts(&[]))).await.unwrap();
    assert!(said(&out, "level 17").await);
    assert_eq!(short(&call(&conn, "A11ySetValue", &(21u32, level_id.as_str(), Value::from(500.0), opts(&[]))).await), "InvalidArgs");
    assert_eq!(short(&call(&conn, "A11ySetValue", &(21u32, press_id.as_str(), Value::from("x"), opts(&[]))).await), "NotSupported");
    // Focus: GTK 4 cannot be given it over AT-SPI (it answers
    // NotSupported to GrabFocus); GTK 3 can, below.
    let r = call(&conn, "A11yAction", &(21u32, name_id.as_str(), "focus", opts(&[]))).await;
    assert_eq!((short(&r), message(&r)), ("NotSupported".to_owned(), "the app does not let its focus be moved over accessibility".to_owned()));

    // Password fields: never set, used or focused, nor read from.
    let excluded = |r: &zbus::Result<zbus::Message>| (short(r), message(r));
    let password_field = ("Excluded".to_owned(), "password-field".to_owned());
    for id in [text(&password, "id"), text(&pin, "id")] {
        assert_eq!(excluded(&call(&conn, "A11ySetValue", &(21u32, id.as_str(), Value::from("guess"), opts(&[]))).await), password_field);
        assert_eq!(excluded(&call(&conn, "A11yAction", &(21u32, id.as_str(), "activate", opts(&[]))).await), password_field);
        assert_eq!(excluded(&call(&conn, "A11yAction", &(21u32, id.as_str(), "focus", opts(&[]))).await), password_field);
        assert_eq!(excluded(&call(&conn, "A11yTree", &(21u32, opts(&[("node", Value::from(id.clone()))]))).await), password_field);
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!out.lock().unwrap().iter().any(|l| l.contains("password") || l.contains("pin")), "{:?}", out.lock().unwrap());

    // Nodes that are not this window's.
    assert_eq!(short(&call(&conn, "A11yAction", &(21u32, "no_such_node", "click", opts(&[]))).await), "NotFound");
    assert_eq!(short(&call(&conn, "A11yAction", &(21u32, "/org/a11y/atspi/accessible/root", "click", opts(&[]))).await), "NotFound");
    assert_eq!(short(&call(&conn, "A11yTree", &(99u32, opts(&[]))).await), "NotFound");

    // A clipboard action needs the clipboard switch.
    let labels: Vec<&Node> = now.iter().filter(|n| <Vec<String>>::try_from(n["actions"].clone()).unwrap_or_default().contains(&"clipboard.copy".to_owned())).collect();
    if let Some(label) = labels.first() {
        let r = call(&conn, "A11yAction", &(21u32, text(label, "id").as_str(), "clipboard.copy", opts(&[]))).await;
        assert_eq!((short(&r), message(&r)), ("FeatureOff".to_owned(), "clipboard".to_owned()));
    }

    // "Delete All" is sensitive: the prompt names it; declined, unasked
    // and allowed.
    let delete_id = text(find(&now, "button", "Delete All"), "id");
    let before = confirm.asked();
    confirm.queue(Some(false));
    assert_eq!(short(&call(&conn, "A11yAction", &(21u32, delete_id.as_str(), "click", opts(&[]))).await), "DeniedByUser");
    assert_eq!(confirm.asked(), before + 1);
    let (method, control) = confirm.last();
    assert_eq!(method, "A11yAction");
    assert_eq!(control, Some(Control { name: "Delete All".into(), role: "button", app: APP.into() }));
    assert_eq!(short(&call(&conn, "A11yAction", &(21u32, delete_id.as_str(), "click", quiet())).await), "NeedsConfirmation");
    assert_eq!(confirm.asked(), before + 1, "not asked without interactive");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!out.lock().unwrap().iter().any(|l| l == "deleted"));
    confirm.queue(Some(true));
    call(&conn, "A11yAction", &(21u32, delete_id.as_str(), "click", opts(&[]))).await.unwrap();
    assert!(said(&out, "deleted").await);
    // Focusing it is not sensitive (GTK 4 then says it cannot); an agent
    // marks anything sensitive.
    assert_eq!(short(&call(&conn, "A11yAction", &(21u32, delete_id.as_str(), "focus", opts(&[]))).await), "NotSupported");
    assert_eq!(confirm.asked(), before + 2);
    confirm.queue(Some(true));
    call(&conn, "A11ySetValue", &(21u32, name_id.as_str(), Value::from("again"), opts(&[("sensitive", Value::from(true))]))).await.unwrap();
    assert_eq!(confirm.asked(), before + 3);
    assert_eq!(confirm.last().0, "A11ySetValue");
    settings.set_boolean("confirm-sensitive", false).unwrap();
    call(&conn, "A11yAction", &(21u32, delete_id.as_str(), "click", opts(&[]))).await.unwrap();
    assert_eq!(confirm.asked(), before + 3, "not asked with confirm-sensitive off");
    settings.set_boolean("confirm-sensitive", true).unwrap();

    // A system prompt is shown: nothing.
    snapshot.lock().unwrap().system_prompt = true;
    for (method, r) in every(opts(&[])).await {
        assert_eq!(excluded(&r), ("Excluded".to_owned(), "system-prompt".to_owned()), "{method}");
    }
    snapshot.lock().unwrap().system_prompt = false;

    // Excluded apps and Settings: their windows, and every window of
    // their processes.
    let set_windows = |windows: Vec<WindowInfo>| snapshot.lock().unwrap().windows = windows;
    settings.set_strv("excluded-apps", ["org.example.Vault"]).unwrap();
    set_windows(vec![win(21, APP, "A11y Test", app_pid), win(22, "org.example.Vault", "Vault", app_pid)]);
    for window in [21u32, 22] {
        let r = call(&conn, "A11yTree", &(window, opts(&[]))).await;
        assert_eq!(excluded(&r), ("Excluded".to_owned(), "app:org.example.Vault".to_owned()), "window {window}");
    }
    set_windows(vec![win(21, APP, "A11y Test", app_pid), win(23, "org.helixdesktop.Settings", "Settings", app_pid)]);
    for window in [21u32, 23] {
        let r = call(&conn, "A11yAction", &(window, press_id.as_str(), "click", opts(&[]))).await;
        assert_eq!(excluded(&r), ("Excluded".to_owned(), "settings".to_owned()), "window {window}");
    }
    // The shell's own process.
    set_windows(vec![win(21, APP, "A11y Test", app_pid), win(24, "org.example.Prompt", "Prompt", std::process::id())]);
    assert_eq!(excluded(&call(&conn, "A11yTree", &(24u32, opts(&[]))).await), ("Excluded".to_owned(), "shell".to_owned()));
    // Not known which process: not guessed.
    set_windows(vec![win(21, APP, "A11y Test", app_pid), win(25, "org.example.Unknown", "Unknown", 0)]);
    assert_eq!(short(&call(&conn, "A11yTree", &(25u32, opts(&[]))).await), "NotSupported");
    // Two windows of one process: told apart by title, or not at all.
    set_windows(vec![win(21, APP, "A11y Test", app_pid), win(26, APP, "Another window", app_pid)]);
    assert_eq!(short(&call(&conn, "A11yTree", &(26u32, opts(&[]))).await), "NotSupported");
    tree(&conn, 21, opts(&[])).await.unwrap();
    // What the app told the compositor: used when its process owns the
    // name, refused when not.
    let a11y_conn = a11y.connect().await;
    let apps: Vec<(String, OwnedObjectPath)> = a11y_conn
        .call_method(Some("org.a11y.atspi.Registry"), "/org/a11y/atspi/accessible/root", Some("org.a11y.atspi.Accessible"), "GetChildren", &())
        .await
        .unwrap()
        .body()
        .deserialize()
        .unwrap();
    let dbus = zbus::fdo::DBusProxy::new(&a11y_conn).await.unwrap();
    let mut app_name = String::new();
    for (name, _) in &apps {
        if dbus.get_connection_unix_process_id(zbus::names::BusName::try_from(name.as_str()).unwrap()).await.ok() == Some(app_pid) {
            app_name = name.clone();
        }
    }
    assert!(!app_name.is_empty(), "the app is on the accessibility bus");
    let frame_path = format!("{}{}", {
        let root = text(&nodes[0], "id");
        // The frame's id is its path's last part; find the full path from
        // the app's own top-level objects.
        let tops: Vec<(String, OwnedObjectPath)> = a11y_conn.call_method(Some(app_name.as_str()), "/org/a11y/atspi/accessible/root", Some("org.a11y.atspi.Accessible"), "GetChildren", &()).await.unwrap().body().deserialize().unwrap();
        let full = tops.iter().map(|(_, p)| p.to_string()).find(|p| p.ends_with(&root)).expect("the frame");
        full.strip_suffix(&root).unwrap().to_owned()
    }, text(&nodes[0], "id"));
    let told = |name: &str| WindowInfo { a11y_name: name.into(), a11y_path: frame_path.clone(), ..win(27, APP, "Renamed", app_pid) };
    set_windows(vec![told(&app_name)]);
    let by_name = tree(&conn, 27, opts(&[])).await.unwrap();
    assert_eq!(text(&by_name[0], "name"), "A11y Test", "the told object, whatever the title");
    set_windows(vec![told("org.a11y.atspi.Registry")]);
    assert_eq!(short(&call(&conn, "A11yTree", &(27u32, opts(&[]))).await), "NotSupported", "a name of another process");
    set_windows(vec![win(21, APP, "A11y Test", app_pid)]);

    // A GTK 3 window (ATK's bridge, which tells the compositor nothing):
    // found by its process, and it takes the focus.
    if gtk3_python() {
        let mut app3 = spawn_app(&write_gtk3_controls(&tmp));
        let out3 = listen(&mut app3);
        assert!(said(&out3, "ready").await, "the GTK 3 window did not start");
        set_windows(vec![win(21, APP, "A11y Test", app_pid), win(31, "notes-test", "Notes Test", app3.id())]);
        let deadline = Instant::now() + Duration::from_secs(15);
        let nodes3 = loop {
            match tree(&conn, 31, opts(&[])).await {
                Ok(nodes) => break nodes,
                Err(err) => {
                    assert!(Instant::now() < deadline, "the GTK 3 window's tree was not read: {err:?}");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        };
        assert_eq!(text(&nodes3[0], "name"), "Notes Test");
        let notes_id = text(find(&nodes3, "text", "Notes"), "id");
        call(&conn, "A11yAction", &(31u32, notes_id.as_str(), "focus", opts(&[]))).await.unwrap();
        assert!(said(&out3, "notes focused").await);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let now = tree(&conn, 31, opts(&[])).await.unwrap();
            if states(find(&now, "text", "Notes")).contains(&"focused".to_owned()) {
                break;
            }
            assert!(Instant::now() < deadline, "the field did not get the focus");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let ok_id = text(find(&nodes3, "button", "OK"), "id");
        call(&conn, "A11yAction", &(31u32, ok_id.as_str(), "click", opts(&[]))).await.unwrap();
        assert!(said(&out3, "ok").await);
        // A node of the other app's window is not this one's; nor is a
        // path ATK's bridge would answer for although it never gave it
        // (it reads the number the path starts with).
        let r = call(&conn, "A11yAction", &(31u32, press_id.as_str(), "click", opts(&[]))).await;
        assert_eq!(short(&r), "NotFound", "{}", message(&r));
        let lookalike = format!("{ok_id}_not_it");
        let r = call(&conn, "A11yAction", &(31u32, lookalike.as_str(), "click", opts(&[]))).await;
        assert_eq!(short(&r), "NotFound", "{}", message(&r));
        let _ = app3.kill();
        let _ = app3.wait();
        set_windows(vec![win(21, APP, "A11y Test", app_pid)]);
    } else {
        eprintln!("GTK 3 for Python is not installed; focusing a field is left out");
    }

    // The log: what was done, by kind and length; none of the window's
    // words, values or node ids.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let lines = activity(&log);
    let target = format!("window:21 ({APP})");
    let read = lines.iter().find(|l| l["method"] == "A11yTree" && l["outcome"] == "allowed" && l["target"] == target.as_str() && l.get("error").is_none()).expect("a tree read");
    assert_eq!(read["level"], "apps");
    assert!(read["node-count"].as_u64().unwrap() > 5);
    // Withheld values, by count.
    assert!(lines.iter().any(|l| l["method"] == "A11yTree" && l["hidden-count"].as_u64().is_some_and(|n| n >= 1)), "{lines:?}");
    assert!(lines.iter().any(|l| l["method"] == "A11yTree" && l["outcome"] == "allowed" && l.get("error").is_none() && l.get("hidden-count").is_none()), "with the switch on, nothing withheld");
    let pressed = lines.iter().find(|l| l["method"] == "A11yAction" && l["action"] == "click" && l["role"] == "button" && l["outcome"] == "allowed").expect("a press");
    assert!(pressed.get("error").is_none());
    let set = lines.iter().find(|l| l["method"] == "A11ySetValue" && l["text-length"] == 5).expect("the text set, by length");
    assert_eq!((set["action"].as_str(), set["role"].as_str()), (Some("set-text"), Some("text")));
    assert!(lines.iter().any(|l| l["method"] == "A11ySetValue" && l["outcome"] == "refused:Excluded" && l["role"] == "password text"));
    assert!(lines.iter().any(|l| l["method"] == "A11yAction" && l["outcome"] == "refused:FeatureOff") || labels.is_empty());
    assert!(lines.iter().any(|l| l["method"] == "SensitivePrompt" && l["target"] == "A11yAction" && l["outcome"] == "refused:DeniedByUser"));
    assert!(lines.iter().any(|l| l["method"] == "A11yFind" && l["error"] == "TimedOut" && l["name-length"] == 15));
    assert!(lines.iter().any(|l| l["method"] == "A11yTree" && l["outcome"] == "refused:Excluded" && l["target"] == "window:22 (org.example.Vault)"));
    let raw: String = std::fs::read_dir(&log).unwrap().flatten().map(|e| std::fs::read_to_string(e.path()).unwrap()).collect();
    for word in ["hello", "world", "planet", "again", "hunter2", "secret", "guess", "Press me", "Delete All", "Clicks", "Name field", "A11y Test", "typed", &press_id, &name_id, "/org/"] {
        assert!(!raw.contains(word), "the log holds {word:?}:\n{raw}");
    }

    let _ = app.kill();
    let _ = app.wait();
    drop(display);
    drop(a11y);
    let _ = std::fs::remove_dir_all(&tmp);
}
