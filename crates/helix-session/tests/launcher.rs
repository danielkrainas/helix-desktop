//! Launcher tests with shell-script components on a private bus.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use helix_session::client::SessionManagerProxy;
use helix_session::launcher::{launch, LaunchError, SessionConfig};
use helix_session::{SessionEnd, SessionService};
use helix_test_support::TestBus;
use zbus::proxy::CacheProperties;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("helix-launcher-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn bus_env(bus: &TestBus) -> BTreeMap<String, String> {
    BTreeMap::from([("DBUS_SESSION_BUS_ADDRESS".to_owned(), bus.address().to_owned())])
}

const SETENV: &str = "dbus-send --session --print-reply --dest=org.gnome.SessionManager \
    /org/gnome/SessionManager org.gnome.SessionManager.Setenv";

fn process_alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks for existence.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[tokio::test(flavor = "multi_thread")]
async fn starts_components_in_order_and_propagates_environment() {
    let Some(bus) = TestBus::start() else { return };
    let dir = scratch("order");
    let out = dir.join("out");
    let config = SessionConfig::parse(&format!(
        r#"
        [environment]
        HELIX_FIXED = "yes"

        [[component]]
        name = "fake-compositor"
        command = ["sh", "-c", "{SETENV} string:WAYLAND_DISPLAY string:wayland-test >/dev/null; exec sleep 60"]
        required = true
        wait_for_env = "WAYLAND_DISPLAY"

        [[component]]
        name = "fake-shell"
        command = ["sh", "-c", "printf '%s %s' \"$WAYLAND_DISPLAY\" \"$HELIX_FIXED\" > {out}; exec sleep 60"]
        "#,
        out = out.display()
    ))
    .unwrap();

    let service = SessionService::start(bus.builder()).await.unwrap();
    let children = launch(&service, &config, &bus_env(&bus)).await.unwrap();

    let conn = bus.connect().await;
    let sm = SessionManagerProxy::builder(&conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .unwrap();
    assert!(sm.is_session_running().await.unwrap());
    // Setenv is closed once the launcher has initialized the session.
    assert!(sm.setenv("LATE", "1").await.is_err());

    for _ in 0..100 {
        if std::fs::read_to_string(&out).is_ok_and(|s| !s.is_empty()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "wayland-test yes");

    let pids = children.pids();
    assert_eq!(pids.len(), 2);
    children.terminate(Duration::from_secs(5)).await;
    for (name, pid) in pids {
        assert!(!process_alive(pid), "{name} still running");
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn waits_for_bus_name() {
    let Some(bus) = TestBus::start() else { return };
    // The component itself never owns the name; another connection claims
    // it after a delay, and launch must wait until then.
    let config = SessionConfig::parse(
        r#"
        [[component]]
        name = "waits"
        command = ["sleep", "60"]
        required = true
        wait_for_name = "org.example.Appears"
        ready_timeout_secs = 5
        "#,
    )
    .unwrap();
    let service = SessionService::start(bus.builder()).await.unwrap();
    let other = bus.connect().await;
    let claimer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        other.request_name("org.example.Appears").await.unwrap();
        other
    });
    let children = launch(&service, &config, &bus_env(&bus)).await.unwrap();
    assert!(claimer.is_finished());
    children.terminate(Duration::from_secs(5)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn required_component_that_never_gets_ready_fails_launch() {
    let Some(bus) = TestBus::start() else { return };
    let config = SessionConfig::parse(
        r#"
        [[component]]
        name = "stuck"
        command = ["sleep", "60"]
        required = true
        wait_for_env = "NEVER_SET"
        ready_timeout_secs = 1
        "#,
    )
    .unwrap();
    let service = SessionService::start(bus.builder()).await.unwrap();
    match launch(&service, &config, &bus_env(&bus)).await {
        Err(LaunchError::NotReady(name)) => assert_eq!(name, "stuck"),
        Err(other) => panic!("unexpected error {other}"),
        Ok(_) => panic!("launch should fail"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn optional_component_failure_is_tolerated() {
    let Some(bus) = TestBus::start() else { return };
    let config = SessionConfig::parse(
        r#"
        [[component]]
        name = "broken"
        command = ["/nonexistent/helix-binary"]

        [[component]]
        name = "fine"
        command = ["sleep", "60"]
        "#,
    )
    .unwrap();
    let service = SessionService::start(bus.builder()).await.unwrap();
    let children = launch(&service, &config, &bus_env(&bus)).await.unwrap();
    assert_eq!(children.pids().len(), 1);
    children.terminate(Duration::from_secs(5)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn required_component_exit_ends_session() {
    let Some(bus) = TestBus::start() else { return };
    let config = SessionConfig::parse(
        r#"
        [[component]]
        name = "short-lived"
        command = ["sh", "-c", "sleep 0.5"]
        required = true
        "#,
    )
    .unwrap();
    let service = SessionService::start(bus.builder()).await.unwrap();
    let mut children = launch(&service, &config, &bus_env(&bus)).await.unwrap();
    let end = tokio::time::timeout(
        Duration::from_secs(10),
        service.run(async {
            assert_eq!(children.required_exited().await, "short-lived");
        }),
    )
    .await
    .expect("session ends")
    .unwrap();
    assert_eq!(end, SessionEnd::Stopped);
    children.terminate(Duration::from_secs(1)).await;
}
