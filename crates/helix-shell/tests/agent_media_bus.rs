//! Agent Access's scoped actions, batch 1, over a private bus
//! (helixdesktop/spec `contracts/agent-access/README.md`, "Media, power and
//! stars"; helixdesktop/desktop#156): `GetMedia` and `MediaControl` with
//! fake MPRIS players (each a process of its own), `GetPower` with fake
//! UPower and power-profiles-daemon on a second private bus standing in for
//! the system bus, and `StarredFiles` from a scratch star store. Excluded
//! apps' players (by `DesktopEntry`, by `Identity`, by their process's
//! other windows) and Settings' are never listed, never the active one and
//! never controlled. Nothing on the host is touched.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use gio::prelude::*;
use helix_shell::agent::desktop::{Snapshot, WindowInfo};
use helix_shell::agent::grants::Answer;
use helix_shell::agent::{self, Agent1, AgentAccess, Desktop, Setup, OBJECT_PATH};
use helix_shell::screensaver::SharedStatus;
use helix_shell::UiEvent;
use helix_test_support::conformance::{assert_conformant, check, introspect, load_dbus_contract};
use helix_test_support::mpris::{python_gio, FakePlayer};
use helix_test_support::upower::{self, Battery};
use helix_test_support::TestBus;
use serde_json::Value as Json;
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

type Opts = HashMap<&'static str, Value<'static>>;

fn opts(pairs: &[(&'static str, Value<'static>)]) -> Opts {
    pairs.iter().cloned().collect()
}

async fn records(conn: &Connection, method: &str, options: Opts) -> Vec<Record> {
    let reply = call(conn, method, &(options,)).await.unwrap_or_else(|e| panic!("{method}: {e:?}"));
    reply.body().deserialize().unwrap()
}

async fn control(conn: &Connection, player: &str, action: &str, options: Opts) -> zbus::Result<zbus::Message> {
    call(conn, "MediaControl", &(player, action, options)).await
}

fn s(r: &Record, key: &str) -> String {
    String::try_from(r[key].clone()).unwrap_or_else(|_| panic!("{key}"))
}

fn b(r: &Record, key: &str) -> bool {
    bool::try_from(r[key].clone()).unwrap_or_else(|_| panic!("{key}"))
}

fn d(r: &Record, key: &str) -> f64 {
    f64::try_from(r[key].clone()).unwrap_or_else(|_| panic!("{key}"))
}

fn desktop_file(dir: &Path, id: &str, name: &str) {
    std::fs::write(dir.join(format!("{id}.desktop")), format!("[Desktop Entry]\nType=Application\nName={name}\nExec=true %U\n")).unwrap();
}

/// The activity log's lines.
fn log_lines(dir: &Path) -> Vec<Json> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let text = std::fs::read_to_string(entry.path()).unwrap_or_default();
        out.extend(text.lines().filter_map(|l| serde_json::from_str::<Json>(l).ok()));
    }
    out
}

fn touch(path: &Path, secs_ago: u64) {
    let when = SystemTime::now() - Duration::from_secs(secs_ago);
    std::fs::File::open(path).unwrap().set_modified(when).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn media_power_and_stars() {
    let Some(bus) = TestBus::start() else { return };
    let Some(system) = TestBus::start() else { return };
    if !python_gio() {
        eprintln!("PyGObject is not installed; skipping");
        return;
    }
    let tmp = std::env::temp_dir().join(format!("helix-agent-media-{}", std::process::id()));
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
        std::env::set_var("XDG_DATA_DIRS", tmp.join("sys"));
        std::env::set_var("XDG_RUNTIME_DIR", tmp.join("run"));
        // The stand-in system bus: fake UPower and power-profiles-daemon.
        std::env::set_var("DBUS_SYSTEM_BUS_ADDRESS", system.address());
        std::env::set_var("PULSE_SERVER", format!("unix:{}", tmp.join("no-pulse").display()));
        std::env::set_var("GIO_USE_VFS", "local");
    }
    desktop_file(&apps, "org.videolan.VLC", "VLC media player");
    desktop_file(&apps, "org.example.Vault", "Secret Vault");
    desktop_file(&apps, "org.example.Radio", "Radio");
    desktop_file(&apps, "org.helixdesktop.Settings", "Settings");

    // The players, each a process of its own.
    let player = |config: Json| FakePlayer::start(bus.address(), &config.to_string());
    let all_can = serde_json::json!({"CanControl": true, "CanPlay": true, "CanPause": true, "CanGoNext": true, "CanGoPrevious": true, "CanSeek": true});
    let vlc = player(serde_json::json!({
        "name": "vlc", "identity": "VLC media player", "desktop-entry": "org.videolan.VLC", "status": "Playing", "position": 42_000_000,
        "metadata": {"title": "Night Drive", "artist": ["Anna", "Ben"], "album": "Roads", "length": 200_000_000, "trackid": "/org/videolan/vlc/track/7",
                     "url": "file:///home/me/secret-diary.mp3", "art": "file:///home/me/cover.png"},
        "can": all_can,
    }));
    let firefox = player(serde_json::json!({
        "name": "firefox.instance_1_42", "identity": "Firefox", "status": "Paused", "metadata": {"title": "A video"},
        "can": {"CanControl": true, "CanPlay": true, "CanPause": true, "CanGoNext": false, "CanGoPrevious": true, "CanSeek": false},
    }));
    // Excluded: by DesktopEntry, by Identity, by its process's window, Settings'.
    let vault = player(serde_json::json!({"name": "aaa.vault", "identity": "Vault player", "desktop-entry": "org.example.Vault", "status": "Playing", "metadata": {"title": "Vault song"}, "can": all_can}));
    let sneaky = player(serde_json::json!({"name": "aab.sneaky", "identity": "Secret Vault", "status": "Playing", "can": all_can}));
    let hidden = player(serde_json::json!({"name": "aac.hidden", "identity": "Radio", "desktop-entry": "org.example.Radio", "status": "Playing", "can": all_can}));
    let settings_player = player(serde_json::json!({"name": "aad.settings", "identity": "Settings", "desktop-entry": "org.helixdesktop.Settings", "status": "Playing", "can": all_can}));
    // Owns its name, answers nothing: left out.
    let _broken = player(serde_json::json!({"name": "aae.broken", "serve": false}));
    let locked = player(serde_json::json!({"name": "zz.locked", "identity": "Kiosk", "desktop-entry": "org.example.Kiosk", "status": "Stopped", "can": {"CanControl": false}}));

    // The UI's view: the hidden player's process also shows a window of the
    // excluded app.
    let snapshot = Arc::new(Mutex::new(Snapshot {
        windows: vec![WindowInfo { id: 5, app_id: "org.example.Vault".into(), title: "Vault".into(), pid: hidden.pid(), geometry: (0, 0, 400, 300), ..Default::default() }],
        workspaces: 1,
        ..Default::default()
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
    let consent: agent::Consent = Arc::new(|_| Box::pin(async { Answer::Session }));
    let log_dir = tmp.join("state/helix/agent-activity");
    let access = AgentAccess::new(lock.clone(), Setup { grants_path: None, consent, confirm: agent::no_confirm(), proc_root: "/proc".into(), settings_exes: Vec::new(), log_dir: Some(log_dir.clone()), reminders_path: None, changes_path: None });
    access.set_desktop(Desktop { ui: ui.clone(), notifications: None });
    let server = bus.builder().serve_at(OBJECT_PATH, Agent1 { access: access.clone() }).unwrap().name("org.gnome.Shell").unwrap().build().await.unwrap();
    access.attach(&server);
    let conn = bus.connect().await;

    // The batch's methods are there, as the contract has them.
    let actual = introspect(&conn, "org.gnome.Shell", OBJECT_PATH).await;
    assert_conformant(IFACE, &check(IFACE, &load_dbus_contract("../agent-access/org.helixdesktop.Agent1.xml"), &actual, helix_test_support::agent::AGENT1_LATER));

    // Off: refused, and arguments are not looked at.
    for method in ["GetMedia", "GetPower", "StarredFiles"] {
        assert_eq!(short(&call(&conn, method, &(opts(&[]),)).await), "AccessOff", "{method}");
    }
    assert_eq!(short(&control(&conn, "", "bogus", opts(&[])).await), "AccessOff");

    settings.set_boolean("enabled", true).unwrap();
    settings.set_boolean("allow-remote", true).unwrap();
    settings.set_string("access-level", "read-only").unwrap();
    settings.set_strv("excluded-apps", ["org.example.Vault.desktop"]).unwrap();

    // ----- GetMedia ------------------------------------------------------
    let players = records(&conn, "GetMedia", opts(&[])).await;
    let ids: Vec<String> = players.iter().map(|p| s(p, "id")).collect();
    assert_eq!(ids, ["vlc", "firefox.instance_1_42", "zz.locked"], "the active one first, then by bus name; excluded and silent players left out");
    let v = &players[0];
    assert!(b(v, "active"), "the first playing player the agent may see");
    assert_eq!((s(v, "app-id"), s(v, "name"), s(v, "status")), ("org.videolan.VLC".into(), "VLC media player".into(), "playing".into()));
    assert_eq!((s(v, "title"), s(v, "album")), ("Night Drive".into(), "Roads".into()));
    assert_eq!(Vec::<String>::try_from(v["artist"].clone()).unwrap(), ["Anna", "Ben"]);
    assert_eq!((d(v, "length"), d(v, "position")), (200.0, 42.0));
    assert!(b(v, "can-seek") && b(v, "can-control"));
    let mut keys: Vec<&str> = v.keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(
        keys,
        ["active", "album", "app-id", "artist", "can-control", "can-go-next", "can-go-previous", "can-pause", "can-play", "can-seek", "id", "length", "name", "position", "status", "title"],
        "never the URL or the art"
    );
    let f = &players[1];
    assert_eq!((s(f, "status"), b(f, "active"), b(f, "can-seek")), ("paused".into(), false, false));
    assert!(!f.contains_key("app-id") && !f.contains_key("position") && !f.contains_key("artist"));
    let k = &players[2];
    assert_eq!((s(k, "status"), b(k, "can-control")), ("stopped".into(), false));

    // ----- MediaControl ----------------------------------------------------
    // A Read only grant is not enough.
    assert_eq!(short(&control(&conn, "", "play-pause", opts(&[])).await), "LevelTooLow");
    settings.set_string("access-level", "desktop").unwrap();
    // The active player: VLC (not the excluded players before it).
    control(&conn, "", "play-pause", opts(&[])).await.unwrap();
    assert!(vlc.wait_call("PlayPause", Duration::from_secs(5)), "{:?}", vlc.calls());
    control(&conn, "vlc", "seek", opts(&[("offset", Value::from(-10.5))])).await.unwrap();
    assert!(vlc.wait_call("Seek -10500000", Duration::from_secs(5)), "{:?}", vlc.calls());
    control(&conn, "vlc", "seek", opts(&[("position", Value::from(30.0))])).await.unwrap();
    assert!(vlc.wait_call("SetPosition /org/videolan/vlc/track/7 30000000", Duration::from_secs(5)), "{:?}", vlc.calls());
    control(&conn, "firefox.instance_1_42", "previous", opts(&[])).await.unwrap();
    assert!(firefox.wait_call("Previous", Duration::from_secs(5)));
    control(&conn, "vlc", "stop", opts(&[])).await.unwrap();
    assert!(vlc.wait_call("Stop", Duration::from_secs(5)));
    // What the players cannot do, and malformed calls.
    let next = control(&conn, "firefox.instance_1_42", "next", opts(&[])).await;
    assert_eq!(short(&next), "NotSupported", "CanGoNext is false");
    assert_eq!(short(&control(&conn, "firefox.instance_1_42", "seek", opts(&[("offset", Value::from(5.0))])).await), "NotSupported");
    assert_eq!(short(&control(&conn, "zz.locked", "play", opts(&[])).await), "NotSupported", "CanControl is false");
    assert_eq!(short(&control(&conn, "vlc", "rewind", opts(&[])).await), "NotSupported");
    assert_eq!(short(&control(&conn, "vlc", "seek", opts(&[])).await), "org.freedesktop.DBus.Error.InvalidArgs");
    assert_eq!(short(&control(&conn, "vlc", "seek", opts(&[("offset", Value::from(1.0)), ("position", Value::from(2.0))])).await), "org.freedesktop.DBus.Error.InvalidArgs");
    assert_eq!(short(&control(&conn, "nope", "play", opts(&[])).await), "NotFound");
    assert_eq!(short(&control(&conn, "aae.broken", "play", opts(&[])).await), "NotFound", "a player that does not answer is not there");
    // Excluded apps' players and Settings': refused, never reached.
    for (id, token) in [("aaa.vault", "app:org.example.Vault"), ("aab.sneaky", "app:org.example.Vault"), ("aac.hidden", "app:org.example.Vault"), ("aad.settings", "settings")] {
        let refused = control(&conn, id, "play", opts(&[])).await;
        assert_eq!((short(&refused), message(&refused)), ("Excluded".into(), token.into()), "{id}");
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    for (p, name) in [(&vault, "vault"), (&sneaky, "sneaky"), (&hidden, "hidden"), (&settings_player, "settings")] {
        assert!(p.calls().is_empty(), "{name}: {:?}", p.calls());
    }
    // Sensitive when marked: the user is asked, or the call refused.
    settings.set_boolean("confirm-sensitive", true).unwrap();
    let marked = control(&conn, "vlc", "next", opts(&[("sensitive", Value::from(true)), ("interactive", Value::from(false))])).await;
    assert_eq!(short(&marked), "NeedsConfirmation");
    settings.set_boolean("confirm-sensitive", false).unwrap();
    // Without the excluded app, the vault's player is listed and is the
    // active one again.
    settings.set_strv("excluded-apps", Vec::<&str>::new()).unwrap();
    let players = records(&conn, "GetMedia", opts(&[])).await;
    assert_eq!(s(&players[0], "id"), "aaa.vault");
    assert!(!players.iter().any(|p| s(p, "id") == "aad.settings"), "Settings' is always left out");
    settings.set_strv("excluded-apps", ["org.example.Vault"]).unwrap();

    // ----- GetPower ----------------------------------------------------------
    let fakes = upower::serve(&system, Some(Battery::default()), Some(("balanced", &["power-saver", "balanced", "performance"]))).await;
    let reply = call(&conn, "GetPower", &(opts(&[]),)).await.unwrap();
    let power: Record = reply.body().deserialize().unwrap();
    assert!(b(&power, "on-battery"));
    assert_eq!(d(&power, "percentage"), 54.0);
    assert_eq!((s(&power, "battery-state"), s(&power, "warning-level")), ("discharging".into(), "none".into()));
    assert_eq!(i64::try_from(power["time-to-empty"].clone()).unwrap(), 7800);
    assert!(!power.contains_key("time-to-full"), "absent when UPower gives 0");
    assert_eq!(s(&power, "power-profile"), "balanced");
    assert_eq!(Vec::<String>::try_from(power["power-profiles"].clone()).unwrap(), ["power-saver", "balanced", "performance"]);
    drop(fakes);
    // Line power only, no power-profiles-daemon: only what there is.
    let fakes = upower::serve(&system, Some(Battery { on_battery: false, kind: 1, ..Battery::default() }), None).await;
    let power: Record = call(&conn, "GetPower", &(opts(&[]),)).await.unwrap().body().deserialize().unwrap();
    assert_eq!(power.keys().map(String::as_str).collect::<Vec<_>>(), ["on-battery"]);
    drop(fakes);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let power: Record = call(&conn, "GetPower", &(opts(&[]),)).await.unwrap().body().deserialize().unwrap();
    assert!(power.is_empty(), "nothing at all: {{}}, not an error");

    // ----- StarredFiles ------------------------------------------------------
    assert!(records(&conn, "StarredFiles", opts(&[])).await.is_empty(), "no store: none");
    let docs = home.join("Documents");
    let pictures = home.join("Pictures");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::create_dir_all(&pictures).unwrap();
    std::fs::write(docs.join("report.pdf"), b"%PDF").unwrap();
    std::fs::write(docs.join("notes.txt"), b"notes").unwrap();
    std::fs::write(home.join("gone.txt"), b"x").unwrap();
    let mut store = helix_settings::stars::Store::open(&data_home, true);
    if store.access() == helix_settings::stars::Access::ReadWrite {
        let uri = |p: &Path| helix_settings::stars::uri_for_path(p).unwrap();
        let starred = [uri(&docs.join("report.pdf")), uri(&docs.join("notes.txt")), uri(&pictures), uri(&home.join("gone.txt"))];
        store.set_starred(&starred, true).unwrap();
        drop(store);
        std::fs::remove_file(home.join("gone.txt")).unwrap();
        touch(&docs.join("report.pdf"), 300);
        touch(&docs.join("notes.txt"), 100);
        touch(&pictures, 200);
        let files = records(&conn, "StarredFiles", opts(&[])).await;
        let names: Vec<String> = files.iter().map(|f| s(f, "name")).collect();
        assert_eq!(names, ["notes.txt", "Pictures", "report.pdf"], "most recently modified first; gone files left out");
        assert_eq!(s(&files[1], "mime-type"), "inode/directory");
        assert!(!files[1].contains_key("size") && files[0].contains_key("size"), "folders have no size");
        assert!(!files[0].contains_key("used"));
        assert_eq!(s(&files[2], "uri"), gio::File::for_path(docs.join("report.pdf")).uri().to_string());
        // (No MIME database here: folders are the type to tell apart.)
        let folders = records(&conn, "StarredFiles", opts(&[("type", Value::from("inode/directory"))])).await;
        assert_eq!(folders.iter().map(|f| s(f, "name")).collect::<Vec<_>>(), ["Pictures"]);
        assert_eq!(records(&conn, "StarredFiles", opts(&[("type", Value::from("inode/*"))])).await.len(), 1);
        let first = records(&conn, "StarredFiles", opts(&[("limit", Value::from(1u32))])).await;
        assert_eq!(first.iter().map(|f| s(f, "name")).collect::<Vec<_>>(), ["notes.txt"]);
        assert_eq!(short(&call(&conn, "StarredFiles", &(opts(&[("limit", Value::from("many"))]),)).await), "org.freedesktop.DBus.Error.InvalidArgs");
        // A store that cannot be read: none, not an error.
        let db = helix_settings::stars::store_dir(&data_home).join("meta.db");
        for extra in ["meta.db-wal", "meta.db-shm"] {
            let _ = std::fs::remove_file(helix_settings::stars::store_dir(&data_home).join(extra));
        }
        std::fs::write(&db, b"this is not a database").unwrap();
        assert!(records(&conn, "StarredFiles", opts(&[])).await.is_empty());
    } else {
        eprintln!("TinySPARQL is not installed; only the empty cases of StarredFiles");
    }

    // ----- The activity log ----------------------------------------------------
    let lines = log_lines(&log_dir);
    let find = |pred: &dyn Fn(&Json) -> bool| lines.iter().find(|l| pred(l)).cloned();
    let played = find(&|l| l["method"] == "MediaControl" && l["action"] == "play-pause" && l["outcome"] == "allowed").expect("the play-pause line");
    assert_eq!(played["target"], "player:vlc (org.videolan.VLC)");
    assert_eq!(played["level"], "desktop");
    let refused = find(&|l| l["method"] == "MediaControl" && l["target"] == "player:aaa.vault (org.example.Vault)").expect("the vault's line");
    assert_eq!((refused["outcome"].clone(), refused["reason"].clone(), refused["action"].clone()), (Json::from("refused:Excluded"), Json::from("app"), Json::from("play")));
    let settings_line = find(&|l| l["method"] == "MediaControl" && l["target"] == "player:aad.settings (org.helixdesktop.Settings)").expect("Settings' player's line");
    assert_eq!(settings_line["reason"], "settings");
    let failed = find(&|l| l["method"] == "MediaControl" && l["target"] == "player:firefox.instance_1_42" && l["action"] == "next").expect("the failed next");
    assert_eq!((failed["outcome"].clone(), failed["error"].clone()), (Json::from("allowed"), Json::from("NotSupported")));
    let seek = find(&|l| l["method"] == "MediaControl" && l["action"] == "seek").expect("a seek line");
    assert_eq!(seek["outcome"], "allowed");
    assert!(find(&|l| l["method"] == "GetMedia" && l["outcome"] == "allowed").is_some_and(|l| l.get("target").is_none()));
    assert!(find(&|l| l["method"] == "GetPower").is_some());
    assert!(find(&|l| l["method"] == "StarredFiles").is_some());
    // Never what the players show.
    let all = lines.iter().map(Json::to_string).collect::<String>();
    for secret in ["Night Drive", "Anna", "Roads", "secret-diary", "Vault song", "A video"] {
        assert!(!all.contains(secret), "{secret} in the log");
    }

    drop((vlc, firefox, vault, sneaky, hidden, settings_player, locked));
    let _ = std::fs::remove_dir_all(&tmp);
}
