//! Agent Access's *Read only* methods over a private bus (helixdesktop/spec
//! `contracts/agent-access/README.md`, "Read only methods";
//! helixdesktop/desktop#146): windows, apps, workspaces and monitors from
//! a stand-in UI, notifications through the shell's own notification
//! server, the quick settings' state, recent files from a scratch
//! `recently-used.xbel`, file search (by scanning, and through a private
//! TinySPARQL endpoint named as LocalSearch when `tinysparql` is
//! installed), the waits, and the refusals: access off, no grant, locked,
//! excluded apps and the notifications switch. Nothing on the host is
//! touched: scratch XDG directories, in-memory settings, no system bus, no
//! sound server.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gio::prelude::*;
use helix_shell::agent::desktop::{MonitorInfo, Snapshot, WindowInfo, ALL_WORKSPACES, MAXIMIZED};
use helix_shell::agent::grants::Answer;
use helix_shell::agent::{self, AgentAccess, Agent1, Desktop, Setup, OBJECT_PATH};
use helix_shell::notifications::{Center, FdoNotifications};
use helix_shell::screensaver::SharedStatus;
use helix_shell::UiEvent;
use helix_test_support::conformance::{assert_conformant, check, introspect, load_dbus_contract};
use helix_test_support::TestBus;
use zbus::zvariant::{OwnedValue, Value};
use zbus::Connection;

const IFACE: &str = "org.helixdesktop.Agent1";

type Record = HashMap<String, OwnedValue>;

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

fn opts(pairs: &[(&'static str, Value<'static>)]) -> HashMap<&'static str, Value<'static>> {
    pairs.iter().cloned().collect()
}

async fn records(conn: &Connection, method: &str, options: HashMap<&'static str, Value<'static>>) -> Vec<Record> {
    let reply = call(conn, method, &(options,)).await.unwrap_or_else(|e| panic!("{method}: {e:?}"));
    reply.body().deserialize().unwrap()
}

fn s(r: &Record, key: &str) -> String {
    String::try_from(r[key].clone()).unwrap_or_else(|_| panic!("{key}"))
}

fn u(r: &Record, key: &str) -> u32 {
    u32::try_from(r[key].clone()).unwrap_or_else(|_| panic!("{key}"))
}

fn b(r: &Record, key: &str) -> bool {
    bool::try_from(r[key].clone()).unwrap_or_else(|_| panic!("{key}"))
}

fn win(id: u32, app: &str, title: &str, workspace: u32) -> WindowInfo {
    WindowInfo { id, app_id: app.into(), title: title.into(), workspace, geometry: (100, 100, 640, 480), ..Default::default() }
}

fn installed(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(program).is_file()))
}

fn desktop_file(dir: &Path, id: &str, name: &str, exec: &str) {
    std::fs::write(dir.join(format!("{id}.desktop")), format!("[Desktop Entry]\nType=Application\nName={name}\nExec={exec} %U\n")).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn read_only_over_the_bus() {
    let Some(bus) = TestBus::start() else { return };
    let tmp = std::env::temp_dir().join(format!("helix-agent-read-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let schemas = tmp.join("schemas");
    std::fs::create_dir_all(&schemas).unwrap();
    let data = concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/org.helixdesktop.agent-access.gschema.xml");
    std::fs::copy(data, schemas.join("org.helixdesktop.agent-access.gschema.xml")).unwrap();
    let Ok(compiled) = std::process::Command::new("glib-compile-schemas").arg(&schemas).status() else {
        eprintln!("glib-compile-schemas is not installed; skipping");
        return;
    };
    assert!(compiled.success());
    let home = tmp.join("home");
    let data_home = tmp.join("data");
    let apps = data_home.join("applications");
    std::fs::create_dir_all(&apps).unwrap();
    std::fs::create_dir_all(home.join("Documents")).unwrap();
    std::fs::create_dir_all(tmp.join("run")).unwrap();
    // SAFETY: this test binary runs this one test, before any GLib use.
    unsafe {
        std::env::set_var("GSETTINGS_SCHEMA_DIR", &schemas);
        std::env::set_var("GSETTINGS_BACKEND", "memory");
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_CONFIG_HOME", tmp.join("config"));
        std::env::set_var("XDG_STATE_HOME", tmp.join("state"));
        std::env::set_var("XDG_CACHE_HOME", tmp.join("cache"));
        std::env::set_var("XDG_DATA_HOME", &data_home);
        // Only this test's apps.
        std::env::set_var("XDG_DATA_DIRS", tmp.join("sys"));
        std::env::set_var("XDG_RUNTIME_DIR", tmp.join("run"));
        // No system bus (NetworkManager), no sound server.
        std::env::set_var("DBUS_SYSTEM_BUS_ADDRESS", format!("unix:path={}", tmp.join("no-bus").display()));
        std::env::set_var("PULSE_SERVER", format!("unix:{}", tmp.join("no-pulse").display()));
        std::env::set_var("GIO_USE_VFS", "local");
    }
    // GIO lists apps whose program it finds.
    desktop_file(&apps, "org.gnome.TextEditor", "Text Editor", "true");
    desktop_file(&apps, "org.gnome.Calculator", "Calculator", "true");
    desktop_file(&apps, "org.example.Vault", "Secret Vault", "true");
    desktop_file(&apps, "org.helixdesktop.Settings", "Settings", "true");

    // The UI's view: a stand-in that answers snapshots.
    let snapshot = Arc::new(Mutex::new(Snapshot {
        windows: vec![
            win(11, "org.gnome.TextEditor", "notes.txt (~/Documents) - Text Editor", 0),
            win(12, "org.example.Vault", "My passwords", 0),
            win(13, "org.helixdesktop.Settings", "Agent Access", 1),
            WindowInfo { state: MAXIMIZED, ..win(14, "org.gnome.Calculator", "Calculator", ALL_WORKSPACES) },
        ],
        focused: 11,
        workspaces: 2,
        active: 0,
        monitors: vec![MonitorInfo { connector: "HEADLESS-1".into(), name: "Virtual".into(), area: (0, 0, 1280, 800), scale: 1.25 }],
        primary: (0, 0, 1280, 800),
    }));
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
    let center = Center::new(ui.clone(), lock.clone());
    let consent: agent::Consent = Arc::new(|_| Box::pin(async { Answer::Session }));
    let access = AgentAccess::new(lock.clone(), Setup { grants_path: None, consent, confirm: agent::no_confirm(), proc_root: "/proc".into(), settings_exes: Vec::new(), log_dir: Some(tmp.join("state/helix/agent-activity")) });
    access.set_desktop(Desktop { ui: ui.clone(), notifications: Some(center.clone()) });
    let server = bus.builder().serve_at(OBJECT_PATH, Agent1 { access: access.clone() }).unwrap().name("org.gnome.Shell").unwrap().build().await.unwrap();
    access.attach(&server);
    let _notifications = bus
        .builder()
        .serve_at("/org/freedesktop/Notifications", FdoNotifications { center: center.clone() })
        .unwrap()
        .name("org.freedesktop.Notifications")
        .unwrap()
        .build()
        .await
        .unwrap();
    let conn = bus.connect().await;

    // The read methods are there now, as the contract has them.
    let actual = introspect(&conn, "org.gnome.Shell", OBJECT_PATH).await;
    let later: Vec<String> = [
        "Ask", "Confirm", "AddReminder", "ListReminders", "CancelReminder",
        "GetClipboard", "SetClipboard", "A11yTree", "A11yAction", "A11ySetValue", "Screenshot", "Input",
    ]
    .iter()
    .map(|m| format!("method {m}"))
    .collect();
    let later: Vec<&str> = later.iter().map(String::as_str).collect();
    assert_conformant(IFACE, &check(IFACE, &load_dbus_contract("../agent-access/org.helixdesktop.Agent1.xml"), &actual, &later));

    // Off: refused, and arguments are not looked at.
    let none = || opts(&[]);
    for method in ["GetState", "ListWindows", "ListApps", "ListWorkspaces", "ListMonitors", "ListNotifications", "RecentFiles"] {
        assert_eq!(short(&call(&conn, method, &(none(),)).await), "AccessOff", "{method}");
    }
    assert_eq!(short(&call(&conn, "SearchFiles", &("", none())).await), "AccessOff");
    assert_eq!(short(&call(&conn, "WaitForWindow", &(none(), 10u32)).await), "AccessOff");
    assert_eq!(short(&call(&conn, "WaitForWindowClosed", &(99u32, 10u32)).await), "AccessOff");

    settings.set_boolean("enabled", true).unwrap();
    settings.set_boolean("allow-remote", true).unwrap();
    settings.set_strv("excluded-apps", ["org.example.Vault.desktop"]).unwrap();
    // No grant, and the user may not be asked.
    assert_eq!(short(&call(&conn, "ListWindows", &(opts(&[("interactive", Value::from(false))]),)).await), "NotGranted");
    // Asked (the stand-in consent allows for the session).
    let listed = records(&conn, "ListWindows", none()).await;

    // Windows: excluded apps' and Settings' left out.
    let ids: Vec<u32> = listed.iter().map(|w| u(w, "id")).collect();
    assert_eq!(ids, [11, 14]);
    let editor = &listed[0];
    assert_eq!(s(editor, "app-id"), "org.gnome.TextEditor");
    assert!(b(editor, "focused"));
    assert_eq!((s(editor, "monitor"), s(editor, "state")), ("HEADLESS-1".to_owned(), "normal".to_owned()));
    assert_eq!(i32::try_from(editor["width"].clone()).unwrap(), 640);
    assert!(!editor.contains_key("document"), "no recent file tells yet");
    let calc = &listed[1];
    assert_eq!((s(calc, "state"), u(calc, "workspace"), b(calc, "all-workspaces")), ("maximized".to_owned(), 0, true));
    let only = records(&conn, "ListWindows", opts(&[("app", Value::from("org.gnome.Calculator.desktop"))])).await;
    assert_eq!(only.len(), 1);
    assert_eq!(records(&conn, "ListWindows", opts(&[("focused", Value::from(true))])).await.len(), 1);
    let excluded = call(&conn, "ListWindows", &(opts(&[("app", Value::from("org.example.Vault"))]),)).await;
    assert_eq!((short(&excluded), message(&excluded)), ("Excluded".to_owned(), "app:org.example.Vault".to_owned()));
    let settings_app = call(&conn, "ListWindows", &(opts(&[("app", Value::from("org.helixdesktop.Settings"))]),)).await;
    assert_eq!((short(&settings_app), message(&settings_app)), ("Excluded".to_owned(), "settings".to_owned()));
    assert_eq!(short(&call(&conn, "ListWindows", &(opts(&[("focused", Value::from("yes"))]),)).await), "org.freedesktop.DBus.Error.InvalidArgs");

    // What is open in the editor: the recent file whose name is in the
    // title, registered by the editor.
    let notes = home.join("Documents/notes.txt");
    std::fs::write(&notes, "quarterly figures").unwrap();
    let vault = home.join("Documents/passwords.kdbx");
    std::fs::write(&vault, "x").unwrap();
    let uri = |p: &Path| gio::File::for_path(p).uri().to_string();
    let bookmark = |p: &Path, mime: &str, app: &str, exec: &str, when: &str| {
        format!(
            "<bookmark href=\"{}\" added=\"{when}\" modified=\"{when}\" visited=\"{when}\"><info><metadata owner=\"http://freedesktop.org\"><mime:mime-type type=\"{mime}\"/><bookmark:applications><bookmark:application name=\"{app}\" exec=\"&apos;{exec} %u&apos;\" modified=\"{when}\" count=\"1\"/></bookmark:applications></metadata></info></bookmark>\n",
            uri(p)
        )
    };
    std::fs::write(
        data_home.join("recently-used.xbel"),
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<xbel version=\"1.0\" xmlns:bookmark=\"http://www.freedesktop.org/standards/desktop-bookmarks\" xmlns:mime=\"http://www.freedesktop.org/standards/shared-mime-info\">\n{}{}</xbel>\n",
            bookmark(&notes, "text/plain", "Text Editor", "gnome-text-editor", "2026-11-19T09:00:00Z"),
            bookmark(&vault, "application/x-keepass2", "Secret Vault", "vault", "2026-11-19T10:00:00Z"),
        ),
    )
    .unwrap();
    let listed = records(&conn, "ListWindows", none()).await;
    assert_eq!(s(&listed[0], "document"), uri(&notes));

    // Apps: by name, the excluded one and Settings left out.
    let all = records(&conn, "ListApps", none()).await;
    let names: Vec<(String, bool, u32)> = all.iter().map(|a| (s(a, "id"), b(a, "running"), u(a, "windows"))).collect();
    assert_eq!(names, [("org.gnome.Calculator".to_owned(), true, 1), ("org.gnome.TextEditor".to_owned(), true, 1)]);
    snapshot.lock().unwrap().windows.retain(|w| w.id != 14);
    let running = records(&conn, "ListApps", opts(&[("running", Value::from(true))])).await;
    assert_eq!(running.iter().map(|a| s(a, "id")).collect::<Vec<_>>(), ["org.gnome.TextEditor"]);

    // Workspaces count only windows agents may see; monitors.
    let spaces = records(&conn, "ListWorkspaces", none()).await;
    assert_eq!(spaces.iter().map(|w| (u(w, "index"), b(w, "active"), u(w, "windows"))).collect::<Vec<_>>(), [(0, true, 1), (1, false, 0)]);
    let monitors = records(&conn, "ListMonitors", none()).await;
    assert_eq!((s(&monitors[0], "connector"), b(&monitors[0], "primary")), ("HEADLESS-1".to_owned(), true));
    assert_eq!(f64::try_from(monitors[0]["scale"].clone()).unwrap(), 1.25);

    // The state: the focused window, and settings; no NetworkManager, sound
    // server or backlight here, so those keys are absent.
    let state = || async { call(&conn, "GetState", &(none(),)).await.unwrap().body().deserialize::<Record>().unwrap() };
    let st = state().await;
    assert_eq!((u(&st, "focused-window"), u(&st, "active-workspace")), (11, 0));
    for key in ["dark-style", "do-not-disturb", "night-light"] {
        assert!(st.contains_key(key), "{key}");
    }
    for key in ["wifi", "volume", "muted", "brightness"] {
        assert!(!st.contains_key(key), "{key}");
    }
    if gio::SettingsSchemaSource::default().and_then(|src| src.lookup("org.gnome.desktop.interface", true)).is_some_and(|s| s.has_key("color-scheme")) {
        gio::Settings::new("org.gnome.desktop.interface").set_string("color-scheme", "prefer-dark").unwrap();
        assert!(b(&state().await, "dark-style"));
    }
    // An excluded app's window has the focus: none, as far as agents know.
    snapshot.lock().unwrap().focused = 12;
    assert_eq!(u(&state().await, "focused-window"), 0);
    snapshot.lock().unwrap().focused = 11;

    // Notifications: only app and time without the switch.
    let notify = |entry: &'static str, app: &'static str, summary: &'static str| {
        let conn = conn.clone();
        async move {
            let hints: HashMap<&str, Value<'_>> = HashMap::from([("desktop-entry", Value::from(entry))]);
            conn.call_method(Some("org.freedesktop.Notifications"), "/org/freedesktop/Notifications", Some("org.freedesktop.Notifications"), "Notify", &(app, 0u32, "", summary, "The <b>body</b>", Vec::<&str>::new(), hints, -1i32))
                .await
                .unwrap();
        }
    };
    notify("org.gnome.Calculator", "Calculator", "Result ready").await;
    notify("org.example.Vault", "Secret Vault", "Password copied").await;
    let notes_list = records(&conn, "ListNotifications", none()).await;
    assert_eq!(notes_list.len(), 1, "the excluded app's left out");
    assert_eq!((s(&notes_list[0], "app-id"), s(&notes_list[0], "app-name")), ("org.gnome.Calculator".to_owned(), "Calculator".to_owned()));
    assert!(b(&notes_list[0], "redacted"));
    assert!(!notes_list[0].contains_key("summary") && !notes_list[0].contains_key("body"));
    assert!(i64::try_from(notes_list[0]["time"].clone()).unwrap() > 1_700_000_000);
    settings.set_boolean("notifications", true).unwrap();
    let notes_list = records(&conn, "ListNotifications", opts(&[("limit", Value::from(5u32))])).await;
    assert_eq!(s(&notes_list[0], "summary"), "Result ready");
    assert!(s(&notes_list[0], "body").contains("body"));
    assert!(!notes_list[0].contains_key("redacted"));

    // Recent files: the excluded app's left out; by type.
    let recent = records(&conn, "RecentFiles", none()).await;
    assert_eq!(recent.iter().map(|f| s(f, "uri")).collect::<Vec<_>>(), [uri(&notes)]);
    assert_eq!((s(&recent[0], "name"), s(&recent[0], "mime-type")), ("notes.txt".to_owned(), "text/plain".to_owned()));
    assert_eq!(u64::try_from(recent[0]["size"].clone()).unwrap(), 17);
    assert!(recent[0].contains_key("used") && recent[0].contains_key("modified"));
    assert!(records(&conn, "RecentFiles", opts(&[("type", Value::from("image/*"))])).await.is_empty());
    assert_eq!(short(&call(&conn, "RecentFiles", &(opts(&[("limit", Value::from("ten"))]),)).await), "org.freedesktop.DBus.Error.InvalidArgs");

    // File search without an index: names below the folder.
    std::fs::write(home.join("Documents/budget-2026.ods"), "x").unwrap();
    std::fs::create_dir_all(home.join(".hidden")).unwrap();
    std::fs::write(home.join(".hidden/budget.txt"), "x").unwrap();
    let search = |query: &'static str, options: HashMap<&'static str, Value<'static>>| {
        let conn = conn.clone();
        async move { call(&conn, "SearchFiles", &(query, options)).await }
    };
    let found: Vec<Record> = search("budget", none()).await.unwrap().body().deserialize().unwrap();
    assert_eq!(found.iter().map(|f| s(f, "name")).collect::<Vec<_>>(), ["budget-2026.ods"], "hidden folders left out");
    let found: Vec<Record> = search("budget", opts(&[("since", Value::from(i64::MAX / 2))])).await.unwrap().body().deserialize().unwrap();
    assert!(found.is_empty(), "changed too long ago");
    let found: Vec<Record> = search("budget", opts(&[("in", Value::from(home.join("Documents").to_string_lossy().into_owned())), ("type", Value::from("text/plain"))])).await.unwrap().body().deserialize().unwrap();
    assert!(found.is_empty(), "not that type");
    assert_eq!(short(&search(" ", none()).await), "org.freedesktop.DBus.Error.InvalidArgs");
    assert_eq!(short(&search("x", opts(&[("in", Value::from("/no/such/folder"))])).await), "NotFound");

    // Through LocalSearch: a private TinySPARQL endpoint with its name.
    if installed("tinysparql") {
        let db = tmp.join("index");
        let mut endpoint = std::process::Command::new("tinysparql")
            .args(["endpoint", "--session", "--ontology", "nepomuk", "-b", "org.freedesktop.LocalSearch3", "-d"])
            .arg(&db)
            .env("DBUS_SESSION_BUS_ADDRESS", bus.address())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let dbus = zbus::fdo::DBusProxy::new(&conn).await.unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !dbus.name_has_owner("org.freedesktop.LocalSearch3".try_into().unwrap()).await.unwrap_or(false) {
            assert!(Instant::now() < deadline, "the endpoint did not start");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // The index knows the note's contents (the scan would not find
        // "quarterly" in a name).
        let insert = format!(
            "INSERT DATA {{ <{u}> a nfo:FileDataObject ; nie:url \"{u}\" ; nfo:fileName \"notes.txt\" ; nie:interpretedAs <urn:helix:n> . <urn:helix:n> a nfo:PlainTextDocument ; nie:isStoredAs <{u}> ; nie:plainTextContent \"quarterly figures\" . }}",
            u = uri(&notes)
        );
        let status = std::process::Command::new("tinysparql")
            .args(["query", "-b", "org.freedesktop.LocalSearch3", "-u", "-q", &insert])
            .env("DBUS_SESSION_BUS_ADDRESS", bus.address())
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        let found: Vec<Record> = search("quarter", none()).await.unwrap().body().deserialize().unwrap();
        assert_eq!(found.iter().map(|f| s(f, "uri")).collect::<Vec<_>>(), [uri(&notes)], "found by its contents through the index");
        let _ = endpoint.kill();
        let _ = endpoint.wait();
    } else {
        eprintln!("tinysparql is not installed; searching through LocalSearch not tested");
    }

    // Waiting for a window: at once when there is one.
    let w: Record = call(&conn, "WaitForWindow", &(opts(&[("app", Value::from("org.gnome.TextEditor"))]), 1000u32)).await.unwrap().body().deserialize().unwrap();
    assert_eq!(u(&w, "id"), 11);
    // ...or when it appears.
    let waiting = {
        let conn = conn.clone();
        tokio::spawn(async move { call(&conn, "WaitForWindow", &(opts(&[("title", Value::from("REPORT"))]), 10_000u32)).await })
    };
    tokio::time::sleep(Duration::from_millis(400)).await;
    snapshot.lock().unwrap().windows.push(win(20, "org.gnome.Papers", "report.pdf", 0));
    let w: Record = waiting.await.unwrap().unwrap().body().deserialize().unwrap();
    assert_eq!(u(&w, "id"), 20);
    // An excluded app's window never matches.
    snapshot.lock().unwrap().windows.push(win(21, "org.example.Vault", "Vault report", 0));
    let timed = call(&conn, "WaitForWindow", &(opts(&[("title", Value::from("vault"))]), 300u32)).await;
    assert_eq!(short(&timed), "TimedOut");
    assert_eq!(short(&call(&conn, "WaitForWindow", &(opts(&[("app", Value::from("org.example.Vault"))]), 300u32)).await), "Excluded");

    // Waiting for one to close.
    assert_eq!(short(&call(&conn, "WaitForWindowClosed", &(999u32, 100u32)).await), "NotFound");
    assert_eq!(short(&call(&conn, "WaitForWindowClosed", &(21u32, 100u32)).await), "Excluded");
    assert_eq!(short(&call(&conn, "WaitForWindowClosed", &(20u32, 300u32)).await), "TimedOut");
    let closing = {
        let conn = conn.clone();
        tokio::spawn(async move { call(&conn, "WaitForWindowClosed", &(20u32, 10_000u32)).await })
    };
    tokio::time::sleep(Duration::from_millis(400)).await;
    snapshot.lock().unwrap().windows.retain(|w| w.id != 20);
    assert_eq!(short(&closing.await.unwrap()), "ok");

    // A wait ends when access does.
    let waiting = {
        let conn = conn.clone();
        tokio::spawn(async move { call(&conn, "WaitForWindow", &(opts(&[("app", Value::from("org.gnome.Maps"))]), 0u32)).await })
    };
    tokio::time::sleep(Duration::from_millis(400)).await;
    lock.lock().unwrap().locked_since = Some(Instant::now());
    let ended = tokio::time::timeout(Duration::from_secs(5), waiting).await.expect("the wait ended").unwrap();
    assert_eq!(short(&ended), "Locked");
    // Locked: nothing at all.
    assert_eq!(short(&call(&conn, "ListWindows", &(none(),)).await), "Locked");
    assert_eq!(short(&call(&conn, "ListNotifications", &(none(),)).await), "Locked");
    lock.lock().unwrap().locked_since = None;
    records(&conn, "ListWindows", none()).await;
    // Turning access off ends a wait too.
    let waiting = {
        let conn = conn.clone();
        tokio::spawn(async move { call(&conn, "WaitForWindow", &(opts(&[("app", Value::from("org.gnome.Maps"))]), 0u32)).await })
    };
    tokio::time::sleep(Duration::from_millis(400)).await;
    settings.set_boolean("enabled", false).unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(5), waiting).await.expect("the wait ended").unwrap();
    assert_eq!(short(&ended), "AccessOff");

    // The activity log has every call with its target, and none of what
    // the agent sent or read: no query, title, notification or file name.
    let log_dir = tmp.join("state/helix/agent-activity");
    let mut days: Vec<_> = std::fs::read_dir(&log_dir).unwrap().flatten().map(|e| e.path()).collect();
    days.sort();
    let text: String = days.iter().map(|d| std::fs::read_to_string(d).unwrap()).collect();
    for secret in ["budget", "quarter", "REPORT", "vault\"", "Result ready", "Password copied", "notes.txt", "passwords"] {
        assert!(!text.contains(secret), "{secret} is in the log:\n{text}");
    }
    let lines: Vec<serde_json::Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let has = |method: &str, outcome: &str, extra: &dyn Fn(&serde_json::Value) -> bool| lines.iter().any(|l| l["method"] == method && l["outcome"] == outcome && extra(l));
    let any = |_: &serde_json::Value| true;
    for method in ["GetState", "ListWindows", "ListApps", "ListWorkspaces", "ListMonitors", "ListNotifications", "RecentFiles", "SearchFiles", "WaitForWindow", "WaitForWindowClosed"] {
        assert!(has(method, "refused:AccessOff", &any), "{method}: {text}");
        assert!(has(method, "allowed", &|l| l["level"] == "read-only"), "{method}: {text}");
    }
    assert!(has("ListWindows", "refused:NotGranted", &any));
    assert!(has("ConsentPrompt", "allowed", &|l| l["grant"] == "session"));
    assert!(has("ListWindows", "allowed", &|l| l["target"] == "org.gnome.Calculator.desktop"));
    assert!(has("ListWindows", "refused:Excluded", &|l| l["target"] == "org.example.Vault"));
    assert!(has("ListWindows", "refused:Excluded", &|l| l["target"] == "org.helixdesktop.Settings"));
    assert!(has("ListWindows", "allowed", &|l| l["error"] == "InvalidArgs"));
    assert!(has("RecentFiles", "allowed", &|l| l["error"] == "InvalidArgs"));
    let home_uri = gio::File::for_path(&home).uri().to_string();
    assert!(has("SearchFiles", "allowed", &|l| l["query-length"] == 6 && l["target"] == home_uri.as_str() && l.get("error").is_none()));
    assert!(has("SearchFiles", "allowed", &|l| l["query-length"] == 1 && l["error"] == "InvalidArgs"));
    assert!(has("SearchFiles", "allowed", &|l| l["error"] == "NotFound"));
    assert!(has("WaitForWindow", "allowed", &|l| l["title-length"] == 6 && l.get("error").is_none()));
    assert!(has("WaitForWindow", "allowed", &|l| l["title-length"] == 5 && l["error"] == "TimedOut"));
    assert!(has("WaitForWindow", "refused:Excluded", &|l| l["target"] == "org.example.Vault"));
    assert!(has("WaitForWindow", "refused:Locked", &|l| l["target"] == "org.gnome.Maps"), "the end of a wait: {text}");
    assert!(has("WaitForWindowClosed", "allowed", &|l| l["target"] == "window:999" && l["error"] == "NotFound"));
    assert!(has("WaitForWindowClosed", "refused:Excluded", &|l| l["target"] == "window:21 (org.example.Vault)"));
    assert!(has("WaitForWindowClosed", "allowed", &|l| l["target"] == "window:20 (org.gnome.Papers)" && l.get("error").is_none()));

    let _ = std::fs::remove_dir_all(&tmp);
}
