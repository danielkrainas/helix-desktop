//! Agent Access's *Control the desktop* methods over a private bus
//! (helixdesktop/spec `contracts/agent-access/README.md`, "Control the
//! desktop methods"; helixdesktop/desktop#147): opening and launching
//! through the shell's launcher (scratch apps that only write down how
//! they were started), window and workspace requests to a stand-in UI,
//! quick settings in in-memory settings, a fake `pactl`, fake
//! NetworkManager and backlight services, the agent's notifications and
//! their rate limit; and the refusals: access off, the level, a read-only
//! grant, locked, excluded apps and Settings, and the sensitive-action
//! prompt allowed, denied and unanswered, all in the activity log.
//! Nothing on the host is touched: scratch XDG directories, in-memory
//! settings, one private bus as both session and system bus.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gio::prelude::*;
use helix_shell::agent::actions::WindowOp;
use helix_shell::agent::desktop::{Snapshot, WindowInfo, FULLSCREEN, MAXIMIZED, TILED_LEFT, TILED_RIGHT};
use helix_shell::agent::grants::Answer;
use helix_shell::agent::{self, AgentAccess, Agent1, AgentAccessSettings, Confirm, Desktop, Setup, OBJECT_PATH};
use helix_shell::notifications::Center;
use helix_shell::screensaver::SharedStatus;
use helix_shell::windows::WindowAction;
use helix_shell::UiEvent;
use helix_test_support::conformance::{assert_conformant, check, introspect, load_dbus_contract};
use helix_test_support::logind::FakeLogind;
use helix_test_support::TestBus;
use zbus::zvariant::{OwnedValue, Value};
use zbus::{interface, Connection};

const IFACE: &str = "org.helixdesktop.Agent1";

/// What the sensitive-action prompt was asked: the method and its action.
type Asked = Arc<Mutex<Vec<(String, Option<String>)>>>;

/// Sensitive-action answers, in order (`None`: unanswered when none is
/// queued), and what was asked.
#[derive(Clone, Default)]
struct FakeConfirm {
    answers: Arc<Mutex<VecDeque<Option<bool>>>>,
    asked: Asked,
}

impl FakeConfirm {
    fn hook(&self) -> Confirm {
        let me = self.clone();
        Arc::new(move |request| {
            me.asked.lock().unwrap().push((request.method.to_owned(), request.action.map(str::to_owned)));
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

    fn last(&self) -> (String, Option<String>) {
        self.asked.lock().unwrap().last().cloned().unwrap()
    }
}

/// NetworkManager's Wi-Fi switch.
struct Nm(Arc<Mutex<bool>>);

#[interface(name = "org.freedesktop.NetworkManager")]
impl Nm {
    #[zbus(property)]
    fn wireless_enabled(&self) -> bool {
        *self.0.lock().unwrap()
    }
    #[zbus(property)]
    fn set_wireless_enabled(&mut self, on: bool) {
        *self.0.lock().unwrap() = on;
    }
    #[zbus(property)]
    fn wwan_enabled(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn set_wwan_enabled(&mut self, _on: bool) {}
}

/// The compositor's backlight, as mutter's DisplayConfig has it.
struct Backlight(Arc<Mutex<i32>>);

#[interface(name = "org.gnome.Mutter.DisplayConfig")]
impl Backlight {
    #[zbus(property)]
    fn backlight(&self) -> (u32, Vec<HashMap<String, OwnedValue>>) {
        let v = |n: i32| OwnedValue::from(n);
        let entry = HashMap::from([
            ("connector".to_owned(), OwnedValue::try_from(Value::from("eDP-1")).unwrap()),
            ("min".to_owned(), v(0)),
            ("max".to_owned(), v(200)),
            ("value".to_owned(), v(*self.0.lock().unwrap())),
        ]);
        (1, vec![entry])
    }
    fn set_backlight(&self, _serial: u32, _connector: &str, value: i32) {
        *self.0.lock().unwrap() = value;
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

fn win(id: u32, app: &str, workspace: u32) -> WindowInfo {
    WindowInfo { id, app_id: app.into(), title: format!("{app} window"), workspace, geometry: (100, 100, 640, 480), ..Default::default() }
}

/// What the compositor would do with `op`, on the stand-in's windows.
fn apply(s: &mut Snapshot, op: WindowOp) {
    let find = |s: &mut Snapshot, id: u32| s.windows.iter().position(|w| w.id == id);
    match op {
        WindowOp::Activate(id) => {
            if let Some(i) = find(s, id) {
                s.windows[i].minimized = false;
                s.active = s.windows[i].workspace;
                s.focused = id;
            }
        }
        WindowOp::Action(id, action) => {
            let Some(i) = find(s, id) else { return };
            let w = &mut s.windows[i];
            match action {
                WindowAction::Close => {
                    s.windows.remove(i);
                }
                WindowAction::Minimize => w.minimized = true,
                WindowAction::Maximize => w.state = MAXIMIZED,
                WindowAction::Unmaximize => w.state = 0,
                WindowAction::ToggleFullscreen => w.state ^= FULLSCREEN,
                WindowAction::ToggleTiledLeft => w.state = TILED_LEFT,
                WindowAction::ToggleTiledRight => w.state = TILED_RIGHT,
                _ => {}
            }
        }
        WindowOp::MoveResize { id, x, y, width, height } => {
            if let Some(i) = find(s, id) {
                s.windows[i].geometry = (x, y, width, height);
                s.windows[i].state = 0;
            }
        }
        WindowOp::ToWorkspace { id, workspace } => {
            if let Some(i) = find(s, id) {
                s.windows[i].workspace = workspace;
            }
        }
        WindowOp::SwitchWorkspace(n) => s.active = n,
    }
}

/// A script that writes down how it was started, to `log`.
fn recorder(path: &Path, log: &Path) {
    std::fs::write(path, format!("#!/bin/sh\necho \"$(basename \"$0\") $*\" >> {}\n", log.display())).unwrap();
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}

fn desktop_file(dir: &Path, id: &str, name: &str, exec: &str) {
    std::fs::write(dir.join(format!("{id}.desktop")), format!("[Desktop Entry]\nType=Application\nName={name}\nExec={exec} %U\n")).unwrap();
}

/// Wait until `log` has a line containing each of `texts`.
async fn launched(log: &Path, texts: &[&str]) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if std::fs::read_to_string(log).unwrap_or_default().lines().any(|l| texts.iter().all(|t| l.contains(t))) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// Every line of the activity log in `dir`.
fn activity(dir: &Path) -> Vec<serde_json::Value> {
    let mut days: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "jsonl")).collect();
    days.sort();
    days.iter().flat_map(|d| std::fs::read_to_string(d).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect::<Vec<_>>()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn desktop_actions_over_the_bus() {
    let Some(bus) = TestBus::start() else { return };
    let tmp = std::env::temp_dir().join(format!("helix-agent-desktop-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let schemas = tmp.join("schemas");
    std::fs::create_dir_all(&schemas).unwrap();
    let data = concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/org.helixdesktop.agent-access.gschema.xml");
    std::fs::copy(data, schemas.join("org.helixdesktop.agent-access.gschema.xml")).unwrap();
    // GNOME's quick settings schemas, where the host has them (read only).
    let gnome = [
        "org.gnome.desktop.interface.gschema.xml",
        "org.gnome.desktop.notifications.gschema.xml",
        "org.gnome.settings-daemon.plugins.color.gschema.xml",
        "org.gnome.desktop.sound.gschema.xml",
        "org.gnome.desktop.enums.xml",
        "org.gnome.settings-daemon.enums.xml",
    ];
    let mut quick_schemas = true;
    for file in gnome {
        let from = Path::new("/usr/share/glib-2.0/schemas").join(file);
        if std::fs::copy(&from, schemas.join(file)).is_err() {
            quick_schemas = false;
        }
    }
    if !quick_schemas {
        for file in gnome {
            let _ = std::fs::remove_file(schemas.join(file));
        }
        eprintln!("GNOME's schemas are not installed; dark style, night light and do not disturb are left out");
    }
    let Ok(compiled) = std::process::Command::new("glib-compile-schemas").arg(&schemas).status() else {
        eprintln!("glib-compile-schemas is not installed; skipping");
        return;
    };
    assert!(compiled.success());

    let home = tmp.join("home");
    let data_home = tmp.join("data");
    let apps = data_home.join("applications");
    let bin = tmp.join("bin");
    let sys = tmp.join("sys");
    for dir in [&apps, &bin, &sys, &home.join("Desktop"), &tmp.join("config"), &tmp.join("run")] {
        std::fs::create_dir_all(dir).unwrap();
    }
    // File types from the host's database (read only), and nothing else
    // of the host's data.
    if Path::new("/usr/share/mime/mime.cache").exists() {
        std::os::unix::fs::symlink("/usr/share/mime", sys.join("mime")).unwrap();
    }
    let launches = tmp.join("launched");
    for program in ["text-editor", "vault", "settings-alias", "launcher-tool"] {
        recorder(&bin.join(program), &launches);
    }
    recorder(&bin.join("helix-settings-app"), &launches);
    // A sound server: pactl, keeping the volume in a file.
    let volume = tmp.join("volume");
    std::fs::write(&volume, "40 no\n").unwrap();
    std::fs::write(
        bin.join("pactl"),
        format!(
            "#!/bin/sh\nf={f}\nread v m < $f\ncase \"$1\" in\n get-sink-volume) echo \"Volume: front-left: 1 /  $v% / 0 dB\";;\n get-sink-mute) echo \"Mute: $m\";;\n set-sink-volume) v=${{3%\\%}}; echo \"$v $m\" > $f;;\n set-sink-mute) case \"$3\" in 1) m=yes;; 0) m=no;; esac; echo \"$v $m\" > $f;;\n *) exit 1;;\nesac\n",
            f = volume.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(bin.join("pactl"), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    std::fs::write(
        tmp.join("config/mimeapps.list"),
        "[Default Applications]\ntext/plain=org.gnome.TextEditor.desktop\napplication/x-desktop=org.gnome.TextEditor.desktop\napplication/pdf=org.example.Vault.desktop\nx-scheme-handler/https=org.gnome.TextEditor.desktop\nx-scheme-handler/mailto=org.example.SettingsAlias.desktop\n",
    )
    .unwrap();
    desktop_file(&apps, "org.gnome.TextEditor", "Text Editor", &bin.join("text-editor").to_string_lossy());
    desktop_file(&apps, "org.example.Vault", "Secret Vault", &bin.join("vault").to_string_lossy());
    desktop_file(&apps, "org.helixdesktop.Settings", "Settings", &bin.join("helix-settings-app").to_string_lossy());
    // Another name for Settings: refused by its program.
    desktop_file(&apps, "org.example.SettingsAlias", "Preferences", &format!("env A=1 {}", bin.join("helix-settings-app").display()));
    std::fs::write(apps.join("org.example.Hidden.desktop"), format!("[Desktop Entry]\nType=Application\nName=Hidden\nNoDisplay=true\nExec={}\n", bin.join("text-editor").display())).unwrap();
    let notes = home.join("notes.txt");
    std::fs::write(&notes, "hello").unwrap();
    std::fs::write(home.join("report.pdf"), "%PDF-1.4\n").unwrap();
    // An executable launcher nobody trusted: a plain file.
    let tool = home.join("Desktop/tool.desktop");
    std::fs::write(&tool, format!("[Desktop Entry]\nType=Application\nName=Invoice.pdf\nExec={} RAN\n", bin.join("launcher-tool").display())).unwrap();
    std::fs::set_permissions(&tool, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

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
        std::env::set_var("PATH", format!("{}:/usr/bin:/bin", bin.display()));
        std::env::set_var("GIO_USE_VFS", "local");
        // logind and NetworkManager on the "system" bus: the private one.
        std::env::set_var("DBUS_SYSTEM_BUS_ADDRESS", bus.address());
        std::env::set_var("DBUS_SESSION_BUS_ADDRESS", bus.address());
    }

    // The UI: snapshots, and window requests done to them as the
    // compositor would.
    let snapshot = Arc::new(Mutex::new(Snapshot {
        windows: vec![win(11, "org.gnome.TextEditor", 0), win(12, "org.example.Vault", 0), win(13, "org.helixdesktop.Settings", 0), win(14, "org.gnome.Calculator", 1), win(15, "org.gnome.Calculator", 0)],
        focused: 11,
        workspaces: 2,
        active: 0,
        ..Default::default()
    }));
    let ops: Arc<Mutex<Vec<WindowOp>>> = Default::default();
    let (ui, mut ui_rx) = tokio::sync::mpsc::unbounded_channel::<UiEvent>();
    let (shown, done_ops) = (snapshot.clone(), ops.clone());
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
                        for op in &request.ops {
                            apply(&mut shown.lock().unwrap(), *op);
                        }
                        done_ops.lock().unwrap().extend(request.ops.iter().copied());
                        let _ = request.reply.send(Ok(()));
                    }
                }
                _ => {}
            }
        }
    });

    let settings = gio::Settings::new("org.helixdesktop.agent-access");
    let lock: SharedStatus = Default::default();
    let center = Center::new(ui.clone(), lock.clone());
    let asked_consent = Arc::new(AtomicUsize::new(0));
    let counted = asked_consent.clone();
    let consent: agent::Consent = Arc::new(move |_| {
        counted.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Answer::Session })
    });
    let confirm = FakeConfirm::default();
    let me = std::env::current_exe().unwrap();
    let log = tmp.join("state/helix/agent-activity");
    let access = AgentAccess::new(lock.clone(), Setup { grants_path: None, consent, confirm: confirm.hook(), proc_root: "/proc".into(), settings_exes: vec![me.clone()], log_dir: Some(log.clone()), reminders_path: None });
    access.set_desktop(Desktop { ui: ui.clone(), notifications: Some(center.clone()) });
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
    let _logind = FakeLogind::start(&bus).await;
    let wifi = Arc::new(Mutex::new(true));
    let backlight = Arc::new(Mutex::new(100));
    let _services = bus
        .builder()
        .serve_at("/org/freedesktop/NetworkManager", Nm(wifi.clone()))
        .unwrap()
        .serve_at("/org/gnome/Mutter/DisplayConfig", Backlight(backlight.clone()))
        .unwrap()
        .name("org.freedesktop.NetworkManager")
        .unwrap()
        .name("org.gnome.Mutter.DisplayConfig")
        .unwrap()
        .build()
        .await
        .unwrap();
    let conn = bus.connect().await;

    // The desktop methods are there now, as the contract has them.
    let actual = introspect(&conn, "org.gnome.Shell", OBJECT_PATH).await;
    assert_conformant(IFACE, &check(IFACE, &load_dbus_contract("../agent-access/org.helixdesktop.Agent1.xml"), &actual, helix_test_support::agent::AGENT1_LATER));

    let notes_uri = gio::File::for_path(&notes).uri().to_string();
    // Every method, refused while access is off (arguments not looked at).
    let every = |conn: Connection, o: Opts| {
        let uri = notes_uri.clone();
        async move {
            vec![
                ("Open", call(&conn, "Open", &(uri.as_str(), o.clone())).await),
                ("Launch", call(&conn, "Launch", &("org.gnome.TextEditor", Vec::<&str>::new(), o.clone())).await),
                ("WindowAction", call(&conn, "WindowAction", &(11u32, "focus", o.clone())).await),
                ("SwitchWorkspace", call(&conn, "SwitchWorkspace", &(1u32, o.clone())).await),
                ("SetToggle", call(&conn, "SetToggle", &("dark-style", true, o.clone())).await),
                ("SetValue", call(&conn, "SetValue", &("volume", 0.5, o.clone())).await),
                ("Notify", call(&conn, "Notify", &("Hi", "", o.clone())).await),
            ]
        }
    };
    for (method, result) in every(conn.clone(), quiet()).await {
        assert_eq!(short(&result), "AccessOff", "{method}");
    }

    // On, but only Read only allowed: the level is too low.
    settings.set_boolean("enabled", true).unwrap();
    settings.set_boolean("allow-remote", true).unwrap();
    for (method, result) in every(conn.clone(), quiet()).await {
        assert_eq!(short(&result), "LevelTooLow", "{method}");
        assert_eq!(message(&result), "desktop", "{method} names the level it needs");
    }
    // A Read only grant covers no desktop method: without asking, refused.
    call(&conn, "RequestAccess", &("read-only", opts(&[]))).await.unwrap();
    settings.set_string("access-level", "desktop").unwrap();
    for (method, result) in every(conn.clone(), quiet()).await {
        assert_eq!(short(&result), "NotGranted", "{method}");
    }
    // Asked, the user allows Control the desktop for the session.
    let consents = asked_consent.load(Ordering::SeqCst);
    call(&conn, "SwitchWorkspace", &(0u32, opts(&[]))).await.unwrap();
    assert_eq!(asked_consent.load(Ordering::SeqCst), consents + 1);

    // Locked: nothing.
    lock.lock().unwrap().locked_since = Some(Instant::now());
    for (method, result) in every(conn.clone(), quiet()).await {
        assert_eq!(short(&result), "Locked", "{method}");
    }
    lock.lock().unwrap().locked_since = None;
    settings.set_strv("excluded-apps", ["org.example.Vault"]).unwrap();

    // Open: a file in its default app, an address, with another app.
    call(&conn, "Open", &(notes.to_string_lossy().as_ref(), opts(&[]))).await.unwrap();
    assert!(launched(&launches, &["text-editor ", "/home/notes.txt"]).await, "{:?}", std::fs::read_to_string(&launches));
    call(&conn, "Open", &("https://user:pw@example.org/reset?token=1", opts(&[]))).await.unwrap();
    assert!(launched(&launches, &["text-editor https://user:pw@example.org/reset?token=1"]).await);
    // An executable launcher nobody trusted opens as a file, not run.
    call(&conn, "Open", &(tool.to_string_lossy().as_ref(), opts(&[]))).await.unwrap();
    assert!(launched(&launches, &["text-editor ", "Desktop/tool.desktop"]).await);
    assert!(!std::fs::read_to_string(&launches).unwrap().contains("RAN"), "the untrusted launcher did not run");
    // Refused: other schemes, missing files, relative paths, and excluded
    // apps and Settings as the app that would open it.
    let open = |uri: &'static str, o: Opts| {
        let conn = conn.clone();
        async move { call(&conn, "Open", &(uri, o)).await }
    };
    assert_eq!(short(&open("ssh://example.org", opts(&[])).await), "NotSupported");
    assert_eq!(short(&open("notes.txt", opts(&[])).await), "InvalidArgs");
    assert_eq!(short(&open("/nonexistent/helix-test", opts(&[])).await), "NotFound");
    let pdf = home.join("report.pdf").to_string_lossy().into_owned();
    if sys.join("mime").exists() {
        let r = call(&conn, "Open", &(pdf.as_str(), opts(&[]))).await;
        assert_eq!((short(&r), message(&r)), ("Excluded".to_owned(), "app:org.example.Vault".to_owned()), "the default app for PDFs is excluded");
    }
    let r = open("mailto:a@example.org", opts(&[])).await;
    assert_eq!((short(&r), message(&r)), ("Excluded".to_owned(), "settings".to_owned()), "its handler runs Settings");
    let r = call(&conn, "Open", &(notes_uri.as_str(), opts(&[("app", Value::from("org.example.Vault"))]))).await;
    assert_eq!(message(&r), "app:org.example.Vault");

    // Launch: an app the grid shows, with files; not others.
    call(&conn, "Launch", &("org.gnome.TextEditor.desktop", vec![notes.to_string_lossy().into_owned(), "https://example.org/".into()], opts(&[]))).await.unwrap();
    assert!(launched(&launches, &["text-editor ", "/home/notes.txt https://example.org/"]).await);
    assert_eq!(short(&call(&conn, "Launch", &("org.example.Nothing", Vec::<&str>::new(), opts(&[]))).await), "NotFound");
    assert_eq!(short(&call(&conn, "Launch", &("org.example.Hidden", Vec::<&str>::new(), opts(&[]))).await), "NotFound", "not in the app grid");
    assert_eq!(short(&call(&conn, "Launch", &("", Vec::<&str>::new(), opts(&[]))).await), "InvalidArgs");
    assert_eq!(short(&call(&conn, "Launch", &("org.gnome.TextEditor", vec!["smb://host/share"], opts(&[]))).await), "NotSupported");
    for (app, token) in [("org.example.Vault", "app:org.example.Vault"), ("org.helixdesktop.Settings", "settings"), ("org.example.SettingsAlias", "settings")] {
        let r = call(&conn, "Launch", &(app, Vec::<&str>::new(), opts(&[]))).await;
        assert_eq!((short(&r), message(&r)), ("Excluded".to_owned(), token.to_owned()), "{app}");
    }
    let text = std::fs::read_to_string(&launches).unwrap();
    assert!(!text.contains("vault") && !text.contains("helix-settings-app") && !text.contains("settings-alias"), "{text}");

    // Windows: focus (switching workspaces), tile, move, resize, to a
    // workspace, restore; already done is nothing.
    let act = |window: u32, action: &'static str, o: Opts| {
        let conn = conn.clone();
        async move { call(&conn, "WindowAction", &(window, action, o)).await }
    };
    act(11, "tile-left", opts(&[])).await.unwrap();
    act(11, "tile-left", opts(&[])).await.unwrap();
    act(11, "maximize", opts(&[])).await.unwrap();
    act(11, "move", opts(&[("x", Value::from(40)), ("y", Value::from(60))])).await.unwrap();
    act(11, "resize", opts(&[("width", Value::from(500)), ("height", Value::from(300))])).await.unwrap();
    act(11, "to-workspace", opts(&[("workspace", Value::from(1u32))])).await.unwrap();
    act(14, "focus", opts(&[])).await.unwrap();
    let now = snapshot.lock().unwrap().clone();
    assert_eq!((now.focused, now.active), (14, 1));
    // Moving needs the window on the active workspace.
    assert_eq!(short(&act(15, "move", opts(&[("x", Value::from(0)), ("y", Value::from(0))])).await), "Failed");
    act(15, "minimize", opts(&[])).await.unwrap();
    act(15, "restore", opts(&[])).await.unwrap();
    act(15, "fullscreen", opts(&[])).await.unwrap();
    act(15, "unfullscreen", opts(&[])).await.unwrap();
    assert_eq!(
        *ops.lock().unwrap(),
        [
            WindowOp::Action(11, WindowAction::ToggleTiledLeft),
            WindowOp::Action(11, WindowAction::Maximize),
            WindowOp::MoveResize { id: 11, x: 40, y: 60, width: 640, height: 480 },
            WindowOp::MoveResize { id: 11, x: 40, y: 60, width: 500, height: 300 },
            WindowOp::ToWorkspace { id: 11, workspace: 1 },
            WindowOp::Activate(14),
            WindowOp::Action(15, WindowAction::Minimize),
            WindowOp::Activate(15),
            WindowOp::Action(15, WindowAction::ToggleFullscreen),
            WindowOp::Action(15, WindowAction::ToggleFullscreen),
        ]
    );
    let w11 = snapshot.lock().unwrap().windows.iter().find(|w| w.id == 11).cloned().unwrap();
    assert_eq!((w11.geometry, w11.workspace), ((40, 60, 500, 300), 1));
    // Refusals and bad arguments.
    assert_eq!(short(&act(999, "focus", opts(&[])).await), "NotFound");
    assert_eq!(short(&act(11, "shake", opts(&[])).await), "NotSupported");
    assert_eq!(short(&act(11, "move", opts(&[("x", Value::from(1))])).await), "InvalidArgs");
    assert_eq!(short(&act(11, "to-workspace", opts(&[("workspace", Value::from(5u32))])).await), "NotFound");
    let r = act(12, "focus", opts(&[])).await;
    assert_eq!((short(&r), message(&r)), ("Excluded".to_owned(), "app:org.example.Vault".to_owned()));
    let r = act(13, "minimize", opts(&[])).await;
    assert_eq!((short(&r), message(&r)), ("Excluded".to_owned(), "settings".to_owned()));
    // Workspaces.
    assert_eq!(snapshot.lock().unwrap().active, 0, "restoring window 15 went to its workspace");
    call(&conn, "SwitchWorkspace", &(1u32, opts(&[]))).await.unwrap();
    assert_eq!(snapshot.lock().unwrap().active, 1);
    assert_eq!(short(&call(&conn, "SwitchWorkspace", &(7u32, opts(&[]))).await), "NotFound");
    assert_eq!(ops.lock().unwrap().last(), Some(&WindowOp::SwitchWorkspace(1)));

    // Closing is sensitive: the excluded Settings window is refused before
    // anyone is asked; without asking it needs confirmation; unanswered,
    // denied, then allowed.
    let asked = confirm.asked();
    let r = act(13, "close", opts(&[])).await;
    assert_eq!(short(&r), "Excluded");
    assert_eq!(confirm.asked(), asked, "nobody was asked about an excluded window");
    assert_eq!(short(&act(15, "close", quiet()).await), "NeedsConfirmation");
    assert_eq!(short(&act(15, "close", opts(&[])).await), "NeedsConfirmation", "the prompt went unanswered");
    confirm.queue(Some(false));
    assert_eq!(short(&act(15, "close", opts(&[])).await), "DeniedByUser");
    assert!(snapshot.lock().unwrap().windows.iter().any(|w| w.id == 15), "not closed");
    confirm.queue(Some(true));
    act(15, "close", opts(&[])).await.unwrap();
    assert_eq!(confirm.last(), ("WindowAction".to_owned(), Some("close".to_owned())));
    assert!(!snapshot.lock().unwrap().windows.iter().any(|w| w.id == 15), "closed");

    // Quick settings: Wi-Fi is sensitive, and the prompt says what it does.
    let toggle = |name: &'static str, on: bool, o: Opts| {
        let conn = conn.clone();
        async move { call(&conn, "SetToggle", &(name, on, o)).await }
    };
    assert_eq!(short(&toggle("wifi", false, quiet()).await), "NeedsConfirmation");
    assert_eq!(short(&toggle("wifi", false, opts(&[])).await), "NeedsConfirmation");
    confirm.queue(Some(false));
    assert_eq!(short(&toggle("wifi", false, opts(&[])).await), "DeniedByUser");
    assert!(*wifi.lock().unwrap());
    confirm.queue(Some(true));
    toggle("wifi", false, opts(&[])).await.unwrap();
    assert_eq!(confirm.last(), ("SetToggle".to_owned(), Some("wifi-off".to_owned())));
    assert!(!*wifi.lock().unwrap(), "Wi-Fi turned off");
    confirm.queue(Some(true));
    toggle("airplane-mode", false, opts(&[])).await.unwrap();
    assert!(*wifi.lock().unwrap(), "airplane mode off turns the radios on");
    // Not sensitive, and without the switch on, no prompt.
    let asked = confirm.asked();
    toggle("mute", true, opts(&[])).await.unwrap();
    assert_eq!(std::fs::read_to_string(&volume).unwrap().trim(), "40 yes");
    settings.set_boolean("confirm-sensitive", false).unwrap();
    toggle("wifi", false, quiet()).await.unwrap();
    assert!(!*wifi.lock().unwrap());
    settings.set_boolean("confirm-sensitive", true).unwrap();
    assert_eq!(confirm.asked(), asked);
    assert_eq!(short(&toggle("bluetooth", true, opts(&[])).await), "NotSupported");
    if quick_schemas {
        toggle("dark-style", true, opts(&[])).await.unwrap();
        toggle("night-light", true, opts(&[])).await.unwrap();
        toggle("do-not-disturb", true, opts(&[])).await.unwrap();
        assert_eq!(gio::Settings::new("org.gnome.desktop.interface").string("color-scheme"), "prefer-dark");
        assert!(gio::Settings::new("org.gnome.settings-daemon.plugins.color").boolean("night-light-enabled"));
        assert!(!gio::Settings::new("org.gnome.desktop.notifications").boolean("show-banners"));
        let state: HashMap<String, OwnedValue> = call(&conn, "GetState", &(opts(&[]),)).await.unwrap().body().deserialize().unwrap();
        for key in ["dark-style", "night-light", "do-not-disturb", "muted"] {
            assert!(bool::try_from(state[key].clone()).unwrap(), "{key}");
        }
        toggle("dark-style", false, opts(&[])).await.unwrap();
        assert_eq!(gio::Settings::new("org.gnome.desktop.interface").string("color-scheme"), "default");
    } else {
        assert_eq!(short(&toggle("dark-style", true, opts(&[])).await), "NotSupported");
    }
    // Values: 0–1, volume up to 1 without over-amplification.
    call(&conn, "SetValue", &("volume", 0.55, opts(&[]))).await.unwrap();
    assert_eq!(std::fs::read_to_string(&volume).unwrap().trim(), "55 no", "setting the volume unmutes");
    assert_eq!(short(&call(&conn, "SetValue", &("volume", 1.2, opts(&[]))).await), "InvalidArgs");
    call(&conn, "SetValue", &("brightness", 0.25, opts(&[]))).await.unwrap();
    assert_eq!(*backlight.lock().unwrap(), 50);
    assert_eq!(short(&call(&conn, "SetValue", &("contrast", 0.5, opts(&[]))).await), "NotSupported");

    // Notifications: labelled with the agent, plain, at most 5 a minute.
    let reply = call(&conn, "Notify", &("Build finished", "All <b>green</b>", opts(&[("urgency", Value::from("low")), ("icon", Value::from("emblem-ok-symbolic"))]))).await.unwrap();
    let id: u32 = reply.body().deserialize().unwrap();
    let listed = center.listed();
    let (n, _) = listed.iter().find(|(n, _)| n.id == id).expect("listed");
    // Its display name comes from its environment at start; without one,
    // the executable's name.
    assert!(n.app_name.starts_with("Agent: agent_desktop_bus-"), "{}", n.app_name);
    assert_eq!((n.summary.as_str(), n.body.as_str(), n.markup, n.app_id.as_str()), ("Build finished", "All <b>green</b>", false, ""));
    assert_eq!(short(&call(&conn, "Notify", &("", "", opts(&[]))).await), "InvalidArgs");
    assert_eq!(short(&call(&conn, "Notify", &("x", "", opts(&[("urgency", Value::from("critical"))]))).await), "InvalidArgs");
    assert_eq!(short(&call(&conn, "Notify", &("x", "", opts(&[("icon", Value::from("/etc/passwd"))]))).await), "InvalidArgs");
    for _ in 0..4 {
        call(&conn, "Notify", &("Again", "", opts(&[]))).await.unwrap();
    }
    let r = call(&conn, "Notify", &("One too many", "", opts(&[]))).await;
    assert_eq!(short(&r), "Failed");
    assert!(message(&r).contains("at most 5"), "{}", message(&r));
    // Marked sensitive by the agent: asked first.
    let asked = confirm.asked();
    assert_eq!(short(&call(&conn, "SwitchWorkspace", &(1u32, opts(&[("sensitive", Value::from(true))]))).await), "NeedsConfirmation");
    assert_eq!(confirm.asked(), asked + 1);
    assert_eq!(confirm.last(), ("SwitchWorkspace".to_owned(), None));

    // Settings takes access away; that is recorded too.
    conn.call_method(Some("org.gnome.Shell"), OBJECT_PATH, Some("org.helixdesktop.AgentAccess1"), "Revoke", &("/no/such/agent",)).await.unwrap();

    // The activity log: targets, details, outcomes; never the text.
    let lines = activity(&log);
    let me = me.to_string_lossy().into_owned();
    let has = |method: &str, outcome: &str, extra: &dyn Fn(&serde_json::Value) -> bool| lines.iter().any(|l| l["method"] == method && l["outcome"] == outcome && extra(l));
    let any = |_: &serde_json::Value| true;
    for method in ["Open", "Launch", "WindowAction", "SwitchWorkspace", "SetToggle", "SetValue", "Notify"] {
        for refusal in ["AccessOff", "LevelTooLow", "NotGranted", "Locked"] {
            assert!(has(method, &format!("refused:{refusal}"), &|l| l["agent"] == me.as_str() && l["level"] == "desktop"), "{method} {refusal}: {lines:#?}");
        }
    }
    assert!(has("Open", "allowed", &|l| l["target"] == notes_uri.as_str() && l.get("error").is_none()));
    assert!(has("Open", "allowed", &|l| l["target"] == "https://example.org"), "only the scheme and host: {lines:#?}");
    assert!(has("Open", "refused:Excluded", &|l| l["target"] == "mailto:" && l["reason"] == "settings"));
    assert!(has("Open", "allowed", &|l| l["error"] == "NotSupported"));
    assert!(has("Launch", "allowed", &|l| l["target"] == "org.gnome.TextEditor.desktop" && l["uri-count"] == 2));
    assert!(has("Launch", "refused:Excluded", &|l| l["target"] == "org.example.SettingsAlias" && l["reason"] == "settings"));
    assert!(has("WindowAction", "allowed", &|l| l["target"] == "window:11 (org.gnome.TextEditor)" && l["action"] == "tile-left"));
    // (The refused close of Settings' window is counted with the refused
    // minimize: identical refusals within a minute are coalesced.)
    assert!(has("WindowAction", "refused:Excluded", &|l| l["target"] == "window:13 (org.helixdesktop.Settings)" && l["action"] == "minimize" && l["reason"] == "settings"));
    assert!(has("WindowAction", "refused:NeedsConfirmation", &|l| l["target"] == "window:15 (org.gnome.Calculator)"));
    assert!(has("WindowAction", "refused:DeniedByUser", &|l| l["action"] == "close"));
    assert!(has("WindowAction", "allowed", &|l| l["error"] == "NotFound"));
    assert!(has("SwitchWorkspace", "allowed", &|l| l["target"] == "workspace:0"));
    assert!(has("SwitchWorkspace", "allowed", &|l| l["target"] == "workspace:1" && l.get("error").is_none()));
    assert!(has("SwitchWorkspace", "allowed", &|l| l["target"] == "workspace:7" && l["error"] == "NotFound"));
    assert!(has("SensitivePrompt", "allowed", &|l| l["target"] == "WindowAction" && l["action"] == "close"));
    assert!(has("SensitivePrompt", "refused:DeniedByUser", &|l| l["target"] == "SetToggle" && l["action"] == "wifi-off"));
    assert!(has("SetToggle", "allowed", &|l| l["target"] == "wifi" && l["state"] == "off"));
    assert!(has("SetToggle", "refused:DeniedByUser", &|l| l["target"] == "wifi"));
    assert!(has("SetValue", "allowed", &|l| l["target"] == "volume" && l.get("error").is_none()));
    assert!(has("SetValue", "allowed", &|l| l["target"] == "volume" && l["error"] == "InvalidArgs"));
    assert!(has("Notify", "allowed", &|l| l["title-length"] == 14 && l["body-length"] == 16));
    assert!(has("Notify", "allowed", &|l| l["error"] == "Failed"), "the rate limit");
    assert!(has("ConsentPrompt", "allowed", &|l| l["level"] == "desktop"));
    assert!(has("Revoke", "allowed", &|l| l["target"] == "/no/such/agent" && l["agent"] == me.as_str()));
    assert!(has("SetToggle", "allowed", &any));
    let all = lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
    for secret in ["Build finished", "green", "token", "pw@", "reset", "0.55"] {
        assert!(!all.contains(secret), "{secret} is in the log");
    }
    let _ = std::fs::remove_dir_all(&tmp);
}
