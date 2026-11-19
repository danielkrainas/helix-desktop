//! Agent Access over a private bus (helixdesktop/spec
//! `contracts/agent-access/README.md`): every refusal of the policy check
//! as a caller sees it, the consent hook, grants, Settings' interface and
//! settings taking effect at once. A fake logind stands in on the "system"
//! bus; nothing on the host is touched.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use gio::prelude::*;
use helix_shell::agent::grants::Answer;
use helix_shell::agent::{self, AgentAccess, Agent1, AgentAccessSettings, Consent, Setup, OBJECT_PATH};
use helix_shell::screensaver::SharedStatus;
use helix_test_support::conformance::{assert_conformant, check, introspect, load_dbus_contract};
use helix_test_support::logind::FakeLogind;
use helix_test_support::TestBus;
use zbus::zvariant::{OwnedValue, Value};
use zbus::Connection;

const IFACE: &str = "org.helixdesktop.Agent1";
const SETTINGS_IFACE: &str = "org.helixdesktop.AgentAccess1";

type Status = HashMap<String, OwnedValue>;

/// Consent answers, in order; `Unanswered` when none is queued.
#[derive(Clone, Default)]
struct FakeConsent {
    answers: Arc<Mutex<VecDeque<Answer>>>,
    asked: Arc<AtomicUsize>,
    levels: Arc<Mutex<Vec<(String, bool)>>>,
}

impl FakeConsent {
    fn hook(&self) -> Consent {
        let me = self.clone();
        Arc::new(move |request| {
            me.asked.fetch_add(1, Ordering::SeqCst);
            me.levels.lock().unwrap().push((request.level.id().to_owned(), request.offer_always));
            let answer = me.answers.lock().unwrap().pop_front().unwrap_or(Answer::Unanswered);
            Box::pin(async move { answer })
        })
    }

    fn queue(&self, answer: Answer) {
        self.answers.lock().unwrap().push_back(answer);
    }

    fn asked(&self) -> usize {
        self.asked.load(Ordering::SeqCst)
    }
}

fn error_name(result: &zbus::Result<zbus::Message>) -> String {
    match result {
        Err(zbus::Error::MethodError(name, _, _)) => name.as_str().to_owned(),
        Err(other) => format!("{other:?}"),
        Ok(_) => "ok".to_owned(),
    }
}

fn short(result: &zbus::Result<zbus::Message>) -> String {
    let name = error_name(result);
    name.strip_prefix("org.helixdesktop.Agent1.Error.").map(str::to_owned).unwrap_or(name)
}

fn text(status: &Status, key: &str) -> String {
    String::try_from(status[key].clone()).unwrap()
}

fn flag(status: &Status, key: &str) -> bool {
    bool::try_from(status[key].clone()).unwrap()
}

async fn call(conn: &Connection, method: &str, body: &(impl serde::Serialize + zbus::zvariant::DynamicType)) -> zbus::Result<zbus::Message> {
    conn.call_method(Some("org.gnome.Shell"), OBJECT_PATH, Some(IFACE), method, body).await
}

async fn status(conn: &Connection) -> Status {
    call(conn, "GetStatus", &()).await.unwrap().body().deserialize().unwrap()
}

fn options(interactive: bool) -> HashMap<&'static str, Value<'static>> {
    HashMap::from([("interactive", Value::from(interactive))])
}

async fn request(conn: &Connection, level: &str, interactive: bool) -> zbus::Result<zbus::Message> {
    call(conn, "RequestAccess", &(level, options(interactive))).await
}

async fn settings_call(conn: &Connection, method: &str, body: &(impl serde::Serialize + zbus::zvariant::DynamicType)) -> zbus::Result<zbus::Message> {
    conn.call_method(Some("org.gnome.Shell"), OBJECT_PATH, Some(SETTINGS_IFACE), method, body).await
}

async fn agents(conn: &Connection) -> Vec<Status> {
    settings_call(conn, "ListAgents", &()).await.unwrap().body().deserialize().unwrap()
}

async fn eventually<F: std::future::Future<Output = bool>>(mut check: impl FnMut() -> F) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !check().await {
        assert!(Instant::now() < deadline, "condition never became true");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn policy_over_the_bus() {
    let Some(bus) = TestBus::start() else { return };
    let tmp = std::env::temp_dir().join(format!("helix-agent-access-{}", std::process::id()));
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
    // SAFETY: this test binary runs this one test, before any GLib use.
    unsafe {
        std::env::set_var("GSETTINGS_SCHEMA_DIR", &schemas);
        std::env::set_var("GSETTINGS_BACKEND", "memory");
        std::env::set_var("XDG_CONFIG_HOME", tmp.join("config"));
        std::env::set_var("XDG_STATE_HOME", tmp.join("state"));
        // logind is asked on the "system" bus: the private one.
        std::env::set_var("DBUS_SYSTEM_BUS_ADDRESS", bus.address());
    }
    let settings = gio::Settings::new("org.helixdesktop.agent-access");
    let logind = FakeLogind::start(&bus).await;
    let consent = FakeConsent::default();
    let lock: SharedStatus = Default::default();
    let me = std::env::current_exe().unwrap();
    let grants_path = tmp.join("config/helix/agent-grants.toml");
    let access = AgentAccess::new(
        lock.clone(),
        Setup {
            grants_path: Some(grants_path.clone()),
            consent: consent.hook(),
            confirm: agent::no_confirm(),
            proc_root: "/proc".into(),
            // This test acts as Settings too.
            settings_exes: vec![me.clone()],
        },
    );
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

    // The interfaces match their contracts (methods of later stages are
    // not there yet).
    let actual = introspect(&conn, "org.gnome.Shell", OBJECT_PATH).await;
    let later: Vec<String> = [
        "GetState", "ListWindows", "ListApps", "ListWorkspaces", "ListMonitors", "ListNotifications", "RecentFiles", "SearchFiles",
        "WaitForWindow", "WaitForWindowClosed", "Ask", "Confirm", "AddReminder", "ListReminders", "CancelReminder", "Open", "Launch",
        "WindowAction", "SwitchWorkspace", "SetToggle", "SetValue", "Notify", "GetClipboard", "SetClipboard", "A11yTree",
        "A11yAction", "A11ySetValue", "Screenshot", "Input",
    ]
    .iter()
    .map(|m| format!("method {m}"))
    .collect();
    let later: Vec<&str> = later.iter().map(String::as_str).collect();
    assert_conformant(IFACE, &check(IFACE, &load_dbus_contract("../agent-access/org.helixdesktop.Agent1.xml"), &actual, &later));
    assert_conformant(SETTINGS_IFACE, &check(SETTINGS_IFACE, &load_dbus_contract("../agent-access/org.helixdesktop.AgentAccess1.xml"), &actual, &[]));

    let mut signals = zbus::MessageStream::for_match_rule(
        zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).interface(IFACE).unwrap().member("AccessChanged").unwrap().build(),
        &conn,
        None,
    )
    .await
    .unwrap();

    // Off by default: GetStatus still answers, and says who we are.
    let s = status(&conn).await;
    assert!(!flag(&s, "enabled"));
    assert_eq!(u32::try_from(s["version"].clone()).unwrap(), 1);
    assert_eq!(text(&s, "access-level"), "read-only");
    assert_eq!(text(&s, "grant"), "none");
    assert_eq!(text(&s, "granted-level"), "");
    assert_eq!(text(&s, "agent"), me.to_string_lossy());
    assert!(flag(&s, "identified"));
    assert!(!flag(&s, "remote") && !flag(&s, "locked"));
    assert_eq!(short(&request(&conn, "", true).await), "AccessOff");
    assert_eq!(short(&request(&conn, "everything", true).await), "AccessOff", "no argument errors without access");
    assert_eq!(consent.asked(), 0);

    settings.set_boolean("enabled", true).unwrap();
    assert_eq!(error_name(&request(&conn, "everything", true).await), "org.freedesktop.DBus.Error.InvalidArgs");

    // A remote session is refused unless allowed; no session is local.
    logind.set_remote(true);
    assert!(flag(&status(&conn).await, "remote"));
    assert_eq!(short(&request(&conn, "", false).await), "Remote");
    settings.set_boolean("allow-remote", true).unwrap();
    assert_eq!(short(&request(&conn, "", false).await), "NotGranted", "past the remote check");
    settings.set_boolean("allow-remote", false).unwrap();
    logind.set_remote(false);
    logind.set_no_session(true);
    assert!(!flag(&status(&conn).await, "remote"));
    logind.set_no_session(false);

    // Nothing while locked (or locking).
    lock.lock().unwrap().locked_since = Some(Instant::now());
    assert!(flag(&status(&conn).await, "locked"));
    assert_eq!(short(&request(&conn, "", true).await), "Locked");
    lock.lock().unwrap().locked_since = None;
    lock.lock().unwrap().requested = true;
    assert_eq!(short(&request(&conn, "", true).await), "Locked");
    lock.lock().unwrap().requested = false;

    // Above the configured level.
    assert_eq!(short(&request(&conn, "desktop", true).await), "LevelTooLow");
    assert_eq!(consent.asked(), 0, "nobody was asked so far");

    // Not interactive: never asks.
    assert_eq!(short(&request(&conn, "read-only", false).await), "NotGranted");
    assert_eq!(consent.asked(), 0);
    // Nobody to ask (the prompt went unanswered).
    assert_eq!(short(&request(&conn, "read-only", true).await), "NotGranted");
    assert_eq!(consent.asked(), 1);

    // Denied: remembered for the session, without asking again.
    consent.queue(Answer::Deny);
    assert_eq!(short(&request(&conn, "", true).await), "DeniedByUser");
    assert_eq!(short(&request(&conn, "", true).await), "DeniedByUser");
    assert_eq!(consent.asked(), 2);
    assert_eq!(text(&status(&conn).await, "grant"), "denied");
    let listed = agents(&conn).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(Vec::<String>::try_from(listed[0]["kinds"].clone()).unwrap(), ["denied"]);
    // Settings revokes the denial.
    settings_call(&conn, "Revoke", &(me.to_string_lossy().as_ref(),)).await.unwrap();
    assert!(agents(&conn).await.is_empty());

    // Allowed for the session; "Always allow" is not offered under "ask".
    consent.queue(Answer::Always);
    let s: Status = request(&conn, "", true).await.unwrap().body().deserialize().unwrap();
    assert_eq!(text(&s, "grant"), "session", "Always allow counts as this session only under ask");
    assert_eq!(text(&s, "granted-level"), "read-only");
    assert_eq!(consent.levels.lock().unwrap().last().cloned(), Some(("read-only".to_owned(), false)));
    request(&conn, "read-only", false).await.unwrap();
    assert_eq!(consent.asked(), 3, "the grant covers it");
    assert!(!grants_path.exists(), "nothing remembered");

    // Raising the level widens no grant: asked again for the new level.
    settings.set_string("access-level", "desktop").unwrap();
    assert_eq!(text(&status(&conn).await, "granted-level"), "read-only");
    assert_eq!(short(&request(&conn, "desktop", false).await), "NotGranted");
    // Remember mode offers Always allow, which is written down.
    settings.set_string("remember-mode", "remember").unwrap();
    consent.queue(Answer::Always);
    request(&conn, "", true).await.unwrap();
    assert_eq!(consent.levels.lock().unwrap().last().cloned(), Some(("desktop".to_owned(), true)));
    let s = status(&conn).await;
    assert_eq!((text(&s, "grant"), text(&s, "granted-level")), ("session".to_owned(), "desktop".to_owned()));
    let file = std::fs::read_to_string(&grants_path).unwrap();
    assert!(file.contains("level = \"desktop\""), "{file}");
    // Lowering the level narrows the grant at once.
    settings.set_string("access-level", "read-only").unwrap();
    assert_eq!(text(&status(&conn).await, "granted-level"), "read-only");
    assert_eq!(short(&request(&conn, "desktop", false).await), "LevelTooLow");
    settings.set_string("access-level", "desktop").unwrap();

    // Revoke keeps the remembered grant (let in again under remember).
    settings_call(&conn, "Revoke", &(me.to_string_lossy().as_ref(),)).await.unwrap();
    let s = status(&conn).await;
    assert_eq!((text(&s, "grant"), text(&s, "granted-level")), ("always".to_owned(), "desktop".to_owned()));
    request(&conn, "desktop", false).await.unwrap();

    // Turning access off ends session grants at once and tells agents.
    while tokio::time::timeout(Duration::from_millis(50), signals.next()).await.is_ok() {}
    consent.queue(Answer::Session);
    settings.set_string("remember-mode", "ask").unwrap();
    request(&conn, "", true).await.unwrap();
    assert_eq!(text(&status(&conn).await, "grant"), "session");
    settings.set_boolean("enabled", false).unwrap();
    tokio::time::timeout(Duration::from_secs(5), signals.next()).await.expect("AccessChanged").unwrap().unwrap();
    eventually(|| {
        let conn = conn.clone();
        async move { text(&status(&conn).await, "grant") == "none" }
    })
    .await;
    assert_eq!(short(&request(&conn, "", true).await), "AccessOff");
    settings.set_boolean("enabled", true).unwrap();
    settings.set_string("remember-mode", "remember").unwrap();
    assert_eq!(text(&status(&conn).await, "grant"), "always", "the remembered grant outlives turning off");

    // Forget removes it, in the file too.
    settings_call(&conn, "Forget", &(me.to_string_lossy().as_ref(),)).await.unwrap();
    assert_eq!(text(&status(&conn).await, "grant"), "none");
    assert!(!std::fs::read_to_string(&grants_path).unwrap().contains("desktop"));
    settings_call(&conn, "StopAll", &()).await.unwrap();
    settings_call(&conn, "Forget", &("/no/such/agent",)).await.unwrap();
    // ClearLog removes the log's day files only.
    let log = tmp.join("state/helix/agent-activity");
    std::fs::create_dir_all(&log).unwrap();
    std::fs::write(log.join("2026-11-19.jsonl"), "{}\n").unwrap();
    std::fs::write(log.join("keep.txt"), "").unwrap();
    settings_call(&conn, "ClearLog", &()).await.unwrap();
    assert!(!log.join("2026-11-19.jsonl").exists() && log.join("keep.txt").exists());

    // Settings' interface refuses programs that are not Settings.
    let other = AgentAccess::new(lock.clone(), Setup { grants_path: None, settings_exes: vec!["/usr/bin/helix-settings-app".into()], ..Setup::default() });
    let _other_server = bus
        .builder()
        .serve_at(OBJECT_PATH, AgentAccessSettings { access: other })
        .unwrap()
        .name("org.helixdesktop.TestOther")
        .unwrap()
        .build()
        .await
        .unwrap();
    for (method, arg) in [("ListAgents", None), ("Revoke", Some("x")), ("Forget", Some("x")), ("StopAll", None), ("ClearLog", None)] {
        let reply = match arg {
            Some(a) => conn.call_method(Some("org.helixdesktop.TestOther"), OBJECT_PATH, Some(SETTINGS_IFACE), method, &(a,)).await,
            None => conn.call_method(Some("org.helixdesktop.TestOther"), OBJECT_PATH, Some(SETTINGS_IFACE), method, &()).await,
        };
        assert_eq!(error_name(&reply), "org.freedesktop.DBus.Error.AccessDenied", "{method}");
    }

    // Without logind, callers count as remote.
    // SAFETY: no other thread reads the environment meanwhile.
    unsafe { std::env::set_var("DBUS_SYSTEM_BUS_ADDRESS", format!("unix:path={}", tmp.join("no-bus").display())) };
    let blind = AgentAccess::new(lock.clone(), Setup { grants_path: None, consent: consent.hook(), settings_exes: Vec::new(), ..Setup::default() });
    let _blind_server = bus
        .builder()
        .serve_at(OBJECT_PATH, Agent1 { access: blind })
        .unwrap()
        .name("org.helixdesktop.TestBlind")
        .unwrap()
        .build()
        .await
        .unwrap();
    let blind_request = conn.call_method(Some("org.helixdesktop.TestBlind"), OBJECT_PATH, Some(IFACE), "RequestAccess", &("", options(false))).await;
    assert_eq!(short(&blind_request), "Remote");

    let _ = std::fs::remove_dir_all(&tmp);
}
