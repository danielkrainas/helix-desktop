//! Agent Access's clipboard methods over a private bus (helixdesktop/spec
//! `contracts/agent-access/README.md`, "Clipboard methods";
//! helixdesktop/desktop#151), against a stand-in UI whose clipboard acts
//! as the compositor's does (a generation, a source, pipes): reading text,
//! HTML, a picture and the types offered; setting text and a picture; and
//! every refusal: access off, the switch, the level, locked, a system
//! prompt, content from the shell, Settings, an excluded app (by its id,
//! another window of its process, its program), a system prompt's program,
//! a password manager's hint (also one that never answers), the clipboard
//! changing while it is read, too much data, types agents may not set,
//! and the sensitive-action prompt. The activity log has only kinds and
//! lengths. Nothing on the host is touched: scratch XDG directories,
//! in-memory settings, a private bus.

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gio::prelude::*;
use helix_shell::agent::clipboard::{Offer, Request, SourceKind};
use helix_shell::agent::desktop::{Snapshot, WindowInfo};
use helix_shell::agent::grants::Answer;
use helix_shell::agent::{self, AgentAccess, Agent1, Confirm, Desktop, Setup, OBJECT_PATH};
use helix_shell::screensaver::SharedStatus;
use helix_shell::UiEvent;
use helix_test_support::conformance::{assert_conformant, check, introspect, load_dbus_contract};
use helix_test_support::TestBus;
use zbus::zvariant::{OwnedValue, Value};
use zbus::Connection;

const IFACE: &str = "org.helixdesktop.Agent1";
const TEXT: &str = "text/plain;charset=utf-8";
const HINT: &str = "x-kde-passwordManagerHint";
const PNG: &[u8] = b"\x89PNG\r\n\x1a\nnot really a picture";

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

    fn queue(&self, answer: Option<bool>) {
        self.answers.lock().unwrap().push_back(answer);
    }

    fn asked(&self) -> usize {
        self.asked.lock().unwrap().len()
    }
}

/// How the stand-in's clipboard owner answers a read.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Serve {
    /// Writes the content and closes.
    Normally,
    /// Never writes nor closes (an app that hangs).
    Never,
    /// The clipboard changes before the read is served (nothing sent).
    Changes,
}

/// The compositor's clipboard, as the stand-in UI keeps it.
struct Clip {
    generation: u32,
    source: SourceKind,
    app_id: String,
    pid: u32,
    /// Types and their content, in the order offered.
    content: Vec<(String, Vec<u8>)>,
    serve: Serve,
    /// Pipes left open by `Serve::Never`.
    hanging: Vec<std::io::PipeWriter>,
}

impl Clip {
    fn set(&mut self, source: SourceKind, app_id: &str, pid: u32, content: &[(&str, &[u8])]) {
        self.generation += 1;
        self.source = source;
        self.app_id = app_id.into();
        self.pid = pid;
        self.content = content.iter().map(|(t, d)| ((*t).to_owned(), d.to_vec())).collect();
        self.serve = Serve::Normally;
    }

    fn offer(&self) -> Offer {
        Offer { types: self.content.iter().map(|(t, _)| t.clone()).collect(), generation: self.generation, source: self.source, app_id: self.app_id.clone(), pid: self.pid }
    }

    fn read(&mut self, generation: u32, mime: &str) -> OwnedFd {
        let (r, mut w) = std::io::pipe().unwrap();
        if self.serve == Serve::Changes {
            self.set(SourceKind::App, "org.gnome.TextEditor", 0, &[(TEXT, b"newer")]);
        }
        let data = self.content.iter().find(|(t, _)| t == mime).map(|(_, d)| d.clone());
        match (generation == self.generation, data) {
            (true, Some(_)) if self.serve == Serve::Never && mime == HINT => self.hanging.push(w),
            (true, Some(data)) => {
                std::thread::spawn(move || {
                    let _ = w.write_all(&data);
                });
            }
            _ => drop(w),
        }
        r.into()
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

fn refusal(result: &zbus::Result<zbus::Message>) -> (String, String) {
    (short(result), message(result))
}

async fn call(conn: &Connection, method: &str, body: &(impl serde::Serialize + zbus::zvariant::DynamicType)) -> zbus::Result<zbus::Message> {
    conn.call_method(Some("org.gnome.Shell"), OBJECT_PATH, Some(IFACE), method, body).await
}

type Opts = HashMap<&'static str, Value<'static>>;

fn opts(pairs: &[(&'static str, Value<'static>)]) -> Opts {
    pairs.iter().cloned().collect()
}

async fn get(conn: &Connection, o: Opts) -> zbus::Result<zbus::Message> {
    call(conn, "GetClipboard", &(o,)).await
}

async fn set(conn: &Connection, mime: &str, data: &[u8], o: Opts) -> zbus::Result<zbus::Message> {
    call(conn, "SetClipboard", &(mime, data.to_vec(), o)).await
}

type Record = HashMap<String, OwnedValue>;

fn record(r: zbus::Result<zbus::Message>) -> Record {
    r.unwrap_or_else(|e| panic!("{e:?}")).body().deserialize().unwrap()
}

fn text(r: &Record, key: &str) -> Option<String> {
    r.get(key).and_then(|v| String::try_from(v.clone()).ok())
}

fn win(id: u32, app: &str, pid: u32) -> WindowInfo {
    WindowInfo { id, app_id: app.into(), title: format!("{app} window"), geometry: (0, 0, 640, 480), pid, ..Default::default() }
}

fn activity(dir: &Path) -> Vec<serde_json::Value> {
    let mut days: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "jsonl")).collect();
    days.sort();
    days.iter().flat_map(|d| std::fs::read_to_string(d).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect::<Vec<_>>()).collect()
}

/// A process whose program is called `name`: a copy of Python that
/// waits (coreutils' programs are one program that goes by its name).
fn program(bin: &Path, name: &str) -> Option<std::process::Child> {
    let python = std::fs::canonicalize("/usr/bin/python3").ok()?;
    let path = bin.join(name);
    std::fs::copy(python, &path).ok()?;
    std::process::Command::new(&path).args(["-c", "import time; time.sleep(60)"]).env("PYTHONHOME", "/usr").spawn().ok()
}

#[tokio::test(flavor = "multi_thread")]
async fn clipboard_over_the_bus() {
    let Some(bus) = TestBus::start() else { return };
    let tmp = std::env::temp_dir().join(format!("helix-agent-clipboard-{}", std::process::id()));
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
    // Processes that set the clipboard: a password prompt's program, an
    // app with another window that is excluded.
    let (Some(mut pinentry), Some(mut two_windows)) = (program(&bin, "pinentry-gnome3"), program(&bin, "two-windows")) else {
        eprintln!("python3 is not installed; skipping");
        return;
    };
    // Its program runs.
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::fs::read_link(format!("/proc/{}/exe", pinentry.id())).map_or(true, |e| !e.ends_with("pinentry-gnome3")) {
        assert!(Instant::now() < deadline, "the stand-in prompt program did not start");
        std::thread::sleep(Duration::from_millis(20));
    }

    // The UI: snapshots, and the clipboard as the compositor keeps it.
    let snapshot = Arc::new(Mutex::new(Snapshot {
        windows: vec![win(11, "org.gnome.TextEditor", 0), win(12, "org.example.Notes", two_windows.id()), win(13, "org.example.Vault", two_windows.id())],
        focused: 11,
        workspaces: 1,
        ..Default::default()
    }));
    let clip = Arc::new(Mutex::new(Clip { generation: 0, source: SourceKind::None, app_id: String::new(), pid: 0, content: Vec::new(), serve: Serve::Normally, hanging: Vec::new() }));
    let (ui, mut ui_rx) = tokio::sync::mpsc::unbounded_channel::<UiEvent>();
    let (shown, board) = (snapshot.clone(), clip.clone());
    tokio::spawn(async move {
        while let Some(event) = ui_rx.recv().await {
            match event {
                UiEvent::AgentSnapshot(slot) => {
                    if let Some(reply) = slot.take() {
                        let _ = reply.send(shown.lock().unwrap().clone());
                    }
                }
                UiEvent::AgentClipboard(slot) => match slot.take() {
                    Some(Request::Peek(reply)) => {
                        let _ = reply.send(Ok(board.lock().unwrap().offer()));
                    }
                    Some(Request::Read { generation, mime, reply }) => {
                        let _ = reply.send(Ok(board.lock().unwrap().read(generation, &mime)));
                    }
                    Some(Request::Set { mime, data, reply }) => {
                        board.lock().unwrap().set(SourceKind::Agent, "", 0, &[(mime.as_str(), &data)]);
                        let _ = reply.send(Ok(()));
                    }
                    None => {}
                },
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
    let server = bus.builder().serve_at(OBJECT_PATH, Agent1 { access: access.clone() }).unwrap().name("org.gnome.Shell").unwrap().build().await.unwrap();
    access.attach(&server);
    let conn = bus.connect().await;

    // The clipboard methods are there, as the contract has them.
    let actual = introspect(&conn, "org.gnome.Shell", OBJECT_PATH).await;
    assert_conformant(IFACE, &check(IFACE, &load_dbus_contract("../agent-access/org.helixdesktop.Agent1.xml"), &actual, &[]));

    let quiet = || opts(&[("interactive", Value::from(false))]);
    clip.lock().unwrap().set(SourceKind::App, "org.gnome.TextEditor", 0, &[(TEXT, "Dear diary".as_bytes()), ("text/html", b"<p>Dear diary</p>"), ("image/png", PNG)]);
    // Off: both refused.
    assert_eq!(short(&get(&conn, quiet()).await), "AccessOff");
    assert_eq!(short(&set(&conn, "", b"x", quiet()).await), "AccessOff");
    // On, Read only: the switch is off for reading; setting needs more.
    settings.set_boolean("enabled", true).unwrap();
    settings.set_boolean("allow-remote", true).unwrap();
    assert_eq!(refusal(&get(&conn, quiet()).await), ("FeatureOff".into(), "clipboard".into()));
    assert_eq!(refusal(&set(&conn, "", b"x", quiet()).await), ("LevelTooLow".into(), "desktop".into()));
    settings.set_string("access-level", "desktop").unwrap();
    assert_eq!(refusal(&set(&conn, "", b"x", quiet()).await), ("FeatureOff".into(), "clipboard".into()), "the switch for setting too");
    settings.set_boolean("clipboard", true).unwrap();

    // Reading: plain text by default, a type asked for, a kind of type,
    // and what it offers.
    let r = record(get(&conn, opts(&[])).await);
    assert_eq!((text(&r, "mime-type").unwrap(), text(&r, "text").unwrap()), (TEXT.to_owned(), "Dear diary".to_owned()));
    assert_eq!(<Vec<String>>::try_from(r["types"].clone()).unwrap(), [TEXT, "text/html", "image/png"]);
    let r = record(get(&conn, opts(&[("type", Value::from("text/html"))])).await);
    assert_eq!(text(&r, "text").unwrap(), "<p>Dear diary</p>");
    let r = record(get(&conn, opts(&[("type", Value::from("image/*"))])).await);
    assert_eq!((text(&r, "mime-type").unwrap(), <Vec<u8>>::try_from(r["data"].clone()).unwrap()), ("image/png".to_owned(), PNG.to_vec()));
    assert!(!r.contains_key("text"));
    let r = record(get(&conn, opts(&[("types-only", Value::from(true))])).await);
    assert_eq!(r.keys().collect::<Vec<_>>(), ["types"], "only the types");
    assert_eq!(short(&get(&conn, opts(&[("type", Value::from("application/pdf"))])).await), "NotFound");
    assert_eq!(short(&get(&conn, opts(&[("type", Value::from(7u32))])).await), "InvalidArgs");

    // Setting: text (the default type), HTML, a picture; read back.
    set(&conn, "", "From the agent".as_bytes(), opts(&[])).await.unwrap();
    assert_eq!(clip.lock().unwrap().source, SourceKind::Agent, "the compositor owns it for the agent");
    let r = record(get(&conn, opts(&[])).await);
    assert_eq!(text(&r, "text").unwrap(), "From the agent", "an agent's copy may be read");
    set(&conn, "text/html", b"<b>bold</b>", opts(&[])).await.unwrap();
    assert_eq!(clip.lock().unwrap().content, [("text/html".to_owned(), b"<b>bold</b>".to_vec())]);
    set(&conn, "image/png", PNG, opts(&[])).await.unwrap();
    assert_eq!(clip.lock().unwrap().content[0].0, "image/png");
    // Not set: other types, a picture that is not PNG, nothing, not UTF-8.
    assert_eq!(short(&set(&conn, "x-special/gnome-copied-files", b"copy\nfile:///etc/passwd", opts(&[])).await), "NotSupported");
    assert_eq!(short(&set(&conn, HINT, b"secret", opts(&[])).await), "NotSupported");
    assert_eq!(short(&set(&conn, "image/png", b"GIF89a", opts(&[])).await), "InvalidArgs");
    assert_eq!(short(&set(&conn, "", b"", opts(&[])).await), "InvalidArgs");
    assert_eq!(short(&set(&conn, "text/plain", &[0xFF, 0xFE], opts(&[])).await), "InvalidArgs");
    assert_eq!(short(&set(&conn, "", &vec![b'a'; (16 << 20) + 1], opts(&[])).await), "InvalidArgs");
    // Marked sensitive: the user is asked (yes, no, or not at all).
    let asked = confirm.asked();
    confirm.queue(Some(true));
    set(&conn, "", b"confirmed", opts(&[("sensitive", Value::from(true))])).await.unwrap();
    confirm.queue(Some(false));
    assert_eq!(short(&set(&conn, "", b"declined", opts(&[("sensitive", Value::from(true))])).await), "DeniedByUser");
    assert_eq!(short(&set(&conn, "", b"quiet", opts(&[("sensitive", Value::from(true)), ("interactive", Value::from(false))])).await), "NeedsConfirmation");
    assert_eq!(confirm.asked(), asked + 2);
    assert_eq!(clip.lock().unwrap().content[0].1, b"confirmed");
    // Not sensitive unless marked.
    set(&conn, "", b"plain", opts(&[])).await.unwrap();
    assert_eq!(confirm.asked(), asked + 2);

    // Where the content came from: refused when it is protected.
    let me = std::process::id();
    settings.set_strv("excluded-apps", ["org.example.Vault", "org.keepassxc.KeePassXC"]).unwrap();
    for (source, app, pid, token) in [
        (SourceKind::App, "org.example.Vault", 0, "app:org.example.Vault"),
        (SourceKind::X11, "org.keepassxc.KeePassXC", 0, "app:org.keepassxc.KeePassXC"),
        (SourceKind::App, "org.helixdesktop.Settings", 0, "settings"),
        (SourceKind::Shell, "", 0, "shell"),
        (SourceKind::App, "org.example.Pretender", me, "shell"),
        (SourceKind::App, "org.example.Pin", pinentry.id(), "system-prompt"),
        (SourceKind::App, "org.example.Notes", two_windows.id(), "app:org.example.Vault"),
    ] {
        clip.lock().unwrap().set(source, app, pid, &[(TEXT, b"hunter2")]);
        assert_eq!(refusal(&get(&conn, opts(&[])).await), ("Excluded".to_owned(), token.to_owned()), "{app} ({source:?})");
        assert_eq!(refusal(&get(&conn, opts(&[("types-only", Value::from(true))])).await), ("Excluded".to_owned(), token.to_owned()), "{app}: not even its types");
    }
    // Unknown apps, remote desktop clients: allowed.
    for source in [SourceKind::App, SourceKind::X11, SourceKind::Remote] {
        clip.lock().unwrap().set(source, "", 0, &[(TEXT, b"fine")]);
        assert_eq!(text(&record(get(&conn, opts(&[])).await), "text").unwrap(), "fine", "{source:?}");
    }
    // A password manager's hint: "secret" is refused, anything else not;
    // a hint that never comes counts as secret.
    clip.lock().unwrap().set(SourceKind::App, "org.example.Passwords", 0, &[(TEXT, b"hunter2"), (HINT, b"secret")]);
    assert_eq!(refusal(&get(&conn, opts(&[])).await), ("Excluded".to_owned(), "password-field".to_owned()));
    clip.lock().unwrap().set(SourceKind::App, "org.example.Passwords", 0, &[(TEXT, b"user name"), (HINT, b"public")]);
    assert_eq!(text(&record(get(&conn, opts(&[])).await), "text").unwrap(), "user name");
    clip.lock().unwrap().set(SourceKind::App, "org.example.Passwords", 0, &[(TEXT, b"hunter2"), (HINT, b"secret")]);
    clip.lock().unwrap().serve = Serve::Never;
    let started = Instant::now();
    assert_eq!(refusal(&get(&conn, opts(&[])).await), ("Excluded".to_owned(), "password-field".to_owned()));
    assert!(started.elapsed() < Duration::from_secs(4), "the hint is waited for briefly");
    // The clipboard changes before the read: never the newer content.
    clip.lock().unwrap().set(SourceKind::App, "org.gnome.TextEditor", 0, &[(TEXT, b"older")]);
    clip.lock().unwrap().serve = Serve::Changes;
    let r = get(&conn, opts(&[])).await;
    assert_eq!(short(&r), "Failed");
    assert!(message(&r).contains("changed"), "{}", message(&r));
    // Too much.
    let big = vec![b'a'; (16 << 20) + 1];
    clip.lock().unwrap().set(SourceKind::App, "org.gnome.TextEditor", 0, &[(TEXT, &big)]);
    let r = get(&conn, opts(&[])).await;
    assert_eq!(short(&r), "Failed");
    assert!(message(&r).contains("16 MiB"), "{}", message(&r));
    // Empty.
    clip.lock().unwrap().set(SourceKind::None, "", 0, &[]);
    assert_eq!(short(&get(&conn, opts(&[])).await), "NotFound");
    let r = record(get(&conn, opts(&[("types-only", Value::from(true))])).await);
    assert!(<Vec<String>>::try_from(r["types"].clone()).unwrap().is_empty());

    // A system prompt shown: neither.
    clip.lock().unwrap().set(SourceKind::App, "org.gnome.TextEditor", 0, &[(TEXT, b"Dear diary")]);
    snapshot.lock().unwrap().system_prompt = true;
    assert_eq!(refusal(&get(&conn, opts(&[])).await), ("Excluded".to_owned(), "system-prompt".to_owned()));
    assert_eq!(refusal(&set(&conn, "", b"x", opts(&[])).await), ("Excluded".to_owned(), "system-prompt".to_owned()));
    snapshot.lock().unwrap().system_prompt = false;
    // Locked: neither.
    lock.lock().unwrap().locked_since = Some(Instant::now());
    assert_eq!(short(&get(&conn, opts(&[])).await), "Locked");
    assert_eq!(short(&set(&conn, "", b"x", opts(&[])).await), "Locked");
    lock.lock().unwrap().locked_since = None;
    // The switch off again: refused at once.
    settings.set_boolean("clipboard", false).unwrap();
    assert_eq!(short(&get(&conn, opts(&[])).await), "FeatureOff");
    assert_eq!(short(&set(&conn, "", b"x", opts(&[])).await), "FeatureOff");
    let _ = pinentry.kill();
    let _ = two_windows.kill();
    let _ = pinentry.wait();
    let _ = two_windows.wait();

    // The activity log: kinds and lengths, the app the content came from;
    // never the content.
    let lines = activity(&log);
    let has = |method: &str, outcome: &str, extra: &dyn Fn(&serde_json::Value) -> bool| lines.iter().any(|l| l["method"] == method && l["outcome"] == outcome && extra(l));
    assert!(has("GetClipboard", "refused:FeatureOff", &|l| l["level"] == "read-only"), "{lines:#?}");
    assert!(has("GetClipboard", "allowed", &|l| l["target"] == "org.gnome.TextEditor" && l["mime-type"] == "text/plain" && l["data-length"] == 10), "{lines:#?}");
    assert!(has("GetClipboard", "allowed", &|l| l["mime-type"] == "image/png" && l["data-length"] == PNG.len()), "{lines:#?}");
    assert!(has("GetClipboard", "refused:Excluded", &|l| l["target"] == "org.example.Vault"), "{lines:#?}");
    assert!(has("GetClipboard", "refused:Excluded", &|l| l["target"] == "org.example.Passwords"), "{lines:#?}");
    assert!(has("GetClipboard", "allowed", &|l| l["error"] == "Failed"), "{lines:#?}");
    assert!(has("SetClipboard", "allowed", &|l| l["level"] == "desktop" && l["mime-type"] == "text/plain" && l["data-length"] == 14 && l.get("error").is_none()), "{lines:#?}");
    assert!(has("SetClipboard", "allowed", &|l| l["mime-type"] == "other" && l["error"] == "NotSupported"), "{lines:#?}");
    assert!(has("SetClipboard", "refused:DeniedByUser", &|_| true), "{lines:#?}");
    assert!(has("SensitivePrompt", "allowed", &|l| l["target"] == "SetClipboard"), "{lines:#?}");
    let all = lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
    for secret in ["diary", "From the agent", "hunter2", "bold", "confirmed", "user name", "secret", "gnome-copied-files"] {
        assert!(!all.contains(secret), "{secret} is in the log");
    }
    let _ = std::fs::remove_dir_all(&tmp);
}
