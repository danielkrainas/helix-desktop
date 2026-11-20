//! `helix-agent a11y` against a whole headless session (helixdesktop/spec
//! `contracts/agent-access/README.md`, "Control apps methods";
//! helixdesktop/desktop#149): a real GTK 4 window on the headless
//! compositor, its process and accessible object passed from the
//! compositor to the shell (window list version 13), and the shell
//! reading and using it on a private accessibility bus. The agent finds a
//! button and presses it, fills in a field and moves a slider, and the
//! window shows it in the snapshot; a password field, a sensitive button
//! without asking and a wrong node are refused; the log holds no text.
//!
//! With `HELIX_AGENT_SNAPSHOTS=dir`, the window is kept there as PNGs
//! before and after.
//!
//! Needs helix-comp and helix-shell built next to this test, dbus-daemon,
//! at-spi2-core's registry and GTK 4 for Python; skipped otherwise.

use std::io::BufRead;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use helix_test_support::a11y::{gtk4_python, write_gtk4_controls, A11yBus};
use helix_test_support::headless::{sibling_binary, HeadlessSession, Options};
use serde_json::Value as Json;

const CLI: &str = env!("CARGO_BIN_EXE_helix-agent");
const APP: &str = "org.example.A11yTest";

fn run(session: &HeadlessSession, args: &[&str]) -> Output {
    let home = session.home();
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
        .env("HELIX_AGENT_NAME", "Form filler")
        .env("DBUS_SESSION_BUS_ADDRESS", session.bus().address())
        .stdin(Stdio::null())
        .output()
        .expect("run helix-agent")
}

fn json(out: &Output) -> Json {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("not JSON ({e}): {} {}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

fn code(out: &Output) -> i32 {
    out.status.code().expect("exited")
}

fn keep(session: &HeadlessSession, name: &str) {
    if let Some(dir) = std::env::var_os("HELIX_AGENT_SNAPSHOTS") {
        let _ = std::fs::create_dir_all(&dir);
        session.keep_snapshot(&Path::new(&dir).join(name));
    }
}

/// Wait until the app said `line`.
fn said(out: &Arc<Mutex<Vec<String>>>, line: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if out.lock().unwrap().iter().any(|l| l == line) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// The id of the first node `find` gives for `args`.
fn find(session: &HeadlessSession, window: &str, args: &[&str]) -> String {
    let mut all = vec!["--json", "a11y", "find", window];
    all.extend_from_slice(args);
    let out = run(session, &all);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stdout));
    json(&out)["nodes"][0]["id"].as_str().unwrap_or_else(|| panic!("nothing found for {args:?}: {}", json(&out))).to_owned()
}

/// Dark pixels (text) in the window's part of the snapshot, as a cheap
/// sign that what it shows changed.
fn dark_pixels((w, h, data): &(u32, u32, Vec<u8>), area: (u32, u32, u32, u32)) -> usize {
    let (x0, y0, aw, ah) = area;
    (y0..(y0 + ah).min(*h)).flat_map(|y| (x0..(x0 + aw).min(*w)).map(move |x| (x, y))).filter(|(x, y)| data[((y * w + x) * 4) as usize] < 80).count()
}

#[test]
fn an_app_driven_through_its_controls() {
    let (Some(comp), Some(shell)) = (sibling_binary("helix-comp"), sibling_binary("helix-shell")) else {
        eprintln!("helix-comp or helix-shell not built; skipping");
        return;
    };
    if !gtk4_python() {
        eprintln!("GTK 4 for Python is not installed; skipping");
        return;
    }
    let schemas = std::env::temp_dir().join(format!("helix-agent-a11y-schemas-{}", std::process::id()));
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
    let Some(a11y) = A11yBus::start() else { return };
    let me = std::env::current_exe().unwrap().to_string_lossy().into_owned();
    // This test runs the CLI, so it is the agent; the user allowed it to
    // control apps always.
    let grants = format!("version = 1\n\n[[agent]]\nid = \"{me}\"\nexecutable = \"{me}\"\nname = \"Form filler\"\nlevel = \"apps\"\ngranted = 1\n");
    let settings = "[org/helixdesktop/agent-access]\nenabled=true\nallow-remote=true\naccess-level='apps'\nremember-mode='remember'\n";
    let Some(session) = HeadlessSession::start_prepared(
        Options {
            comp: &comp,
            shell: Some(&shell),
            size: (800, 600),
            settings,
            env: vec![
                ("GSETTINGS_SCHEMA_DIR".into(), schemas.to_string_lossy().into_owned()),
                ("AT_SPI_BUS_ADDRESS".into(), a11y.address().to_owned()),
                ("RUST_LOG".into(), "info,helix_shell::agent::a11y=debug".into()),
            ],
        },
        |home| {
            std::fs::create_dir_all(home.join(".config/helix")).unwrap();
            std::fs::write(home.join(".config/helix/agent-grants.toml"), &grants).unwrap();
        },
    ) else {
        return;
    };
    assert!(session.wait_for_log("shell ready", Duration::from_secs(30)), "the shell did not start:\n{}", session.log());

    // The GTK 4 window.
    let program = write_gtk4_controls(session.home());
    let home = session.home();
    let mut app = Command::new("/usr/bin/python3")
        .arg(&program)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home)
        .env("XDG_RUNTIME_DIR", home.join("run"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("WAYLAND_DISPLAY", &session.wayland_display)
        .env("GDK_BACKEND", "wayland")
        .env("GSETTINGS_BACKEND", "memory")
        .env("LANG", "C.UTF-8")
        .env("DBUS_SESSION_BUS_ADDRESS", session.bus().address())
        .env("AT_SPI_BUS_ADDRESS", a11y.address())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("start the GTK 4 window");
    let out: Arc<Mutex<Vec<String>>> = Default::default();
    let lines = out.clone();
    let stdout = app.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
            lines.lock().unwrap().push(line);
        }
    });
    assert!(said(&out, "ready"), "the GTK 4 window did not start");

    // Its window, as agents see it.
    let waited = run(&session, &["--json", "wait", "window", "--app", APP, "--timeout", "15"]);
    assert_eq!(code(&waited), 0, "{}\n{}", String::from_utf8_lossy(&waited.stdout), session.log());
    let window = json(&waited);
    let id = window["id"].as_u64().unwrap().to_string();
    // The app appears on the accessibility bus a little after its window.
    let deadline = Instant::now() + Duration::from_secs(15);
    let tree = loop {
        let out = run(&session, &["--json", "a11y", "tree", &id]);
        if code(&out) == 0 {
            break json(&out);
        }
        assert!(Instant::now() < deadline, "the tree was not read: {}\n{}", String::from_utf8_lossy(&out.stdout), session.log());
        std::thread::sleep(Duration::from_millis(250));
    };
    assert_eq!(tree["app-id"], APP);
    // GTK 4 told the compositor which object is the window, and the
    // compositor told the shell.
    assert!(session.log().contains("a window's controls, where its app said"), "{}", session.log());
    assert_eq!((tree["root"]["role"].as_str(), tree["root"]["name"].as_str()), (Some("frame"), Some("A11y Test")));
    assert!(tree["root"]["children"].as_array().is_some_and(|c| !c.is_empty()));
    assert!(!tree.to_string().contains("hunter2"), "the password field's text");
    let text = run(&session, &["--text", "a11y", "tree", &id]);
    let outline = String::from_utf8_lossy(&text.stdout).into_owned();
    assert!(outline.lines().any(|l| l.trim_start().starts_with("button “Press me”") && l.contains("(click) #")), "{outline}");
    assert!(outline.contains("password text “Password” (password field)"), "{outline}");
    // Reading text fields is off (the default): the field, not its text.
    assert!(outline.contains("text “Name field” (hidden)"), "{outline}");
    assert!(!outline.contains("hello") && !tree.to_string().contains("hello"), "{outline}");
    // Where it is, now that it is shown.
    let window = json(&run(&session, &["--json", "windows", "--app", APP]))["windows"][0].clone();
    let area = (window["x"].as_u64().unwrap_or(0) as u32, window["y"].as_u64().unwrap_or(0) as u32, window["width"].as_u64().unwrap() as u32, window["height"].as_u64().unwrap() as u32);
    assert!(area.2 > 0 && area.3 > 0, "{window}");
    std::thread::sleep(Duration::from_millis(500));
    let before = session.fresh_snapshot();
    keep(&session, "a11y-before.png");

    // Press the button, fill in the field, move the slider.
    let press = find(&session, &id, &["--role", "button", "--name", "Press me"]);
    for n in 1..=3 {
        let pressed = run(&session, &["a11y", "press", &id, &press]);
        assert_eq!(code(&pressed), 0, "{}", String::from_utf8_lossy(&pressed.stdout));
        // GTK shows a pressed button for a moment, and ignores presses
        // meanwhile.
        assert!(said(&out, &format!("clicked {n}")), "{:?}", out.lock().unwrap());
    }
    let name = find(&session, &id, &["--role", "text", "--name", "Name field"]);
    let typed = run(&session, &["a11y", "set", &id, &name, "Typed by an agent"]);
    assert_eq!(code(&typed), 0, "{}", String::from_utf8_lossy(&typed.stdout));
    assert!(said(&out, "name length 17"));
    let level = find(&session, &id, &["--role", "slider"]);
    assert_eq!(code(&run(&session, &["a11y", "set", &id, &level, "75", "--number"])), 0);
    assert!(said(&out, "level 75"));
    let waited = run(&session, &["--json", "a11y", "wait", &id, "--role", "label", "--name", "Clicks: 3", "--timeout", "5"]);
    assert_eq!((code(&waited), json(&waited)["name"].as_str()), (0, Some("Clicks: 3")));
    std::thread::sleep(Duration::from_millis(1200));
    let after = session.fresh_snapshot();
    keep(&session, "a11y-after.png");
    assert!(dark_pixels(&after, area) > dark_pixels(&before, area) + 200, "the window shows what changed: {window} {} {}", dark_pixels(&before, area), dark_pixels(&after, area));

    // Refused: the password field (exit 14), the sensitive button without
    // asking (exit 18), a node that is not there (exit 22), focus on
    // GTK 4 (exit 23).
    let password = find(&session, &id, &["--role", "password text"]);
    let r = run(&session, &["--json", "a11y", "set", &id, &password, "guess"]);
    assert_eq!((code(&r), json(&r)["error"]["message"].as_str()), (14, Some("Agents can't use password fields")));
    let r = run(&session, &["--text", "a11y", "press", &id, &password]);
    assert_eq!(code(&r), 14);
    assert!(String::from_utf8_lossy(&r.stderr).contains("Agents can't use password fields"));
    let delete = find(&session, &id, &["--role", "button", "--name", "delete"]);
    let r = run(&session, &["--json", "--no-ask", "a11y", "press", &id, &delete]);
    assert_eq!((code(&r), json(&r)["error"]["code"].as_str()), (18, Some("needs-confirmation")));
    assert_eq!(code(&run(&session, &["--json", "a11y", "press", &id, "no_such_node"])), 22);
    // The app's reason, not the session's.
    let r = run(&session, &["--json", "a11y", "focus", &id, &name]);
    assert_eq!((code(&r), json(&r)["error"]["message"].as_str()), (23, Some("Not supported here: the app does not let its focus be moved over accessibility")));
    let r = run(&session, &["--json", "a11y", "wait", &id, "--name", "Never there", "--timeout", "1"]);
    assert_eq!(code(&r), 21);
    std::thread::sleep(Duration::from_millis(300));
    assert!(!out.lock().unwrap().iter().any(|l| l == "deleted" || l.starts_with("password")), "{:?}", out.lock().unwrap());

    // The log: methods, actions and lengths, never the text.
    let log = json(&run(&session, &["--json", "log"]));
    let entries = log["entries"].as_array().unwrap();
    assert!(entries.iter().any(|e| e["method"] == "A11yAction" && e["action"] == "default" && e["outcome"] == "allowed"), "{log}");
    assert!(entries.iter().any(|e| e["method"] == "A11ySetValue" && e["text-length"] == 17), "{log}");
    assert!(entries.iter().any(|e| e["method"] == "A11ySetValue" && e["outcome"] == "refused:Excluded"), "{log}");
    let raw = log.to_string();
    for word in ["Typed by an agent", "guess", "hunter2", "Press me", "hello", &press, &name] {
        assert!(!raw.contains(word), "the log holds {word:?}");
    }

    let _ = app.kill();
    let _ = app.wait();
    drop(session);
    drop(a11y);
    let _ = std::fs::remove_dir_all(&schemas);
}
