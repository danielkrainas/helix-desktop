//! `helix-agent media`, `power` and `files starred` end to end
//! (helixdesktop/spec `contracts/agent-access/cli.md`;
//! helixdesktop/desktop#156): a headless Helix session (helix-comp with
//! helix-shell, scratch home, private bus), fake MPRIS players on its bus,
//! and fake UPower and power-profiles-daemon on a second private bus the
//! shell takes for the system bus.
//!
//! - `media` lists the players, the active one first; an excluded app's
//!   player is never listed and `media … --player` refuses it (exit 14).
//! - `media play-pause` and `media seek` reach the player.
//! - `power` reads the battery and the power mode.
//! - `files starred` lists the starred files (when TinySPARQL is there).
//! - The activity log has the calls, never what the players play.
//!
//! Needs helix-comp and helix-shell built next to this test, and
//! PyGObject; skipped otherwise.

use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use helix_test_support::headless::{sibling_binary, HeadlessSession, Options};
use helix_test_support::mpris::{python_gio, FakePlayer};
use helix_test_support::upower::{self, Battery};
use helix_test_support::TestBus;
use serde_json::Value as Json;

const CLI: &str = env!("CARGO_BIN_EXE_helix-agent");

/// Run the CLI on `bus` with `home` as every XDG directory.
fn run(bus: &str, home: &Path, args: &[&str]) -> Output {
    Command::new(CLI)
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("LANG", "C.UTF-8")
        .env("DBUS_SESSION_BUS_ADDRESS", bus)
        .env("HELIX_AGENT_NAME", "Media test")
        .output()
        .expect("run helix-agent")
}

fn json(out: &Output) -> Json {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("not JSON ({e}): {}", String::from_utf8_lossy(&out.stdout)))
}

fn code(out: &Output) -> i32 {
    out.status.code().expect("exited")
}

fn text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Run until `ok` holds for the output (settings take a moment).
fn until(what: &str, mut f: impl FnMut() -> Output, ok: impl Fn(&Output) -> bool) -> Output {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let out = f();
        if ok(&out) {
            return out;
        }
        assert!(Instant::now() < deadline, "{what}: {}", String::from_utf8_lossy(&out.stdout));
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn touch(path: &Path, secs_ago: u64) {
    let when = std::time::SystemTime::now() - Duration::from_secs(secs_ago);
    std::fs::File::open(path).unwrap().set_modified(when).unwrap();
}

#[test]
fn media_power_and_stars() {
    let (Some(comp), Some(shell)) = (sibling_binary("helix-comp"), sibling_binary("helix-shell")) else {
        eprintln!("helix-comp or helix-shell not built; skipping");
        return;
    };
    if !python_gio() {
        eprintln!("PyGObject is not installed; skipping");
        return;
    }
    // The stand-in system bus, with UPower and power-profiles-daemon.
    let Some(system) = TestBus::start() else { return };
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
    let _power = runtime.block_on(upower::serve(&system, Some(Battery::default()), Some(("balanced", &["power-saver", "balanced", "performance"]))));

    let schemas = std::env::temp_dir().join(format!("helix-agent-media-schemas-{}", std::process::id()));
    std::fs::create_dir_all(&schemas).unwrap();
    for schema in ["org.helixdesktop.agent-access", "org.helixdesktop.shell", "org.helixdesktop.extensions"] {
        let data = format!("{}/../../data/{schema}.gschema.xml", env!("CARGO_MANIFEST_DIR"));
        std::fs::copy(data, schemas.join(format!("{schema}.gschema.xml"))).unwrap();
    }
    let Ok(compiled) = Command::new("glib-compile-schemas").arg(&schemas).status() else {
        eprintln!("glib-compile-schemas is not installed; skipping");
        return;
    };
    assert!(compiled.success());
    let on = "[org/gnome/desktop/interface]\nenable-hot-corners=false\n\n[org/helixdesktop/agent-access]\nenabled=true\nallow-remote=true\naccess-level='desktop'\nremember-mode='remember'\nexcluded-apps=['org.example.Vault']\n";
    // This test runs the CLI, so the shell sees this test as the agent;
    // the user allowed it to control the desktop.
    let me = std::env::current_exe().unwrap().to_string_lossy().into_owned();
    let grants = format!("version = 1\n\n[[agent]]\nid = \"{me}\"\nexecutable = \"{me}\"\nname = \"\"\nlevel = \"desktop\"\ngranted = 1\n");
    let env = vec![("GSETTINGS_SCHEMA_DIR".into(), schemas.to_string_lossy().into_owned()), ("DBUS_SYSTEM_BUS_ADDRESS".into(), system.address().to_owned())];
    let Some(session) = HeadlessSession::start_prepared(Options { comp: &comp, shell: Some(&shell), size: (800, 600), settings: on, env }, |home| {
        std::fs::create_dir_all(home.join(".config/helix")).unwrap();
        std::fs::write(home.join(".config/helix/agent-grants.toml"), &grants).unwrap();
    }) else {
        return;
    };
    assert!(session.wait_for_log("shell ready", Duration::from_secs(30)), "the shell did not start:\n{}", session.log());
    let (bus, home) = (session.bus().address().to_owned(), session.home().to_owned());
    let cli = |args: &[&str]| run(&bus, &home, args);

    // ----- Media players ----------------------------------------------------
    let out = cli(&["media"]);
    assert_eq!((code(&out), json(&out)), (0, serde_json::json!({"players": []})), "none yet");
    assert_eq!(text(&cli(&["--text", "media"])).trim(), "No media players.");
    let all_can = serde_json::json!({"CanControl": true, "CanPlay": true, "CanPause": true, "CanGoNext": true, "CanGoPrevious": true, "CanSeek": true});
    let vlc = FakePlayer::start(
        &bus,
        &serde_json::json!({
            "name": "vlc", "identity": "VLC media player", "desktop-entry": "org.videolan.VLC", "status": "Playing", "position": 42_000_000,
            "metadata": {"title": "Night Drive", "artist": ["Anna"], "length": 200_000_000, "trackid": "/org/videolan/vlc/track/7", "url": "file:///home/me/secret-diary.mp3"},
            "can": all_can,
        })
        .to_string(),
    );
    let vault = FakePlayer::start(&bus, &serde_json::json!({"name": "aaa.vault", "identity": "Vault", "desktop-entry": "org.example.Vault", "status": "Playing", "metadata": {"title": "Vault song"}, "can": all_can}).to_string());
    let out = cli(&["media"]);
    assert_eq!(code(&out), 0, "{}", text(&out));
    let players = json(&out)["players"].clone();
    assert_eq!(players.as_array().unwrap().len(), 1, "the excluded app's player is left out: {players}");
    let v = &players[0];
    assert_eq!((v["id"].clone(), v["active"].clone(), v["status"].clone(), v["title"].clone()), (Json::from("vlc"), Json::from(true), Json::from("playing"), Json::from("Night Drive")));
    assert_eq!((v["length"].clone(), v["position"].clone(), v["artist"].clone()), (Json::from(200.0), Json::from(42.0), serde_json::json!(["Anna"])));
    assert!(v.get("url").is_none() && v.get("xesam:url").is_none());
    assert_eq!(json(&cli(&["media", "list"]))["players"], players, "“media list” is “media”");
    let shown = text(&cli(&["--text", "media"]));
    assert!(shown.starts_with("*  VLC media player  vlc  playing  Night Drive — Anna"), "{shown}");

    // Control: the active player, a seek from where it is and from the start.
    let out = cli(&["media", "play-pause"]);
    assert_eq!((code(&out), json(&out)), (0, serde_json::json!({})), "{}", text(&out));
    assert!(vlc.wait_call("PlayPause", Duration::from_secs(5)), "{:?}", vlc.calls());
    assert_eq!(code(&cli(&["media", "seek", "+5"])), 0);
    assert!(vlc.wait_call("Seek 5000000", Duration::from_secs(5)), "{:?}", vlc.calls());
    assert_eq!(code(&cli(&["media", "seek", "-2.5", "--player", "vlc"])), 0);
    assert!(vlc.wait_call("Seek -2500000", Duration::from_secs(5)), "{:?}", vlc.calls());
    assert_eq!(code(&cli(&["media", "seek", "12.5"])), 0);
    assert!(vlc.wait_call("SetPosition /org/videolan/vlc/track/7 12500000", Duration::from_secs(5)), "{:?}", vlc.calls());
    // Paused now: still the active one (no other the agent may see plays).
    assert_eq!(json(&cli(&["media"]))["players"][0]["status"], "paused");
    // The excluded app's player: refused, never reached.
    let out = cli(&["media", "play", "--player", "aaa.vault"]);
    assert_eq!((code(&out), json(&out)["error"]["code"].clone()), (14, Json::from("excluded")), "{}", text(&out));
    assert!(vault.calls().is_empty(), "{:?}", vault.calls());
    let out = cli(&["media", "next", "--player", "nope"]);
    assert_eq!((code(&out), json(&out)["error"]["code"].clone()), (22, Json::from("not-found")));
    let out = cli(&["media", "seek", "soon"]);
    assert_eq!(code(&out), 2, "a usage error, before calling the shell");

    // ----- Power -------------------------------------------------------------
    let out = cli(&["power"]);
    assert_eq!(code(&out), 0, "{}", text(&out));
    let power = json(&out);
    assert_eq!((power["percentage"].clone(), power["battery-state"].clone(), power["power-profile"].clone()), (Json::from(54.0), Json::from("discharging"), Json::from("balanced")));
    assert_eq!(power["power-profiles"], serde_json::json!(["power-saver", "balanced", "performance"]));
    assert_eq!(json(&cli(&["power", "status"])), power);
    assert_eq!(text(&cli(&["--text", "power"])).trim(), "Battery: 54%, discharging, 2 h 10 min left\nPower mode: Balanced (also: Power Saver, Performance)");

    // ----- Starred files ------------------------------------------------------
    let out = cli(&["files", "starred"]);
    assert_eq!((code(&out), json(&out)), (0, serde_json::json!({"files": []})), "no store: none");
    let data_home = home.join(".local/share");
    let docs = home.join("Documents");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(docs.join("report.pdf"), b"%PDF").unwrap();
    std::fs::write(docs.join("notes.txt"), b"notes").unwrap();
    let mut store = helix_settings::stars::Store::open(&data_home, true);
    if store.access() == helix_settings::stars::Access::ReadWrite {
        let uri = |p: &Path| helix_settings::stars::uri_for_path(p).unwrap();
        store.set_starred(&[uri(&docs.join("report.pdf")), uri(&docs.join("notes.txt"))], true).unwrap();
        drop(store);
        touch(&docs.join("report.pdf"), 100);
        touch(&docs.join("notes.txt"), 300);
        let files = json(&cli(&["files", "starred"]))["files"].clone();
        let names: Vec<&str> = files.as_array().unwrap().iter().map(|f| f["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["report.pdf", "notes.txt"], "most recently modified first");
        let one = json(&cli(&["files", "starred", "--limit", "1"]))["files"].clone();
        assert_eq!(one.as_array().unwrap().len(), 1);
        assert!(text(&cli(&["--text", "files", "starred"])).contains("Documents/report.pdf"));
    } else {
        eprintln!("TinySPARQL is not installed; only the empty case of files starred");
    }

    // ----- The activity log --------------------------------------------------
    let log = until("the log has the calls", || cli(&["log", "--agent", &me]), |o| text(o).contains("MediaControl"));
    let entries = json(&log)["entries"].as_array().unwrap().clone();
    let played = entries.iter().find(|e| e["method"] == "MediaControl" && e["action"] == "play-pause").expect("play-pause");
    assert_eq!((played["target"].clone(), played["outcome"].clone()), (Json::from("player:vlc (org.videolan.VLC)"), Json::from("allowed")));
    let refused = entries.iter().find(|e| e["method"] == "MediaControl" && e["outcome"] == "refused:Excluded").expect("the refusal");
    assert_eq!((refused["target"].clone(), refused["reason"].clone()), (Json::from("player:aaa.vault (org.example.Vault)"), Json::from("app")));
    for method in ["GetMedia", "GetPower", "StarredFiles"] {
        assert!(entries.iter().any(|e| e["method"] == method), "{method}");
    }
    let all = text(&log);
    for secret in ["Night Drive", "Anna", "secret-diary", "Vault song"] {
        assert!(!all.contains(secret), "{secret} in the log");
    }

    drop((vlc, vault));
    drop(session);
    let _ = std::fs::remove_dir_all(&schemas);
}
