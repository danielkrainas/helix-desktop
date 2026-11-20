//! `helix-agent`'s commands that change settings, end to end
//! (helixdesktop/spec `contracts/agent-access/cli.md`;
//! helixdesktop/desktop#157): a headless Helix session (helix-comp with
//! helix-shell, scratch home, private bus, keyfile settings), a fake
//! power-profiles-daemon on a second private bus the shell takes for the
//! system bus, a fake file manager and a fake app's notification on the
//! session's bus.
//!
//! - `accent`, `power mode`, `wallpaper`, `dock pin`, `files star` print
//!   Change records and change the settings; a second call changes
//!   nothing;
//! - `--until` is checked before the shell is called, and the shell puts
//!   the old value back when it comes (`accent … --until`), and `toggle
//!   --until` says when;
//! - `wallpaper` copies the picture and refuses hidden folders (exit 14);
//! - `notifications` lists buttons, `notifications press` and `dismiss`
//!   reach the app; `files reveal` reaches the file manager;
//! - the activity log has the changes with old and new values and the
//!   restore.
//!
//! Needs helix-comp and helix-shell built next to this test and GNOME's
//! schemas; skipped otherwise.

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use helix_test_support::headless::{sibling_binary, HeadlessSession, Options};
use helix_test_support::upower;
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
        .env("TZ", "UTC")
        .env("DBUS_SESSION_BUS_ADDRESS", bus)
        .env("HELIX_AGENT_NAME", "Changes test")
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
    format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
}

/// Wait (at most `secs`) until `ok` holds.
fn until(what: &str, secs: u64, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !ok() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A key of the session's keyfile settings, as written there.
fn setting(keyfile: &Path, group: &str, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(keyfile).ok()?;
    let mut in_group = false;
    for line in text.lines() {
        if let Some(g) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_group = g == group;
        } else if in_group {
            if let Some(v) = line.strip_prefix(&format!("{key}=")) {
                return Some(v.to_owned());
            }
        }
    }
    None
}

/// A tiny PNG.
fn png() -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, 2, 2);
        enc.set_color(png::ColorType::Rgba);
        let mut w = enc.write_header().unwrap();
        w.write_image_data(&[90; 16]).unwrap();
    }
    out
}

#[test]
fn settings_changes_end_to_end() {
    let (Some(comp), Some(shell)) = (sibling_binary("helix-comp"), sibling_binary("helix-shell")) else {
        eprintln!("helix-comp or helix-shell not built; skipping");
        return;
    };
    let Some(system) = TestBus::start() else { return };
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let _power = runtime.block_on(upower::serve(&system, None, Some(("balanced", &["power-saver", "balanced", "performance"]))));
    let system_conn = runtime.block_on(system.connect());

    let schemas = std::env::temp_dir().join(format!("helix-agent-changes-schemas-{}", std::process::id()));
    std::fs::create_dir_all(&schemas).unwrap();
    for schema in ["org.helixdesktop.agent-access", "org.helixdesktop.shell", "org.helixdesktop.extensions"] {
        let data = format!("{}/../../data/{schema}.gschema.xml", env!("CARGO_MANIFEST_DIR"));
        std::fs::copy(data, schemas.join(format!("{schema}.gschema.xml"))).unwrap();
    }
    for file in [
        "org.gnome.desktop.interface.gschema.xml",
        "org.gnome.desktop.background.gschema.xml",
        "org.gnome.desktop.notifications.gschema.xml",
        "org.gnome.settings-daemon.plugins.color.gschema.xml",
        "org.gnome.shell.gschema.xml",
        "org.gnome.desktop.enums.xml",
        "org.gnome.settings-daemon.enums.xml",
    ] {
        if std::fs::copy(Path::new("/usr/share/glib-2.0/schemas").join(file), schemas.join(file)).is_err() {
            eprintln!("GNOME's schemas are not installed ({file}); skipping");
            let _ = std::fs::remove_dir_all(&schemas);
            return;
        }
    }
    let Ok(compiled) = Command::new("glib-compile-schemas").arg(&schemas).status() else {
        eprintln!("glib-compile-schemas is not installed; skipping");
        return;
    };
    assert!(compiled.success());
    let on = "[org/gnome/desktop/interface]\nenable-hot-corners=false\n\n[org/helixdesktop/agent-access]\nenabled=true\nallow-remote=true\naccess-level='desktop'\nremember-mode='remember'\nnotifications=true\nexcluded-apps=['org.example.Vault']\n";
    // This test runs the CLI, so the shell sees this test as the agent.
    let me = std::env::current_exe().unwrap().to_string_lossy().into_owned();
    let grants = format!("version = 1\n\n[[agent]]\nid = \"{me}\"\nexecutable = \"{me}\"\nname = \"\"\nlevel = \"desktop\"\ngranted = 1\n");
    let env = vec![("GSETTINGS_SCHEMA_DIR".into(), schemas.to_string_lossy().into_owned()), ("DBUS_SYSTEM_BUS_ADDRESS".into(), system.address().to_owned())];
    let Some(session) = HeadlessSession::start_prepared(Options { comp: &comp, shell: Some(&shell), size: (800, 600), settings: on, env }, |home| {
        std::fs::create_dir_all(home.join(".config/helix")).unwrap();
        std::fs::write(home.join(".config/helix/agent-grants.toml"), &grants).unwrap();
        let apps = home.join(".local/share/applications");
        std::fs::create_dir_all(&apps).unwrap();
        std::fs::write(apps.join("org.gnome.TextEditor.desktop"), "[Desktop Entry]\nType=Application\nName=Text Editor\nExec=true %U\n").unwrap();
        for dir in ["Pictures", "Documents", ".cache/app"] {
            std::fs::create_dir_all(home.join(dir)).unwrap();
        }
    }) else {
        let _ = std::fs::remove_dir_all(&schemas);
        return;
    };
    assert!(session.wait_for_log("shell ready", Duration::from_secs(30)), "the shell did not start:\n{}", session.log());
    let (bus, home) = (session.bus().address().to_owned(), session.home().to_owned());
    let cli = |args: &[&str]| run(&bus, &home, args);
    let keyfile = home.join(".config/glib-2.0/settings/keyfile");
    let accent = || setting(&keyfile, "org/gnome/desktop/interface", "accent-color");

    // ----- The accent color ---------------------------------------------------
    let out = cli(&["accent", "red"]);
    assert_eq!(code(&out), 0, "{}", text(&out));
    let change = json(&out);
    assert_eq!((change["changed"].clone(), change["name"].clone(), change["old"].clone(), change["new"].clone()), (Json::from(true), Json::from("accent"), Json::from("blue"), Json::from("red")));
    assert_eq!(change["id"].as_str().map(str::len), Some(16));
    until("the accent is red", 5, || accent().as_deref() == Some("'red'"));
    assert_eq!(text(&cli(&["--text", "accent", "red"])).trim(), "Accent color is already Red");
    let out = cli(&["accent", "magenta"]);
    assert_eq!(code(&out), 2, "not a color: before calling the shell");
    // --until: checked first.
    for when in ["+5s", "+2d", "-1h", "soon"] {
        let out = cli(&["accent", "teal", "--until", when]);
        assert_eq!((code(&out), json(&out)["error"]["code"].clone()), (2, Json::from("usage")), "{when}: {}", text(&out));
    }
    assert_eq!(accent().as_deref(), Some("'red'"));
    // Temporary: put back by the shell when it comes, the CLI long gone.
    let out = cli(&["--text", "accent", "green", "--until", "+12s"]);
    assert_eq!(code(&out), 0, "{}", text(&out));
    assert!(text(&out).starts_with("Accent color: Red → Green, back at "), "{}", text(&out));
    until("the accent is green", 5, || accent().as_deref() == Some("'green'"));
    until("the accent is back to red", 40, || accent().as_deref() == Some("'red'"));

    // ----- Quick settings with --until ------------------------------------------
    let out = cli(&["toggle", "dark-style", "on", "--until", "+1h"]);
    assert_eq!(code(&out), 0, "{}", text(&out));
    let toggled = json(&out);
    assert_eq!((toggled["name"].clone(), toggled["on"].clone()), (Json::from("dark-style"), Json::from(true)));
    let at = toggled["until"].as_i64().unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    assert!((now + 3590..=now + 3600).contains(&at), "{toggled}");
    until("dark style on", 5, || setting(&keyfile, "org/gnome/desktop/interface", "color-scheme").as_deref() == Some("'prefer-dark'"));
    assert_eq!(json(&cli(&["state"]))["accent"], "red");

    // ----- Power mode -----------------------------------------------------------
    let out = cli(&["power", "mode", "performance"]);
    assert_eq!(code(&out), 0, "{}", text(&out));
    assert_eq!((json(&out)["old"].clone(), json(&out)["new"].clone()), (Json::from("balanced"), Json::from("performance")));
    assert_eq!(runtime.block_on(upower::active_profile(&system_conn)).as_deref(), Some("performance"));
    assert_eq!(json(&cli(&["power"]))["power-profile"], "performance");
    assert_eq!(text(&cli(&["--text", "power", "mode", "performance"])).trim(), "Power mode is already Performance");

    // ----- The background -------------------------------------------------------
    let sea = home.join("Pictures/sea.png");
    std::fs::write(&sea, png()).unwrap();
    let out = cli(&["wallpaper", "~/Pictures/sea.png", "--fit", "zoom"]);
    assert_eq!(code(&out), 0, "{}", text(&out));
    let set = json(&out);
    let copy = set["new"].as_str().unwrap().to_owned();
    assert!(copy.starts_with(&format!("file://{}/.local/share/backgrounds/", home.display())) && copy.ends_with("-sea.png"), "{set}");
    assert_eq!(set["new-dark"], copy.as_str());
    until("the background is the copy", 5, || setting(&keyfile, "org/gnome/desktop/background", "picture-uri").as_deref() == Some(&format!("'{copy}'")));
    std::fs::write(home.join(".cache/app/secret.png"), png()).unwrap();
    let out = cli(&["wallpaper", "~/.cache/app/secret.png"]);
    assert_eq!((code(&out), json(&out)["error"]["message"].clone()), (14, Json::from("Agents can't use files in hidden or system folders")));
    let out = cli(&["wallpaper", "~/Pictures/none.png"]);
    assert_eq!(code(&out), 22);

    // ----- The dock and stars -----------------------------------------------------
    // Text Editor is among GNOME's default favorites: out, then back in at
    // the end.
    let out = cli(&["dock", "unpin", "org.gnome.TextEditor"]);
    assert_eq!(code(&out), 0, "{}", text(&out));
    assert_eq!((json(&out)["changed"].clone(), json(&out)["target"].clone(), json(&out)["old"].clone(), json(&out)["new"].clone()), (Json::from(true), Json::from("org.gnome.TextEditor"), Json::from(true), Json::from(false)));
    let favorites = || setting(&keyfile, "org/gnome/shell", "favorite-apps").unwrap_or_default();
    until("unpinned", 5, || favorites().starts_with('[') && !favorites().contains("org.gnome.TextEditor.desktop"));
    assert_eq!(json(&cli(&["dock", "pin", "org.gnome.TextEditor"]))["new"], true);
    until("pinned at the end", 5, || favorites().ends_with("'org.gnome.TextEditor.desktop']"));
    assert_eq!(code(&cli(&["dock", "pin", "org.example.Vault"])), 14, "an excluded app");
    let report = home.join("Documents/report.pdf");
    std::fs::write(&report, b"%PDF").unwrap();
    let out = cli(&["files", "star", "~/Documents/report.pdf"]);
    match code(&out) {
        0 => {
            assert_eq!(json(&out)["new"], true);
            let starred = json(&cli(&["files", "starred"]));
            assert!(starred["files"].as_array().unwrap().iter().any(|f| f["name"] == "report.pdf"), "{starred}");
            assert_eq!(json(&cli(&["files", "unstar", "~/Documents/report.pdf"]))["new"], false);
        }
        23 => eprintln!("TinySPARQL is not installed; stars cannot be written"),
        other => panic!("files star: {other}: {}", text(&out)),
    }
    assert_eq!(code(&cli(&["files", "star", "/etc/hostname"])), 23, "outside the home folder");

    // ----- Notifications: a fake app's, with buttons ------------------------------
    let invoked: Arc<Mutex<Vec<(String, u32, String)>>> = Default::default();
    let (app, id) = runtime.block_on(async {
        let conn = session.bus().connect().await;
        let rule = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).interface("org.freedesktop.Notifications").unwrap().build();
        let mut stream = zbus::MessageStream::for_match_rule(rule, &conn, None).await.unwrap();
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
        let hints: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::from([("desktop-entry", zbus::zvariant::Value::from("org.gnome.Fractal"))]);
        let reply = conn
            .call_method(Some("org.freedesktop.Notifications"), "/org/freedesktop/Notifications", Some("org.freedesktop.Notifications"), "Notify", &("Chat", 0u32, "", "Anna wrote", "Lunch?", vec!["open", "Show", "archive", "Archive"], hints, -1i32))
            .await
            .unwrap();
        let id: u32 = reply.body().deserialize().unwrap();
        (conn, id)
    });
    let listed = json(&cli(&["notifications"]));
    let note = listed["notifications"].as_array().unwrap().iter().find(|n| n["id"] == id).cloned().expect("listed");
    assert_eq!(note["actions"], serde_json::json!([{"id": "open", "label": "Show"}, {"id": "archive", "label": "Archive"}]));
    assert_eq!(note["default-action"], false);
    let shown = text(&cli(&["--text", "notifications"]));
    assert!(shown.contains(&format!("{id}  ")) && shown.contains("[archive] Archive"), "{shown}");
    let out = cli(&["notifications", "press", &id.to_string(), "archive"]);
    assert_eq!(code(&out), 0, "{}", text(&out));
    until("ActionInvoked archive", 5, || invoked.lock().unwrap().contains(&("ActionInvoked".into(), id, "archive".into())));
    assert_eq!(code(&cli(&["notifications", "press", &id.to_string(), "nope"])), 22);
    let other = runtime.block_on(async {
        let hints: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::new();
        let reply = app
            .call_method(Some("org.freedesktop.Notifications"), "/org/freedesktop/Notifications", Some("org.freedesktop.Notifications"), "Notify", &("Chat", 0u32, "", "Another", "", Vec::<&str>::new(), hints, -1i32))
            .await
            .unwrap();
        reply.body().deserialize::<u32>().unwrap()
    });
    assert_eq!(code(&cli(&["notifications", "dismiss", &other.to_string()])), 0);
    until("NotificationClosed 2", 5, || invoked.lock().unwrap().contains(&("NotificationClosed".into(), other, "2".into())));
    assert_eq!(code(&cli(&["notifications", "--limit", "1", "dismiss", "1"])), 2);

    // ----- Revealing a file ---------------------------------------------------------
    let (_files, revealed) = runtime.block_on(helix_test_support::file_manager::serve(session.bus()));
    let out = cli(&["files", "reveal", "~/Documents/report.pdf"]);
    assert_eq!(code(&out), 0, "{}", text(&out));
    let report_uri = format!("file://{}", report.display());
    assert_eq!(revealed.lock().unwrap().last().map(|(u, _)| u.clone()), Some(vec![report_uri.clone()]));

    // ----- Windows ------------------------------------------------------------------
    assert_eq!(code(&cli(&["window", "to-monitor", "4242", "HEADLESS-9"])), 22);
    assert_eq!(code(&cli(&["window", "to-monitor", "4242"])), 2, "a monitor is needed");

    // ----- The activity log -----------------------------------------------------------
    let log = json(&cli(&["log", "--agent", &me]));
    let entries = log["entries"].as_array().unwrap().clone();
    let set_red = entries.iter().find(|e| e["method"] == "SetAccent" && e["new"] == "red" && e.get("change").is_some()).expect("SetAccent");
    assert_eq!(set_red["old"], "blue");
    let restored = entries.iter().find(|e| e["method"] == "Restore").expect("the restore");
    assert_eq!((restored["old"].clone(), restored["new"].clone(), restored["cause"].clone()), (Json::from("green"), Json::from("red"), Json::from("expired")));
    let wall = entries.iter().find(|e| e["method"] == "SetWallpaper" && e["outcome"] == "refused:Excluded").expect("the refusal");
    assert_eq!(wall["reason"], "private-file");
    assert!(entries.iter().any(|e| e["method"] == "NotificationAction" && e["action"] == "button" && e["outcome"] == "allowed"));
    let all = serde_json::to_string(&entries).unwrap();
    for secret in ["Anna wrote", "Lunch", "Archive"] {
        assert!(!all.contains(secret), "{secret} in the log");
    }

    drop(app);
    drop(session);
    let _ = std::fs::remove_dir_all(&schemas);
}
