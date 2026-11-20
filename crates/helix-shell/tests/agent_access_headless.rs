//! Agent Access in a whole headless session (helixdesktop/spec
//! `contracts/agent-access/README.md`): the real shell serves
//! `org.helixdesktop.Agent1` as its contract says, checks callers, follows
//! the settings as they change, shows the indicator while an agent is
//! active (its menu stops it), and asks in the consent prompt, which the
//! session's synthetic input (the headless input socket, the way remote
//! desktop input arrives too) cannot answer. The session has no system
//! bus, so callers count as remote unless `allow-remote` is on.
//!
//! With `HELIX_AGENT_SNAPSHOTS=dir`, the indicator, its menu and the
//! prompt are kept there as PNGs.
//!
//! Needs helix-comp and helix-shell built next to this test; skipped
//! otherwise.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use helix_test_support::conformance::{assert_conformant, check, introspect, load_dbus_contract};
use helix_test_support::headless::{sibling_binary, HeadlessSession, Options};
use zbus::zvariant::{OwnedValue, Value};
use zbus::Connection;

const PATH: &str = "/org/helixdesktop/Agent1";
const IFACE: &str = "org.helixdesktop.Agent1";

fn short(result: &zbus::Result<zbus::Message>) -> String {
    match result {
        Err(zbus::Error::MethodError(name, _, _)) => name.as_str().trim_start_matches("org.helixdesktop.Agent1.Error.").to_owned(),
        Err(other) => format!("{other:?}"),
        Ok(_) => "ok".to_owned(),
    }
}

async fn status(conn: &Connection) -> HashMap<String, OwnedValue> {
    conn.call_method(Some("org.gnome.Shell"), PATH, Some(IFACE), "GetStatus", &()).await.expect("GetStatus").body().deserialize().unwrap()
}

async fn request(conn: &Connection, interactive: bool) -> zbus::Result<zbus::Message> {
    request_level(conn, "", interactive).await
}

async fn request_level(conn: &Connection, level: &str, interactive: bool) -> zbus::Result<zbus::Message> {
    let options = HashMap::from([("interactive", Value::from(interactive))]);
    conn.call_method(Some("org.gnome.Shell"), PATH, Some(IFACE), "RequestAccess", &(level, options)).await
}

fn grant(s: &HashMap<String, OwnedValue>) -> String {
    String::try_from(s["grant"].clone()).unwrap()
}

/// The privacy pills' red (and its hover shade).
fn red(p: &[u8]) -> bool {
    matches!((p[0], p[1], p[2]), (0xC0, 0x1C, 0x28) | (0xA5, 0x1D, 0x2D) | (0x8A, 0x16, 0x22))
}

/// The leftmost red pixel along the panel's middle row, if any.
fn indicator_x((w, _, data): &(u32, u32, Vec<u8>)) -> Option<u32> {
    let y = 16;
    (0..*w).find(|x| red(&data[((y * w + x) * 4) as usize..]))
}

fn keep(session: &HeadlessSession, name: &str) {
    if let Some(dir) = std::env::var_os("HELIX_AGENT_SNAPSHOTS") {
        let _ = std::fs::create_dir_all(&dir);
        session.keep_snapshot(&std::path::Path::new(&dir).join(name));
    }
}

fn flag(s: &HashMap<String, OwnedValue>, key: &str) -> bool {
    bool::try_from(s[key].clone()).unwrap()
}

#[test]
fn agent_access_in_a_session() {
    let (Some(comp), Some(shell)) = (sibling_binary("helix-comp"), sibling_binary("helix-shell")) else {
        eprintln!("helix-comp or helix-shell not built; skipping");
        return;
    };
    let schemas = std::env::temp_dir().join(format!("helix-agent-headless-schemas-{}", std::process::id()));
    std::fs::create_dir_all(&schemas).unwrap();
    for schema in ["org.helixdesktop.agent-access", "org.helixdesktop.shell", "org.helixdesktop.extensions"] {
        let data = format!("{}/../../data/{schema}.gschema.xml", env!("CARGO_MANIFEST_DIR"));
        std::fs::copy(data, schemas.join(format!("{schema}.gschema.xml"))).unwrap();
    }
    let Ok(compiled) = std::process::Command::new("glib-compile-schemas").arg(&schemas).status() else {
        eprintln!("glib-compile-schemas is not installed; skipping");
        return;
    };
    assert!(compiled.success());
    let settings = "[org/helixdesktop/agent-access]\nenabled=true\nallow-remote=false\naccess-level='desktop'\nremember-mode='remember'\n";
    // This test is the agent; the user allowed it to read always.
    let me = std::env::current_exe().unwrap().to_string_lossy().into_owned();
    let grants = format!("version = 1\n\n[[agent]]\nid = \"{me}\"\nexecutable = \"{me}\"\nname = \"\"\nlevel = \"read-only\"\ngranted = 1\n");
    let Some(mut session) = HeadlessSession::start_prepared(Options {
        comp: &comp,
        shell: Some(&shell),
        size: (800, 600),
        settings,
        env: vec![
            ("GSETTINGS_SCHEMA_DIR".into(), schemas.to_string_lossy().into_owned()),
            // The compositor says when it drops synthetic input.
            ("RUST_LOG".into(), "info,helix_comp::compositor::remote=debug,helix_comp::compositor::headless_input=debug".into()),
        ],
    }, |home| {
        std::fs::create_dir_all(home.join(".config/helix")).unwrap();
        std::fs::write(home.join(".config/helix/agent-grants.toml"), &grants).unwrap();
    }) else {
        return;
    };
    assert!(session.wait_for_log("shell ready", Duration::from_secs(30)), "the shell did not start:\n{}", session.log());
    let keyfile = session.home().join(".config/glib-2.0/settings/keyfile");

    // Several threads: the pending request goes on while the test waits.
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    rt.block_on(async {
        let conn = session.bus().connect().await;
        // As the contract says.
        let actual = introspect(&conn, "org.gnome.Shell", PATH).await;
        assert_conformant(IFACE, &check(IFACE, &load_dbus_contract("../agent-access/org.helixdesktop.Agent1.xml"), &actual, &[]));

        // On, but without logind the caller counts as remote.
        let s = status(&conn).await;
        assert!(flag(&s, "enabled") && flag(&s, "remote") && !flag(&s, "locked"));
        assert_eq!(String::try_from(s["agent"].clone()).unwrap(), std::env::current_exe().unwrap().to_string_lossy());
        assert_eq!(short(&request(&conn, true).await), "Remote");

        // Settings take effect at once: allow remote sessions.
        let on = "[org/helixdesktop/agent-access]\nenabled=true\nallow-remote=true\naccess-level='desktop'\nremember-mode='remember'\n";
        std::fs::write(&keyfile, on).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let reply = request(&conn, false).await;
            if short(&reply) == "NotGranted" {
                break;
            }
            assert_eq!(short(&reply), "Remote");
            assert!(Instant::now() < deadline, "allow-remote was not followed");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(grant(&status(&conn).await), "always");

        // The indicator: shown while the agent is active (red, beside the
        // privacy pills), its menu stops it.
        let before = session.fresh_snapshot();
        assert_eq!(indicator_x(&before), None, "no indicator before any call");
        assert_eq!(short(&request_level(&conn, "read-only", true).await), "ok", "the remembered grant covers reading");

        // The CLI's call is in the activity log, which the CLI shows (this
        // test, its parent, is the agent).
        if let Some(cli) = sibling_binary("helix-agent") {
            let run = |args: &[&str]| {
                let out = std::process::Command::new(&cli)
                    .args(args)
                    .env("DBUS_SESSION_BUS_ADDRESS", session.bus().address())
                    .env("XDG_STATE_HOME", session.home().join(".local/state"))
                    .env("HOME", session.home())
                    .output()
                    .unwrap();
                assert!(out.status.success(), "helix-agent {args:?}: {}", String::from_utf8_lossy(&out.stderr));
                serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()
            };
            run(&["windows", "--no-ask", "--json"]);
            let log = run(&["log", "--json", "--agent", &me]);
            let entries = log["entries"].as_array().unwrap();
            assert!(
                entries.iter().any(|e| e["method"] == "ListWindows" && e["outcome"] == "allowed" && e["level"] == "read-only" && e["time"].is_i64()),
                "{log:#}"
            );
            assert!(entries.iter().any(|e| e["method"] == "RequestAccess" && e["outcome"] == "refused:Remote"), "{log:#}");
        } else {
            eprintln!("helix-agent not built; its log not tested");
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        let x = loop {
            if let Some(x) = indicator_x(&session.fresh_snapshot()) {
                break x;
            }
            assert!(Instant::now() < deadline, "no indicator:\n{}", session.log());
        };
        keep(&session, "agent-indicator.png");
        session.input(&format!("click {} 16", x + 8));
        tokio::time::sleep(Duration::from_millis(500)).await;
        keep(&session, "agent-indicator-menu.png");
        // Down to "Stop …", Enter.
        session.input("keysym 0xff54 1\nkeysym 0xff54 0\nkeysym 0xff0d 1\nkeysym 0xff0d 0");
        let deadline = Instant::now() + Duration::from_secs(10);
        while grant(&status(&conn).await) != "none" {
            assert!(Instant::now() < deadline, "Stop did not stop the agent:\n{}", session.log());
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(session.wait_for_log("agent stopped from the indicator", Duration::from_secs(5)));
        let deadline = Instant::now() + Duration::from_secs(10);
        while indicator_x(&session.fresh_snapshot()).is_some() {
            assert!(Instant::now() < deadline, "the indicator stayed after Stop");
        }

        // Stopped, it is asked again: the consent prompt.
        let quiet = session.fresh_snapshot();
        let asker = session.bus().connect().await;
        let pending = tokio::spawn(async move { short(&request_level(&asker, "read-only", true).await) });
        assert!(session.wait_for_log("agent prompt shown", Duration::from_secs(10)), "no prompt:\n{}", session.log());
        // (Armed after a second.)
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let shown = session.fresh_snapshot();
        keep(&session, "agent-prompt.png");
        let at = |(w, _, data): &(u32, u32, Vec<u8>), x: u32, y: u32| -> u32 { let i = ((y * w + x) * 4) as usize; u32::from(data[i]) + u32::from(data[i + 1]) + u32::from(data[i + 2]) };
        assert!(at(&shown, 20, 580) < at(&quiet, 20, 580), "the prompt's scrim darkens the screen");

        // Synthetic input cannot answer it: Enter (Deny has the focus),
        // Tab and Enter (Allow for This Session), Escape, and clicks all
        // over where its buttons are.
        session.input("keysym 0xff0d 1\nkeysym 0xff0d 0\ntap 15\nkeysym 0xff0d 1\nkeysym 0xff0d 0\ntap 1\ntype y");
        for y in (200..520).step_by(16) {
            for x in [150, 300, 400, 500, 650] {
                session.input(&format!("click {x} {y}"));
            }
        }
        tokio::time::sleep(Duration::from_millis(1000)).await;
        assert!(!pending.is_finished(), "the prompt was answered by synthetic input:\n{}", session.log());
        assert_eq!(grant(&status(&conn).await), "none");
        let log = session.log();
        assert!(log.contains("an agent prompt is shown"), "the compositor dropped the input:\n{log}");
        assert!(!log.contains("agent prompt answered"), "{log}");
        // Other callers wait for the prompt or are refused without asking.
        assert_eq!(short(&request(&conn, false).await), "NotGranted");
        // Settings' interface is for Settings only.
        let list = conn.call_method(Some("org.gnome.Shell"), PATH, Some("org.helixdesktop.AgentAccess1"), "ListAgents", &()).await;
        assert!(matches!(&list, Err(zbus::Error::MethodError(n, _, _)) if n.as_str() == "org.freedesktop.DBus.Error.AccessDenied"), "{list:?}");

        // Off again.
        std::fs::write(&keyfile, "[org/helixdesktop/agent-access]\nenabled=false\nallow-remote=true\naccess-level='desktop'\nremember-mode='remember'\n").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while short(&request(&conn, false).await) != "AccessOff" {
            assert!(Instant::now() < deadline, "turning access off was not followed");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });
    // The activity log: calls, Stop from the indicator, and a program
    // other than Settings knocking on Settings' interface.
    let dir = session.home().join(".local/state/helix/agent-activity");
    let mut days: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.path()).collect();
    days.sort();
    let lines: Vec<serde_json::Value> = days.iter().flat_map(|d| std::fs::read_to_string(d).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect::<Vec<_>>()).collect();
    let has = |method: &str, outcome: &str| lines.iter().any(|l| l["method"] == method && l["outcome"] == outcome && l["agent"] == me.as_str());
    assert!(has("RequestAccess", "refused:Remote") && has("RequestAccess", "allowed") && has("RequestAccess", "refused:AccessOff"), "{lines:#?}");
    assert!(has("Stop", "cancelled"), "Stop from the indicator: {lines:#?}");
    assert!(has("ListAgents", "refused:AccessDenied"), "{lines:#?}");
    assert!(!lines.iter().any(|l| l["method"] == "GetStatus"));
    let _ = std::fs::remove_dir_all(&schemas);
}
