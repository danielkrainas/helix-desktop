//! Agent Access's *talk to the user* methods over a private bus
//! (helixdesktop/spec `contracts/agent-access/README.md`, "Talk to the
//! user methods"; helixdesktop/desktop#148): `Ask` and `Confirm` through a
//! stand-in UI that answers as scripted (typed answers, choices, yes, no,
//! cancelled) or holds the question; their limits, one question of an
//! agent's at a time, the timeout, the lock and a revoke closing an open
//! question; reminders added, listed, cancelled (by the agent, the
//! notification's button and Settings), shown when due, kept in their
//! file across a new shell, and dropped while access is off; the
//! refusals; and the activity log, which never holds the questions,
//! choices, answers or reminder texts. Nothing on the host is touched:
//! scratch XDG directories, in-memory settings, one private bus.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gio::prelude::*;
use helix_shell::agent::grants::Answer;
use helix_shell::agent::talk::{Dialog, QuestionRequest, Reply};
use helix_shell::agent::{self, AgentAccess, Agent1, AgentAccessSettings, Desktop, Setup, OBJECT_PATH};
use helix_shell::notifications::Center;
use helix_shell::notify_store::Origin;
use helix_shell::screensaver::SharedStatus;
use helix_shell::UiEvent;
use helix_test_support::conformance::{assert_conformant, check, introspect, load_dbus_contract};
use helix_test_support::TestBus;
use zbus::zvariant::{OwnedValue, Value};
use zbus::Connection;

const IFACE: &str = "org.helixdesktop.Agent1";
const SETTINGS_IFACE: &str = "org.helixdesktop.AgentAccess1";

/// The stand-in UI: answers questions as scripted (`None`: holds it),
/// and notes what it was shown and closed.
#[derive(Default)]
struct FakeUi {
    script: VecDeque<Option<Reply>>,
    held: Vec<QuestionRequest>,
    seen: Vec<(u64, Dialog, String)>,
    closed: Vec<u64>,
}

type Ui = Arc<Mutex<FakeUi>>;

fn script(ui: &Ui, reply: Option<Reply>) {
    ui.lock().unwrap().script.push_back(reply);
}

fn seen(ui: &Ui) -> usize {
    ui.lock().unwrap().seen.len()
}

fn last_seen(ui: &Ui) -> (u64, Dialog, String) {
    ui.lock().unwrap().seen.last().cloned().unwrap()
}

/// Answer the oldest question held.
fn answer_held(ui: &Ui, reply: Reply) {
    let request = ui.lock().unwrap().held.remove(0);
    request.reply.send(reply).unwrap();
}

async fn until(what: &str, f: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < deadline, "{what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn short(result: &zbus::Result<zbus::Message>) -> String {
    match result {
        Err(zbus::Error::MethodError(name, _, _)) => name.as_str().trim_start_matches("org.helixdesktop.Agent1.Error.").trim_start_matches("org.freedesktop.DBus.Error.").to_owned(),
        Err(other) => format!("{other:?}"),
        Ok(_) => "ok".to_owned(),
    }
}

async fn call(conn: &Connection, method: &str, body: &(impl serde::Serialize + zbus::zvariant::DynamicType)) -> zbus::Result<zbus::Message> {
    conn.call_method(Some("org.gnome.Shell"), OBJECT_PATH, Some(IFACE), method, body).await
}

type Opts = HashMap<&'static str, Value<'static>>;

fn opts(pairs: &[(&'static str, Value<'static>)]) -> Opts {
    pairs.iter().cloned().collect()
}

type Record = HashMap<String, OwnedValue>;

fn text(r: &Record, key: &str) -> String {
    String::try_from(r[key].clone()).unwrap()
}

async fn ask(conn: &Connection, question: &str, o: Opts) -> zbus::Result<zbus::Message> {
    call(conn, "Ask", &(question, o)).await
}

fn answer_of(reply: zbus::Result<zbus::Message>) -> (String, i32) {
    let r: Record = reply.expect("answered").body().deserialize().unwrap();
    (text(&r, "answer"), i32::try_from(r["choice"].clone()).unwrap())
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

/// Every line of the activity log in `dir`.
fn activity(dir: &Path) -> Vec<serde_json::Value> {
    let mut days: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "jsonl")).collect();
    days.sort();
    days.iter().flat_map(|d| std::fs::read_to_string(d).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect::<Vec<_>>()).collect()
}

/// The notifications listed, as (summary, body, app name, origin).
fn notes(center: &Center) -> Vec<(String, String, String, Origin, u32)> {
    center.listed().into_iter().map(|(n, _)| (n.summary.clone(), n.body.clone(), n.app_name.clone(), n.origin.clone(), n.id)).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn talking_to_the_user_over_the_bus() {
    let Some(bus) = TestBus::start() else { return };
    let tmp = std::env::temp_dir().join(format!("helix-agent-talk-{}", std::process::id()));
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
    for dir in ["home", "config", "data", "run", "sys"] {
        std::fs::create_dir_all(tmp.join(dir)).unwrap();
    }
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
        std::env::set_var("DBUS_SYSTEM_BUS_ADDRESS", bus.address());
        std::env::set_var("DBUS_SESSION_BUS_ADDRESS", bus.address());
    }

    let fake: Ui = Default::default();
    let (ui, mut ui_rx) = tokio::sync::mpsc::unbounded_channel::<UiEvent>();
    let state = fake.clone();
    tokio::spawn(async move {
        while let Some(event) = ui_rx.recv().await {
            match event {
                UiEvent::AgentQuestion(slot) => {
                    let Some(request) = slot.take() else { continue };
                    let mut s = state.lock().unwrap();
                    s.seen.push((request.id, request.dialog.clone(), request.name.clone()));
                    match s.script.pop_front().flatten() {
                        Some(reply) => {
                            let _ = request.reply.send(reply);
                        }
                        None => s.held.push(request),
                    }
                }
                UiEvent::AgentQuestionClose(id) => {
                    let mut s = state.lock().unwrap();
                    s.closed.push(id);
                    s.held.retain(|r| r.id != id);
                }
                _ => {}
            }
        }
    });

    let settings = gio::Settings::new("org.helixdesktop.agent-access");
    let lock: SharedStatus = Default::default();
    let center = Center::new(ui.clone(), lock.clone());
    let consent: agent::Consent = Arc::new(|_| Box::pin(async { Answer::Session }));
    let me = std::env::current_exe().unwrap();
    let me_id = me.to_string_lossy().into_owned();
    let log = tmp.join("state/helix/agent-activity");
    let reminders = tmp.join("state/helix/agent-reminders.json");
    let setup = || Setup {
        grants_path: None,
        consent: consent.clone(),
        confirm: agent::no_confirm(),
        proc_root: "/proc".into(),
        settings_exes: vec![me.clone()],
        log_dir: Some(log.clone()),
        reminders_path: Some(reminders.clone()),
    };
    let access = AgentAccess::new(lock.clone(), setup());
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
    let conn = bus.connect().await;

    // The methods are there now, as the contract has them; Settings'
    // interface too.
    let actual = introspect(&conn, "org.gnome.Shell", OBJECT_PATH).await;
    let later: Vec<String> = ["Screenshot", "Input"].iter().map(|m| format!("method {m}")).collect();
    let later: Vec<&str> = later.iter().map(String::as_str).collect();
    assert_conformant(IFACE, &check(IFACE, &load_dbus_contract("../agent-access/org.helixdesktop.Agent1.xml"), &actual, &later));
    assert_conformant(SETTINGS_IFACE, &check(SETTINGS_IFACE, &load_dbus_contract("../agent-access/org.helixdesktop.AgentAccess1.xml"), &actual, &[]));

    // Off: every method refused, nothing shown, nothing kept.
    let every = |conn: Connection| async move {
        vec![
            ("Ask", ask(&conn, "Deploy?", opts(&[])).await),
            ("Confirm", call(&conn, "Confirm", &("Deploy?", opts(&[]))).await),
            ("AddReminder", call(&conn, "AddReminder", &(now() + 60, "Tea", opts(&[]))).await),
            ("ListReminders", call(&conn, "ListReminders", &(opts(&[]),)).await),
            ("CancelReminder", call(&conn, "CancelReminder", &("0011223344556677", opts(&[]))).await),
        ]
    };
    for (method, result) in every(conn.clone()).await {
        assert_eq!(short(&result), "AccessOff", "{method}");
    }
    settings.set_boolean("enabled", true).unwrap();
    settings.set_boolean("allow-remote", true).unwrap();
    // Locked: refused before anything is shown.
    lock.lock().unwrap().locked_since = Some(Instant::now());
    for (method, result) in every(conn.clone()).await {
        assert_eq!(short(&result), "Locked", "{method}");
    }
    lock.lock().unwrap().locked_since = None;
    assert_eq!(seen(&fake), 0, "nothing was shown");
    assert!(!reminders.exists());

    // A typed answer (the consent prompt allows Read only first).
    script(&fake, Some(Reply::Text("feature/login".into())));
    let question = "Which branch should I\ndeploy?\u{202E}";
    let got = answer_of(ask(&conn, question, opts(&[("default", Value::from("main"))])).await);
    assert_eq!(got, ("feature/login".to_owned(), -1));
    let (_, dialog, _) = last_seen(&fake);
    assert_eq!(dialog, Dialog::Ask { question: "Which branch should I deploy?".into(), choices: vec![], default: "main".into() }, "plain text, one line, nothing reordering it");
    // A choice.
    script(&fake, Some(Reply::Choice(1)));
    let choices = opts(&[("choices", Value::from(vec!["production", "staging"])), ("default", Value::from("staging"))]);
    assert_eq!(answer_of(ask(&conn, "Where to?", choices).await), ("staging".to_owned(), 1));
    // Cancelled.
    script(&fake, Some(Reply::Cancelled));
    let r = ask(&conn, "Anything else?", opts(&[])).await;
    assert_eq!(short(&r), "Cancelled");
    // Malformed: refused before anything is shown.
    let shown = seen(&fake);
    for (q, o) in [
        ("", opts(&[])),
        ("Which?", opts(&[("choices", Value::from(vec!["only"]))])),
        ("Which?", opts(&[("choices", Value::from(vec!["same", "same"]))])),
        ("Which?", opts(&[("choices", Value::from(vec!["a", "b"])), ("default", Value::from("c"))])),
        ("Which?", opts(&[("timeout-ms", Value::from("soon"))])),
    ] {
        assert_eq!(short(&ask(&conn, q, o).await), "InvalidArgs", "{q}");
    }
    assert_eq!(short(&ask(&conn, &"q".repeat(501), opts(&[])).await), "InvalidArgs");
    assert_eq!(short(&call(&conn, "Confirm", &("Go?", opts(&[("yes", Value::from("x".repeat(25)))]))).await), "InvalidArgs");
    assert_eq!(seen(&fake), shown);

    // Confirm: yes, no, cancelled; its own labels.
    script(&fake, Some(Reply::Yes(true)));
    let yes: bool = call(&conn, "Confirm", &("Delete 3 old backups?", opts(&[("yes", Value::from("Delete")), ("no", Value::from("Keep"))]))).await.unwrap().body().deserialize().unwrap();
    assert!(yes);
    assert_eq!(last_seen(&fake).1, Dialog::Confirm { question: "Delete 3 old backups?".into(), yes: "Delete".into(), no: "Keep".into() });
    script(&fake, Some(Reply::Yes(false)));
    let no: bool = call(&conn, "Confirm", &("Deploy?", opts(&[]))).await.unwrap().body().deserialize().unwrap();
    assert!(!no);
    script(&fake, Some(Reply::Cancelled));
    assert_eq!(short(&call(&conn, "Confirm", &("Deploy?", opts(&[]))).await), "Cancelled");
    // `interactive` false spares the consent prompt only: still asked.
    script(&fake, Some(Reply::Yes(true)));
    assert!(call(&conn, "Confirm", &("Quietly?", opts(&[("interactive", Value::from(false))]))).await.is_ok());

    // Unanswered: the time runs out and the dialog is closed.
    script(&fake, None);
    let started = Instant::now();
    let r = ask(&conn, "Still there?", opts(&[("timeout-ms", Value::from(400u32))])).await;
    assert_eq!(short(&r), "TimedOut");
    assert!(started.elapsed() < Duration::from_secs(5));
    let id = last_seen(&fake).0;
    until("the timed-out question was closed", || fake.lock().unwrap().closed.contains(&id)).await;

    // One question of an agent's at a time: the second waits for the first.
    script(&fake, None);
    script(&fake, None);
    let before = seen(&fake);
    let (c1, c2) = (conn.clone(), conn.clone());
    let first = tokio::spawn(async move { answer_of(ask(&c1, "First?", opts(&[])).await) });
    until("the first question is shown", || seen(&fake) == before + 1).await;
    let second = tokio::spawn(async move { answer_of(ask(&c2, "Second?", opts(&[])).await) });
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(seen(&fake), before + 1, "the second waits while the first is open");
    answer_held(&fake, Reply::Text("one".into()));
    assert_eq!(first.await.unwrap(), ("one".to_owned(), -1));
    until("the second question is shown", || seen(&fake) == before + 2).await;
    assert_eq!(last_seen(&fake).1.question(), "Second?");
    answer_held(&fake, Reply::Text("two".into()));
    assert_eq!(second.await.unwrap(), ("two".to_owned(), -1));

    // Locking closes an open question: cancelled.
    script(&fake, None);
    let c = conn.clone();
    let pending = tokio::spawn(async move { short(&ask(&c, "Before lunch?", opts(&[])).await) });
    until("the question is shown", || !fake.lock().unwrap().held.is_empty()).await;
    lock.lock().unwrap().locked_since = Some(Instant::now());
    assert_eq!(pending.await.unwrap(), "Cancelled");
    let id = last_seen(&fake).0;
    until("closed on lock", || fake.lock().unwrap().closed.contains(&id)).await;
    lock.lock().unwrap().locked_since = None;
    // Access taken away meanwhile: the error a new call would get.
    script(&fake, None);
    let c = conn.clone();
    let pending = tokio::spawn(async move { short(&ask(&c, "Shall I go on?", opts(&[("interactive", Value::from(false))])).await) });
    until("the question is shown", || !fake.lock().unwrap().held.is_empty()).await;
    access.revoke(&me_id);
    assert_eq!(pending.await.unwrap(), "NotGranted");
    let id = last_seen(&fake).0;
    until("closed on revoke", || fake.lock().unwrap().closed.contains(&id)).await;

    // Reminders: added (the consent prompt allows again), said in a
    // notification under the agent's name with a button to cancel it.
    let soon = now() + 2;
    let reply = call(&conn, "AddReminder", &(soon, "Stand <b>up</b>\n now", opts(&[]))).await.unwrap();
    let tea: String = reply.body().deserialize().unwrap();
    assert_eq!(tea.len(), 16);
    let set = notes(&center).into_iter().find(|n| n.3 == Origin::Reminder { id: tea.clone() }).expect("the reminder-set notification");
    assert!(set.0.starts_with("Reminder set for "), "{set:?}");
    assert_eq!(set.1, "Stand <b>up</b>  now", "plain text");
    assert!(set.2.starts_with("Agent: agent_talk_bus-"), "{}", set.2);
    let listed: Vec<Record> = call(&conn, "ListReminders", &(opts(&[]),)).await.unwrap().body().deserialize().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!((text(&listed[0], "id"), i64::try_from(listed[0]["time"].clone()).unwrap(), text(&listed[0], "text")), (tea.clone(), soon, "Stand <b>up</b>  now".to_owned()));
    assert!(!listed[0].contains_key("agent"), "an agent's own records say nothing of whose");
    let mode = std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&reminders).unwrap().permissions());
    assert_eq!(mode & 0o777, 0o600);
    // Malformed, or not its own.
    assert_eq!(short(&call(&conn, "AddReminder", &(now() - 3600, "Late", opts(&[]))).await), "InvalidArgs");
    assert_eq!(short(&call(&conn, "AddReminder", &(now() + 400 * 86_400, "Far", opts(&[]))).await), "InvalidArgs");
    assert_eq!(short(&call(&conn, "AddReminder", &(now() + 60, " ", opts(&[]))).await), "InvalidArgs");
    assert_eq!(short(&call(&conn, "CancelReminder", &("ffffffffffffffff", opts(&[]))).await), "NotFound");
    // When due: shown under the agent's name, the "set" one taken back,
    // and gone from the file.
    until("the reminder came due", || notes(&center).iter().any(|n| n.0 == "Reminder" && n.1 == "Stand <b>up</b>  now")).await;
    assert!(!notes(&center).iter().any(|n| n.4 == set.4), "the reminder-set notification was taken back");
    let due = notes(&center).into_iter().find(|n| n.0 == "Reminder").unwrap();
    assert!(due.2.starts_with("Agent: "), "{}", due.2);
    let file = std::fs::read_to_string(&reminders).unwrap();
    assert!(!file.contains(&tea), "{file}");

    // Cancelled by the agent, by the notification's button, by Settings.
    let later_time = now() + 3600;
    let add = |text: &'static str| {
        let conn = conn.clone();
        async move { call(&conn, "AddReminder", &(later_time, text, opts(&[]))).await.map(|r| r.body().deserialize::<String>().unwrap()) }
    };
    let mine = add("Mine").await.unwrap();
    call(&conn, "CancelReminder", &(mine.as_str(), opts(&[]))).await.unwrap();
    assert!(!notes(&center).iter().any(|n| n.3 == Origin::Reminder { id: mine.clone() }), "its notification goes too");
    let button = add("Button").await.unwrap();
    let note = notes(&center).into_iter().find(|n| n.3 == Origin::Reminder { id: button.clone() }).unwrap();
    center.activate(&conn, note.4, Some("cancel-reminder".into())).await.unwrap();
    let by_settings = add("Settings").await.unwrap();
    let all: Vec<Record> = conn.call_method(Some("org.gnome.Shell"), OBJECT_PATH, Some(SETTINGS_IFACE), "ListReminders", &()).await.unwrap().body().deserialize().unwrap();
    assert_eq!(all.iter().map(|r| text(r, "id")).collect::<Vec<_>>(), std::slice::from_ref(&by_settings));
    assert_eq!(text(&all[0], "agent"), me_id);
    conn.call_method(Some("org.gnome.Shell"), OBJECT_PATH, Some(SETTINGS_IFACE), "CancelReminder", &(by_settings.as_str(),)).await.unwrap();
    let listed: Vec<Record> = call(&conn, "ListReminders", &(opts(&[]),)).await.unwrap().body().deserialize().unwrap();
    assert!(listed.is_empty());
    // At most 5 a minute (the four above and this one, then no more).
    add("Fifth").await.unwrap();
    let r = add("Sixth").await;
    assert!(matches!(&r, Err(zbus::Error::MethodError(n, Some(m), _)) if n.as_str().ends_with("Failed") && m.contains("at most 5")), "{r:?}");

    // Kept across a new shell: one started on the same file shows it
    // when it is due.
    let kept: Vec<Record> = call(&conn, "ListReminders", &(opts(&[]),)).await.unwrap().body().deserialize().unwrap();
    assert_eq!(kept.len(), 1);
    let fifth = text(&kept[0], "id");
    let (ui2, _rx2) = tokio::sync::mpsc::unbounded_channel::<UiEvent>();
    let center2 = Center::new(ui2.clone(), lock.clone());
    let again = AgentAccess::new(lock.clone(), setup());
    again.set_desktop(Desktop { ui: ui2, notifications: Some(center2.clone()) });
    again.reminders_due(later_time - 1);
    assert!(notes(&center2).is_empty(), "not yet");
    again.reminders_due(later_time);
    assert!(notes(&center2).iter().any(|n| n.0 == "Reminder" && n.1 == "Fifth"), "{:?}", notes(&center2));
    drop(again);

    // Due while access is off: dropped unshown (this shell still has it).
    settings.set_boolean("enabled", false).unwrap();
    access.reminders_due(later_time);
    assert!(!notes(&center).iter().any(|n| n.1 == "Fifth"), "neither shown nor still said to be set");
    assert!(!std::fs::read_to_string(&reminders).unwrap().contains(&fifth));

    // The activity log: methods, outcomes, lengths and counts; never the
    // questions, choices, answers or reminder texts.
    let lines = activity(&log);
    let has = |method: &str, outcome: &str, extra: &dyn Fn(&serde_json::Value) -> bool| lines.iter().any(|l| l["method"] == method && l["outcome"] == outcome && extra(l));
    for method in ["Ask", "Confirm", "AddReminder", "ListReminders", "CancelReminder"] {
        for refusal in ["AccessOff", "Locked"] {
            assert!(has(method, &format!("refused:{refusal}"), &|l| l["agent"] == me_id.as_str() && l["level"] == "read-only"), "{method} {refusal}: {lines:#?}");
        }
    }
    assert!(has("Ask", "allowed", &|l| l["question-length"] == 30 && l.get("choice-count").is_none() && l.get("error").is_none()), "{lines:#?}");
    assert!(has("Ask", "allowed", &|l| l["choice-count"] == 2 && l["question-length"] == 9));
    assert!(has("Ask", "cancelled", &|l| l["question-length"] == 14));
    assert!(has("Ask", "allowed", &|l| l["error"] == "TimedOut"));
    assert!(has("Ask", "allowed", &|l| l["error"] == "InvalidArgs"));
    // (The one closed on lock is counted with that one: identical
    // outcomes other than allowed within a minute are coalesced.)
    assert!(has("Ask", "refused:NotGranted", &|l| l["question-length"] == 14), "revoked while open: {lines:#?}");
    assert!(has("Confirm", "allowed", &|l| l["question-length"] == 21));
    assert!(has("Confirm", "cancelled", &|_| true));
    assert!(has("AddReminder", "allowed", &|l| l["target"] == format!("reminder:{tea}").as_str() && l["text-length"] == 20));
    assert!(has("AddReminder", "allowed", &|l| l["error"] == "Failed"), "the rate limit");
    assert!(has("CancelReminder", "allowed", &|l| l["target"] == format!("reminder:{mine}").as_str() && l["agent"] == me_id.as_str() && l["level"] == "read-only"));
    assert!(has("CancelReminder", "allowed", &|l| l["target"] == "reminder:ffffffffffffffff" && l["error"] == "NotFound"));
    assert!(has("CancelReminder", "allowed", &|l| l["target"] == format!("reminder:{by_settings}").as_str() && l.get("level").is_none()), "Settings' cancel");
    assert!(has("Reminder", "allowed", &|l| l["target"] == format!("reminder:{tea}").as_str() && l["agent"] == me_id.as_str() && l.get("level").is_none()));
    assert!(has("Reminder", "cancelled", &|l| l["target"] == format!("reminder:{button}").as_str()), "the notification's button");
    assert!(has("Reminder", "refused:AccessOff", &|_| true), "dropped while off");
    assert!(!lines.iter().any(|l| l["method"] == "ListReminders" && l.get("level").is_none()), "Settings' list is not recorded");
    let all = lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
    for secret in ["branch", "deploy", "feature/login", "production", "staging", "main", "Delete", "Keep", "Stand", "Fifth", "Before lunch"] {
        assert!(!all.contains(secret), "{secret} is in the log:\n{all}");
    }
    let _ = std::fs::remove_dir_all(&tmp);
}
