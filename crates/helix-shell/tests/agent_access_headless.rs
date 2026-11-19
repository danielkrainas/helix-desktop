//! Agent Access in a whole headless session (helixdesktop/spec
//! `contracts/agent-access/README.md`): the real shell serves
//! `org.helixdesktop.Agent1` as its contract says, checks callers, follows
//! the settings as they change, and, with no consent prompt yet
//! (helixdesktop/desktop#143), grants nothing. The session has no system
//! bus, so callers count as remote unless `allow-remote` is on.
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
    let options = HashMap::from([("interactive", Value::from(interactive))]);
    conn.call_method(Some("org.gnome.Shell"), PATH, Some(IFACE), "RequestAccess", &("", options)).await
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
    let settings = "[org/helixdesktop/agent-access]\nenabled=true\nallow-remote=false\n";
    let Some(session) = HeadlessSession::start(Options {
        comp: &comp,
        shell: Some(&shell),
        size: (800, 600),
        settings,
        env: vec![("GSETTINGS_SCHEMA_DIR".into(), schemas.to_string_lossy().into_owned())],
    }) else {
        return;
    };
    assert!(session.wait_for_log("shell ready", Duration::from_secs(30)), "the shell did not start:\n{}", session.log());
    let keyfile = session.home().join(".config/glib-2.0/settings/keyfile");

    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let conn = session.bus().connect().await;
        // As the contract says; later stages' methods are not there yet.
        let actual = introspect(&conn, "org.gnome.Shell", PATH).await;
        let later: Vec<String> = [
            "GetState", "ListWindows", "ListApps", "ListWorkspaces", "ListMonitors", "ListNotifications", "RecentFiles",
            "SearchFiles", "WaitForWindow", "WaitForWindowClosed", "Ask", "Confirm", "AddReminder", "ListReminders",
            "CancelReminder", "Open", "Launch", "WindowAction", "SwitchWorkspace", "SetToggle", "SetValue", "Notify",
            "GetClipboard", "SetClipboard", "A11yTree", "A11yAction", "A11ySetValue", "Screenshot", "Input",
        ]
        .iter()
        .map(|m| format!("method {m}"))
        .collect();
        let later: Vec<&str> = later.iter().map(String::as_str).collect();
        assert_conformant(IFACE, &check(IFACE, &load_dbus_contract("../agent-access/org.helixdesktop.Agent1.xml"), &actual, &later));

        // On, but without logind the caller counts as remote.
        let s = status(&conn).await;
        assert!(flag(&s, "enabled") && flag(&s, "remote") && !flag(&s, "locked"));
        assert_eq!(String::try_from(s["agent"].clone()).unwrap(), std::env::current_exe().unwrap().to_string_lossy());
        assert_eq!(short(&request(&conn, true).await), "Remote");

        // Settings take effect at once: allow remote sessions.
        std::fs::write(&keyfile, "[org/helixdesktop/agent-access]\nenabled=true\nallow-remote=true\n").unwrap();
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
        // Nobody can be asked yet: no grant.
        assert_eq!(short(&request(&conn, true).await), "NotGranted");
        assert_eq!(String::try_from(status(&conn).await["grant"].clone()).unwrap(), "none");
        // Settings' interface is for Settings only.
        let list = conn.call_method(Some("org.gnome.Shell"), PATH, Some("org.helixdesktop.AgentAccess1"), "ListAgents", &()).await;
        assert!(matches!(&list, Err(zbus::Error::MethodError(n, _, _)) if n.as_str() == "org.freedesktop.DBus.Error.AccessDenied"), "{list:?}");

        // Off again.
        std::fs::write(&keyfile, "[org/helixdesktop/agent-access]\nenabled=false\nallow-remote=true\n").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while short(&request(&conn, false).await) != "AccessOff" {
            assert!(Instant::now() < deadline, "turning access off was not followed");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });
    let log = session.log();
    assert!(log.contains("agent call"), "calls are recorded:\n{log}");
    let _ = std::fs::remove_dir_all(&schemas);
}
