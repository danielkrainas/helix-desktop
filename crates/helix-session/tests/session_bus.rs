//! End-to-end tests against a private `dbus-daemon`.
//!
//! Each test starts its own bus, so they never touch the user's session bus.
//! If `dbus-daemon` is not installed the tests are skipped (with a notice).

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use helix_session::state::{EndSessionAction, LogoutMode};
use helix_session::{SessionEnd, SessionService};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use zbus::proxy::CacheProperties;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};
use zbus::{connection, Connection};

#[zbus::proxy(
    interface = "org.gnome.SessionManager",
    default_service = "org.gnome.SessionManager",
    default_path = "/org/gnome/SessionManager"
)]
trait SessionManager {
    fn setenv(&self, variable: &str, value: &str) -> zbus::Result<()>;
    fn initialized(&self) -> zbus::Result<()>;
    fn register_client(&self, app_id: &str, ignored: &str) -> zbus::Result<OwnedObjectPath>;
    fn unregister_client(&self, client_id: &ObjectPath<'_>) -> zbus::Result<()>;
    fn inhibit(&self, app_id: &str, ignored: u32, reason: &str, flags: u32) -> zbus::Result<u32>;
    fn uninhibit(&self, inhibit_cookie: u32) -> zbus::Result<()>;
    fn is_inhibited(&self, flags: u32) -> zbus::Result<bool>;
    fn get_inhibitors(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
    fn logout(&self, mode: u32) -> zbus::Result<()>;
    fn shutdown(&self) -> zbus::Result<()>;
    fn reboot(&self) -> zbus::Result<()>;
    fn is_session_running(&self) -> zbus::Result<bool>;
    fn get_locale(&self, category: i32) -> zbus::Result<String>;
    fn initialization_error(&self, message: &str, fatal: bool) -> zbus::Result<()>;
    fn can_reboot_to_firmware_setup(&self) -> zbus::Result<bool>;

    #[zbus(signal)]
    fn session_over(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn session_is_active(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn session_is_locked(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn inhibited_actions(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn session_name(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn restore_supported(&self) -> zbus::Result<bool>;
}

/// A private `dbus-daemon` that is killed on drop.
struct TestBus {
    child: Child,
    dir: PathBuf,
    address: String,
}

impl TestBus {
    fn start() -> Option<Self> {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "helix-session-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("bus");
        let address = format!("unix:path={}", socket.display());

        let child = match Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--nopidfile"])
            .arg(format!("--address={address}"))
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
        {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("dbus-daemon not found; skipping D-Bus integration test");
                return None;
            }
            Err(e) => panic!("failed to spawn dbus-daemon: {e}"),
        };

        let bus = Self { child, dir, address };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !socket.exists() {
            assert!(Instant::now() < deadline, "dbus-daemon did not create its socket");
            std::thread::sleep(Duration::from_millis(10));
        }
        Some(bus)
    }

    fn builder(&self) -> connection::Builder<'static> {
        connection::Builder::address(self.address.as_str()).unwrap()
    }

    async fn connect(&self) -> Connection {
        self.builder().build().await.unwrap()
    }
}

impl Drop for TestBus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Start the service on `bus`, returning its task and a stop trigger.
async fn spawn_service(
    bus: &TestBus,
) -> (JoinHandle<zbus::Result<SessionEnd>>, oneshot::Sender<()>) {
    let service = SessionService::start(bus.builder()).await.unwrap();
    let (stop_tx, stop_rx) = oneshot::channel();
    let task = tokio::spawn(service.run(async move {
        let _ = stop_rx.await;
    }));
    (task, stop_tx)
}

async fn proxy(conn: &Connection) -> SessionManagerProxy<'static> {
    SessionManagerProxy::builder(conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .unwrap()
}

fn error_name(err: &zbus::Error) -> &str {
    match err {
        zbus::Error::MethodError(name, _, _) => name.as_str(),
        other => panic!("expected a D-Bus method error, got {other:?}"),
    }
}

async fn wait_until<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(5);
    while !check().await {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn session_lifecycle_over_dbus() {
    let Some(bus) = TestBus::start() else { return };
    let (service, _stop) = spawn_service(&bus).await;
    let conn = bus.connect().await;
    let sm = proxy(&conn).await;
    let mut session_over = sm.receive_session_over().await.unwrap();

    // Initialization phase.
    assert!(!sm.is_session_running().await.unwrap());
    assert!(sm.session_is_active().await.unwrap());
    assert!(!sm.session_is_locked().await.unwrap());
    assert_eq!(sm.session_name().await.unwrap(), "helix");
    assert!(!sm.restore_supported().await.unwrap());
    assert!(!sm.can_reboot_to_firmware_setup().await.unwrap());
    assert!(!sm.get_locale(libc::LC_MESSAGES).await.unwrap().is_empty());
    let err = sm.get_locale(-1).await.unwrap_err();
    assert_eq!(error_name(&err), "org.gnome.SessionManager.Error.InvalidArgs");
    sm.initialization_error("non-fatal hiccup", false).await.unwrap();
    sm.setenv("HELIX_TEST", "1").await.unwrap();
    sm.initialized().await.unwrap();
    assert!(sm.is_session_running().await.unwrap());

    let err = sm.setenv("HELIX_TEST", "2").await.unwrap_err();
    assert_eq!(error_name(&err), "org.gnome.SessionManager.Error.NotInInitialization");

    // Client registration.
    let client = sm.register_client("org.example.App", "").await.unwrap();
    assert!(client.as_str().starts_with("/org/gnome/SessionManager/Client"));
    sm.unregister_client(&client.as_ref()).await.unwrap();
    let err = sm.unregister_client(&client.as_ref()).await.unwrap_err();
    assert_eq!(error_name(&err), "org.gnome.SessionManager.Error.NotRegistered");

    // Inhibit cookie lifecycle.
    let cookie = sm.inhibit("org.example.App", 0, "unsaved work", 0b101).await.unwrap();
    assert_ne!(cookie, 0);
    assert!(sm.is_inhibited(0b001).await.unwrap());
    assert!(!sm.is_inhibited(0b010).await.unwrap());
    assert_eq!(sm.inhibited_actions().await.unwrap(), 0b101);
    assert_eq!(sm.get_inhibitors().await.unwrap().len(), 1);

    // Ending the session is refused while logout-inhibited (except force).
    for mode in 0..=1 {
        let err = sm.logout(mode).await.unwrap_err();
        assert_eq!(error_name(&err), "org.gnome.SessionManager.Error.Inhibited");
    }
    let err = sm.shutdown().await.unwrap_err();
    assert_eq!(error_name(&err), "org.gnome.SessionManager.Error.Inhibited");
    let err = sm.reboot().await.unwrap_err();
    assert_eq!(error_name(&err), "org.gnome.SessionManager.Error.Inhibited");
    assert!(sm.is_session_running().await.unwrap());

    sm.uninhibit(cookie).await.unwrap();
    assert!(!sm.is_inhibited(u32::MAX).await.unwrap());
    assert_eq!(sm.inhibited_actions().await.unwrap(), 0);
    let err = sm.uninhibit(cookie).await.unwrap_err();
    assert_eq!(error_name(&err), "org.gnome.SessionManager.Error.InvalidCookie");

    // Undocumented logout modes are rejected.
    let err = sm.logout(3).await.unwrap_err();
    assert_eq!(error_name(&err), "org.gnome.SessionManager.Error.InvalidArgs");

    // A valid logout ends the session and the service exits.
    sm.logout(1).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), session_over.next())
        .await
        .expect("SessionOver signal")
        .expect("signal stream open");
    let end = tokio::time::timeout(Duration::from_secs(5), service)
        .await
        .expect("service exits")
        .unwrap()
        .unwrap();
    assert_eq!(
        end,
        SessionEnd::Requested(EndSessionAction::Logout(LogoutMode::NoConfirmation))
    );

    // The name is released.
    let dbus = zbus::fdo::DBusProxy::new(&conn).await.unwrap();
    let owned = dbus
        .name_has_owner("org.gnome.SessionManager".try_into().unwrap())
        .await
        .unwrap();
    assert!(!owned);
}

#[tokio::test(flavor = "multi_thread")]
async fn vanished_peer_releases_its_inhibitors_and_clients() {
    let Some(bus) = TestBus::start() else { return };
    let (_service, _stop) = spawn_service(&bus).await;
    let observer = bus.connect().await;
    let sm = proxy(&observer).await;
    sm.initialized().await.unwrap();

    {
        let app = bus.connect().await;
        let app_sm = proxy(&app).await;
        app_sm.register_client("org.example.Crashy", "").await.unwrap();
        app_sm.inhibit("org.example.Crashy", 0, "busy", 1).await.unwrap();
        assert!(sm.is_inhibited(1).await.unwrap());
        // `app` is dropped here, disconnecting from the bus like a crash would.
    }

    wait_until("inhibitor release", || async {
        !sm.is_inhibited(u32::MAX).await.unwrap()
    })
    .await;
    assert!(sm.get_inhibitors().await.unwrap().is_empty());

    // With the leaked inhibitor gone, logout goes through.
    sm.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn second_instance_cannot_take_the_name() {
    let Some(bus) = TestBus::start() else { return };
    let (_service, _stop) = spawn_service(&bus).await;
    match SessionService::start(bus.builder()).await {
        Err(zbus::Error::NameTaken) => {}
        Err(other) => panic!("expected NameTaken, got {other:?}"),
        Ok(_) => panic!("second instance should not own org.gnome.SessionManager"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn external_stop_emits_session_over() {
    let Some(bus) = TestBus::start() else { return };
    let (service, stop) = spawn_service(&bus).await;
    let conn = bus.connect().await;
    let sm = proxy(&conn).await;
    let mut session_over = sm.receive_session_over().await.unwrap();
    // Inhibitors don't block an external stop (e.g. SIGTERM).
    sm.inhibit("org.example.App", 0, "busy", 1).await.unwrap();

    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), session_over.next())
        .await
        .expect("SessionOver signal")
        .expect("signal stream open");
    let end = service.await.unwrap().unwrap();
    assert_eq!(end, SessionEnd::Stopped);
}

#[tokio::test(flavor = "multi_thread")]
async fn force_logout_overrides_inhibitors() {
    let Some(bus) = TestBus::start() else { return };
    let (service, _stop) = spawn_service(&bus).await;
    let conn = bus.connect().await;
    let sm = proxy(&conn).await;
    sm.initialized().await.unwrap();
    // Idle-only inhibitors never block; logout inhibitors block all but force.
    sm.inhibit("org.example.Player", 0, "video", 8).await.unwrap();
    sm.inhibit("org.example.Editor", 0, "unsaved", 1).await.unwrap();
    let err = sm.logout(0).await.unwrap_err();
    assert_eq!(error_name(&err), "org.gnome.SessionManager.Error.Inhibited");
    sm.logout(2).await.unwrap();
    let end = service.await.unwrap().unwrap();
    assert_eq!(end, SessionEnd::Requested(EndSessionAction::Logout(LogoutMode::Force)));
}

#[tokio::test(flavor = "multi_thread")]
async fn fatal_initialization_error_ends_session() {
    let Some(bus) = TestBus::start() else { return };
    let (service, _stop) = spawn_service(&bus).await;
    let conn = bus.connect().await;
    let sm = proxy(&conn).await;
    sm.initialization_error("compositor crashed", true).await.unwrap();
    let end = service.await.unwrap().unwrap();
    assert_eq!(end, SessionEnd::Requested(EndSessionAction::InitializationFailed));
}
