//! `Setenv` values reach the application launch environment
//! (helix-desktop-spec `org.gnome.SessionManager.md`, "Setenv").

use std::path::{Path, PathBuf};
use std::time::Duration;

use helix_session::client::SessionManagerProxy;
use helix_session::SessionService;
use helix_test_support::TestBus;

const PROBES: [&str; 2] = ["org.helix.EnvProbe1", "org.helix.EnvProbe2"];

/// A bus whose activatable services dump their environment to
/// `<dir>/<name>.env`.
fn probe_bus(dir: &Path) -> Option<TestBus> {
    let services = dir.join("services");
    std::fs::create_dir_all(&services).unwrap();
    for name in PROBES {
        let out = dir.join(format!("{name}.env"));
        std::fs::write(
            services.join(format!("{name}.service")),
            format!(
                "[D-BUS Service]\nName={name}\nExec=/bin/sh -c 'env > {0}.tmp && mv {0}.tmp {0}'\n",
                out.display()
            ),
        )
        .unwrap();
    }
    TestBus::start_with_services(&services)
}

/// Have the bus start probe `name` and return the environment it saw.
async fn activated_env(conn: &zbus::Connection, dir: &Path, name: &'static str) -> String {
    // The probe exits without taking its name, so the call only fails
    // after the bus's start timeout; don't wait for it.
    let caller = conn.clone();
    tokio::spawn(async move {
        let _ = caller.call_method(Some(name), "/", Some(name), "Ping", &()).await;
    });
    let out: PathBuf = dir.join(format!("{name}.env"));
    for _ in 0..100 {
        if let Ok(env) = std::fs::read_to_string(&out) {
            return env;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("probe {name} never ran");
}

#[tokio::test]
async fn setenv_reaches_activated_services() {
    let dir = std::env::temp_dir().join(format!("helix-setenv-{}", std::process::id()));
    let Some(bus) = probe_bus(&dir) else { return };
    let service = SessionService::start(bus.builder()).await.unwrap();
    let conn = bus.connect().await;
    let sm = SessionManagerProxy::new(&conn).await.unwrap();

    sm.setenv("HELIX_PROBE", "first").await.unwrap();
    sm.setenv("HELIX_PROBE", "hello world").await.unwrap();
    let env = activated_env(&conn, &dir, PROBES[0]).await;
    assert!(env.lines().any(|l| l == "HELIX_PROBE=hello world"), "{env}");
    assert_eq!(service.environment()["HELIX_PROBE"], "hello world");

    // After initialization Setenv is refused and changes nothing.
    sm.initialized().await.unwrap();
    assert!(sm.setenv("HELIX_LATE", "x").await.is_err());
    let env = activated_env(&conn, &dir, PROBES[1]).await;
    assert!(env.lines().any(|l| l == "HELIX_PROBE=hello world"), "{env}");
    assert!(!env.contains("HELIX_LATE"), "{env}");

    drop(bus);
    let _ = std::fs::remove_dir_all(&dir);
}
