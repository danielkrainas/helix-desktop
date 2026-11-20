//! `helix-agent screenshot` and `helix-agent input` against a whole
//! headless session (helixdesktop/spec `contracts/agent-access/README.md`,
//! "Screen and input methods"; helixdesktop/desktop#150): real GTK 4
//! windows on the headless compositor, one of them an excluded app's (all
//! red). The agent's screenshot has the excluded window black where the
//! compositor's own picture has it red, and the agent's window drawn over
//! it; window screenshots of it are refused. The agent clicks a button,
//! types text with characters the keyboard layout has no key for, presses
//! Ctrl+S, and is refused a password field and a protected window that has
//! the focus. The user's own input (the test socket) stops a long text
//! halfway (exit 20), new input is refused for a moment, then works again.
//! The log holds no typed text.
//!
//! With `HELIX_AGENT_SNAPSHOTS=dir`, the pictures are kept there.
//!
//! Needs helix-comp and helix-shell built next to this test, dbus-daemon
//! and GTK 4 for Python; skipped otherwise.

use std::io::BufRead;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use helix_test_support::a11y::{gtk4_python, write_gtk4_input};
use helix_test_support::headless::{sibling_binary, HeadlessSession, Options};
use serde_json::Value as Json;

const CLI: &str = env!("CARGO_BIN_EXE_helix-agent");
const APP: &str = "org.example.InputTest";

fn command(session: &HeadlessSession, args: &[&str]) -> Command {
    let home = session.home();
    let mut cmd = Command::new(CLI);
    cmd.args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("LANG", "C.UTF-8")
        .env("HELIX_AGENT_NAME", "Screen driver")
        .env("DBUS_SESSION_BUS_ADDRESS", session.bus().address())
        .stdin(Stdio::null());
    cmd
}

fn run(session: &HeadlessSession, args: &[&str]) -> Output {
    command(session, args).output().expect("run helix-agent")
}

fn json(out: &Output) -> Json {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("not JSON ({e}): {} {}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

fn code(out: &Output) -> i32 {
    out.status.code().expect("exited")
}

fn ok(out: &Output, session: &HeadlessSession) {
    assert_eq!(code(out), 0, "{} {}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr), session.log());
}

fn keep(path: &Path, name: &str) {
    if let Some(dir) = std::env::var_os("HELIX_AGENT_SNAPSHOTS") {
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::copy(path, Path::new(&dir).join(name));
    }
}

/// A GTK 4 window of the test program; its output lines.
fn window(session: &HeadlessSession, program: &Path, vault: bool) -> (Child, Arc<Mutex<Vec<String>>>) {
    let home = session.home();
    let mut cmd = Command::new("/usr/bin/python3");
    cmd.arg(program);
    if vault {
        cmd.arg("--vault");
    }
    let mut child = cmd
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
        .env("GTK_A11Y", "none")
        .env("LANG", "C.UTF-8")
        .env("DBUS_SESSION_BUS_ADDRESS", session.bus().address())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("start a GTK 4 window");
    let out: Arc<Mutex<Vec<String>>> = Default::default();
    let lines = out.clone();
    let stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
            lines.lock().unwrap().push(line);
        }
    });
    assert!(said(&out, |l| l == "ready"), "the GTK 4 window did not start");
    (child, out)
}

/// Wait until the app said a line `matches`.
fn said(out: &Arc<Mutex<Vec<String>>>, matches: impl Fn(&str) -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if out.lock().unwrap().iter().any(|l| matches(l)) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// The last "text …" line the app said.
fn last_text(out: &Arc<Mutex<Vec<String>>>) -> String {
    out.lock().unwrap().iter().rev().find_map(|l| l.strip_prefix("text ").map(str::to_owned)).unwrap_or_default()
}

fn read_png(path: &Path) -> (u32, u32, Vec<u8>) {
    let decoder = png::Decoder::new(std::io::Cursor::new(std::fs::read(path).unwrap()));
    let mut reader = decoder.read_info().unwrap();
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).unwrap();
    buf.truncate(info.buffer_size());
    assert_eq!(info.color_type, png::ColorType::Rgba);
    (info.width, info.height, buf)
}

fn pixel((w, _, data): &(u32, u32, Vec<u8>), x: u32, y: u32) -> [u8; 4] {
    let i = ((y * w + x) * 4) as usize;
    data[i..i + 4].try_into().unwrap()
}

fn red(p: [u8; 4]) -> bool {
    p[0] > 200 && p[1] < 60 && p[2] < 60
}

#[test]
fn screenshots_and_input() {
    let (Some(comp), Some(shell)) = (sibling_binary("helix-comp"), sibling_binary("helix-shell")) else {
        eprintln!("helix-comp or helix-shell not built; skipping");
        return;
    };
    if !gtk4_python() {
        eprintln!("GTK 4 for Python is not installed; skipping");
        return;
    }
    let schemas = std::env::temp_dir().join(format!("helix-agent-screen-schemas-{}", std::process::id()));
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
    let me = std::env::current_exe().unwrap().to_string_lossy().into_owned();
    // This test runs the CLI, so it is the agent; the user allowed it the
    // screen and input always.
    let grants = format!("version = 1\n\n[[agent]]\nid = \"{me}\"\nexecutable = \"{me}\"\nname = \"Screen driver\"\nlevel = \"screen-and-input\"\ngranted = 1\n");
    let settings = "[org/helixdesktop/agent-access]\nenabled=true\nallow-remote=true\naccess-level='screen-and-input'\nremember-mode='remember'\nexcluded-apps=['org.example.Vault']\n";
    let Some(mut session) = HeadlessSession::start_prepared(
        Options {
            comp: &comp,
            shell: Some(&shell),
            size: (1000, 700),
            settings,
            env: vec![("GSETTINGS_SCHEMA_DIR".into(), schemas.to_string_lossy().into_owned()), ("RUST_LOG".into(), "info".into())],
        },
        |home| {
            std::fs::create_dir_all(home.join(".config/helix")).unwrap();
            std::fs::write(home.join(".config/helix/agent-grants.toml"), &grants).unwrap();
        },
    ) else {
        return;
    };
    assert!(session.wait_for_log("shell ready", Duration::from_secs(30)), "the shell did not start:\n{}", session.log());
    let home = session.home().to_owned();
    let program = write_gtk4_input(&home);

    // The excluded app's red window, then the agent's window over it.
    let (mut vault, _) = window(&session, &program, true);
    let (mut app, out) = window(&session, &program, false);
    let waited = run(&session, &["--json", "wait", "window", "--app", APP, "--focused", "--timeout", "15"]);
    ok(&waited, &session);
    let id = json(&waited)["id"].as_u64().unwrap();
    let id_text = id.to_string();
    // The vault is not listed.
    let listed = json(&run(&session, &["--json", "windows"]));
    assert!(!listed.to_string().contains("org.example.Vault"), "{listed}");
    std::thread::sleep(Duration::from_millis(800));
    let (x, y) = {
        let w = &json(&run(&session, &["--json", "windows", "--app", APP]))["windows"][0];
        (w["x"].as_u64().unwrap() as u32, w["y"].as_u64().unwrap() as u32)
    };

    // The compositor's own picture: the vault is red where it shows.
    let plain = session.fresh_snapshot();
    keep(&session.snapshot_path(), "screen-compositor.png");
    let red_pixels: Vec<(u32, u32)> = (0..plain.1).flat_map(|py| (0..plain.0).map(move |px| (px, py))).filter(|&(px, py)| red(pixel(&plain, px, py))).collect();
    assert!(red_pixels.len() > 50_000, "the vault is shown: {} red pixels", red_pixels.len());

    // The agent's: black there, the agent's window drawn over it.
    let shot = home.join("agent-screen.png");
    let r = run(&session, &["--json", "screenshot", "--output", shot.to_str().unwrap()]);
    ok(&r, &session);
    let record = json(&r);
    assert!(record["filled"].as_u64().unwrap() >= 1, "{record}");
    assert_eq!((record["width"].as_u64(), record["height"].as_u64()), (Some(1000), Some(700)));
    assert!(record.get("png").is_none() && record["path"].is_string(), "{record}");
    keep(&shot, "screen-agent.png");
    let agent = read_png(&shot);
    assert_eq!((agent.0, agent.1), (1000, 700));
    let not_black: Vec<_> = red_pixels.iter().filter(|&&(px, py)| pixel(&agent, px, py) != [0, 0, 0, 255]).collect();
    assert!(not_black.is_empty(), "{} of the vault's pixels are not black, e.g. {:?}", not_black.len(), &not_black[..not_black.len().min(5)]);
    assert!(!(0..agent.1).any(|py| (0..agent.0).any(|px| red(pixel(&agent, px, py)))), "no red anywhere");
    let inside = pixel(&agent, x + 200, y + 280);
    assert_ne!(inside, [0, 0, 0, 255], "the agent's window is drawn over the vault");
    // The shell's top bar is shown.
    assert_ne!(pixel(&agent, 500, 5), [0, 0, 0, 255]);
    // As base64 in JSON.
    let r = run(&session, &["--json", "screenshot", "--area", "0,0,100,50"]);
    ok(&r, &session);
    assert!(json(&r)["png"].as_str().is_some_and(|b| b.starts_with("iVBORw0KGgo")), "base64 PNG");
    assert_eq!(json(&r)["width"], 100);
    // Text output to a terminal-less pipe: the PNG itself.
    let r = run(&session, &["--text", "screenshot", "--monitor", "HEADLESS-1", "--area", "0,0,10,10"]);
    ok(&r, &session);
    assert!(r.stdout.starts_with(b"\x89PNG"));

    // The agent's window alone; the vault's refused outright.
    let wshot = home.join("agent-window.png");
    ok(&run(&session, &["--json", "screenshot", "--window", &id_text, "--output", wshot.to_str().unwrap()]), &session);
    keep(&wshot, "window-agent.png");
    let (ww, wh, _) = read_png(&wshot);
    assert_eq!((ww, wh), (400, 300));
    let vault_id = (1..id).rev().find(|v| {
        let r = run(&session, &["--json", "screenshot", "--window", &v.to_string()]);
        code(&r) == 14
    });
    let vault_id = vault_id.expect("the vault's window id is refused");
    let r = run(&session, &["--json", "screenshot", "--window", &vault_id.to_string()]);
    assert_eq!((code(&r), json(&r)["error"]["message"].as_str()), (14, Some("org.example.Vault is excluded from agent access")));

    // Click the button, type into the field (with characters a US layout
    // has no key for), save with Ctrl+S.
    ok(&run(&session, &["input", "click", "200", "50"]), &session);
    assert!(said(&out, |l| l == "clicked 1"), "{:?}\n{}", out.lock().unwrap(), session.log());
    ok(&run(&session, &["input", "click", "--count", "2", "200", "50"]), &session);
    assert!(said(&out, |l| l == "clicked 3"), "{:?}", out.lock().unwrap());
    ok(&run(&session, &["input", "click", "200", "126"]), &session);
    let typed = "Héllo wörld € ✓";
    ok(&run(&session, &["input", "type", typed]), &session);
    assert!(said(&out, |l| l == format!("text {typed}")), "{:?}\n{}", out.lock().unwrap(), session.log());
    ok(&run(&session, &["input", "key", "ctrl+s"]), &session);
    assert!(said(&out, |l| l == "saved"), "{:?}", out.lock().unwrap());
    ok(&run(&session, &["input", "key", "BackSpace", "End"]), &session);
    assert!(said(&out, |l| l == format!("text {}", &typed[..typed.len() - "✓".len()])), "{:?}", out.lock().unwrap());
    ok(&run(&session, &["input", "sequence", "key-down", "shift", "type", "x", "key-up", "shift", "key", "a"]), &session);
    assert!(said(&out, |l| l.ends_with("€ xa")), "{:?}", out.lock().unwrap());
    std::thread::sleep(Duration::from_millis(1200));
    keep(&session.snapshot_path(), "after-typing.png");

    // The password field: refused, nothing typed into it.
    ok(&run(&session, &["input", "click", "200", "172"]), &session);
    std::thread::sleep(Duration::from_millis(300));
    let r = run(&session, &["--json", "input", "type", "hunter2"]);
    assert_eq!((code(&r), json(&r)["error"]["message"].as_str()), (14, Some("Agents can't use password fields")), "{}", session.log());
    assert!(!out.lock().unwrap().iter().any(|l| l == "password changed"));

    // The user clicks the vault: it has the focus, and no input goes to it.
    let (vx, vy) = *red_pixels.last().unwrap();
    session.input(&format!("click {vx} {vy}"));
    std::thread::sleep(Duration::from_millis(300));
    let r = run(&session, &["--json", "input", "type", "nope"]);
    assert_eq!((code(&r), json(&r)["error"]["message"].as_str()), (14, Some("org.example.Vault is excluded from agent access")));
    // Named, the agent's window is focused first.
    ok(&run(&session, &["input", "click", "--window", &id_text, "200", "126"]), &session);
    ok(&run(&session, &["input", "type", "!"]), &session);
    assert!(said(&out, |l| l.starts_with("text") && l.ends_with('!')), "{:?}", out.lock().unwrap());
    let before = last_text(&out);

    // The hard stop: the user moves the pointer while a long text is typed.
    let long: String = "abcdefghij".repeat(60);
    let typing = command(&session, &["--json", "input", "type", &long]).stdout(Stdio::piped()).spawn().unwrap();
    assert!(said(&out, |l| l.starts_with("text") && l.len() > before.len() + 30), "typing started");
    session.input("move 5 300");
    let stopped = typing.wait_with_output().unwrap();
    assert_eq!(
        (code(&stopped), json(&stopped)["error"]["code"].as_str(), json(&stopped)["error"]["message"].as_str()),
        (20, Some("no"), Some("Stopped: the user took over the keyboard or pointer")),
        "{}",
        session.log()
    );
    std::thread::sleep(Duration::from_millis(200));
    let partial = last_text(&out);
    assert!(partial.len() < before.len() + long.len(), "stopped halfway: {} of {}", partial.len() - before.len(), long.len());
    // For a moment, no more input; then again.
    let r = run(&session, &["--json", "input", "type", "z"]);
    assert_eq!((code(&r), json(&r)["error"]["message"].as_str()), (20, Some("The user is using the keyboard or pointer; try again in a moment")));
    std::thread::sleep(Duration::from_millis(2300));
    ok(&run(&session, &["input", "type", "z"]), &session);
    assert!(said(&out, |l| l.starts_with("text") && l.ends_with('z')), "{:?}", out.lock().unwrap().last());
    std::thread::sleep(Duration::from_millis(1200));
    keep(&session.snapshot_path(), "after-stop.png");

    // The log: scopes, sizes and counts, never text or keys.
    let log = json(&run(&session, &["--json", "log"]));
    let entries = log["entries"].as_array().unwrap();
    assert!(entries.iter().any(|e| e["method"] == "Screenshot" && e["scope"] == "screen" && e["width"] == 1000 && e["filled-count"].as_u64().unwrap_or(0) >= 1), "{log}");
    assert!(entries.iter().any(|e| e["method"] == "Screenshot" && e["outcome"] == "refused:Excluded"), "{log}");
    assert!(entries.iter().any(|e| e["method"] == "Input" && e["text-length"] == typed.chars().count() && e["outcome"] == "allowed"), "{log}");
    assert!(entries.iter().any(|e| e["method"] == "Input" && e["shortcut-count"] == 1), "{log}");
    assert!(entries.iter().any(|e| e["method"] == "Input" && e["outcome"] == "cancelled" && e["text-length"] == 600), "{log}");
    assert!(entries.iter().any(|e| e["method"] == "Input" && e["outcome"] == "refused:Excluded" && e["target"].as_str().is_some_and(|t| t.contains("org.example.Vault"))), "{log}");
    let raw = log.to_string();
    for word in ["Héllo", "wörld", "hunter2", "nope", "abcdefghij", "ctrl"] {
        assert!(!raw.contains(word), "the log holds {word:?}");
    }

    let _ = app.kill();
    let _ = app.wait();
    let _ = vault.kill();
    let _ = vault.wait();
    let _ = std::fs::remove_dir_all(&schemas);
}
