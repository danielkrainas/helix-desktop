//! `CanShutdown` / `CanReboot` / `CanSuspend` and session restore, over a
//! private bus (fake logind; restore records in a scratch file).

use helix_session::logind::Logind;
use helix_session::state::{EndSessionAction, LogoutMode};
use helix_session::{ServiceOptions, SessionEnd, SessionService};
use helix_test_support::logind::FakeLogind;
use helix_test_support::TestBus;
use zbus::Connection;

const SM: (&str, &str, &str) = ("org.gnome.SessionManager", "/org/gnome/SessionManager", "org.gnome.SessionManager");

async fn call<B, R>(conn: &Connection, method: &str, body: &B) -> zbus::Result<R>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
    R: for<'d> zbus::zvariant::DynamicDeserialize<'d>,
{
    let reply = conn.call_method(Some(SM.0), SM.1, Some(SM.2), method, body).await?;
    reply.body().deserialize::<R>()
}

#[tokio::test(flavor = "multi_thread")]
async fn power_availability() {
    let Some(bus) = TestBus::start() else { return };
    let fake = FakeLogind::start(&bus).await;
    fake.set_can("CanPowerOff", "challenge");
    fake.set_can("CanReboot", "na");
    let logind = Logind::new(bus.connect().await);
    let options = ServiceOptions { logind: Some(logind), ..Default::default() };
    let service = SessionService::start_with_options(bus.builder(), options).await.unwrap();
    tokio::spawn(service.run(std::future::pending()));
    let conn = bus.connect().await;
    assert_eq!(call::<_, u32>(&conn, "CanShutdown", &()).await.unwrap(), 2, "after authentication");
    assert_eq!(call::<_, u32>(&conn, "CanReboot", &()).await.unwrap(), 0, "unavailable");
    assert_eq!(call::<_, u32>(&conn, "CanSuspend", &()).await.unwrap(), 3, "available");
    // A suspend inhibitor blocks suspend.
    let _cookie: u32 = call(&conn, "Inhibit", &("org.example.Player", 0u32, "playing", 4u32)).await.unwrap();
    assert_eq!(call::<_, u32>(&conn, "CanSuspend", &()).await.unwrap(), 1, "blocked by an inhibitor");

    // Without logind nothing is available.
    let Some(bus) = TestBus::start() else { return };
    let service = SessionService::start(bus.builder()).await.unwrap();
    tokio::spawn(service.run(std::future::pending()));
    let conn = bus.connect().await;
    assert_eq!(call::<_, u32>(&conn, "CanShutdown", &()).await.unwrap(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn restore_across_sessions() {
    let file = std::env::temp_dir().join(format!("helix-restore-bus-{}.toml", std::process::id()));
    let _ = std::fs::remove_file(&file);

    // First session: an editor registers; the session ends cleanly.
    let Some(bus) = TestBus::start() else { return };
    let options = ServiceOptions { restore: Some(file.clone()), ..Default::default() };
    let service = SessionService::start_with_options(bus.builder(), options).await.unwrap();
    assert!(service.apps_to_restore().is_empty());
    let run = tokio::spawn(service.run(std::future::pending()));
    let app = bus.connect().await;
    let me = app.unique_name().unwrap().to_string();
    assert!(zbus::Proxy::new(&app, SM.0, SM.1, SM.2).await.unwrap().get_property::<bool>("RestoreSupported").await.unwrap());
    call::<_, ()>(&app, "Initialized", &()).await.unwrap();
    let (reason, id, cleanup): (u32, String, Vec<String>) =
        call(&app, "RegisterRestore", &("org.example.Editor", me.as_str())).await.unwrap();
    assert_eq!(reason, 1, "a normal launch");
    assert!(id.starts_with("helix-"));
    assert!(cleanup.is_empty());
    call::<_, ()>(&app, "Logout", &(LogoutMode::NoConfirmation as u32,)).await.unwrap();
    let end = tokio::time::timeout(std::time::Duration::from_secs(5), run).await.unwrap().unwrap().unwrap();
    assert_eq!(end, SessionEnd::Requested(EndSessionAction::Logout(LogoutMode::NoConfirmation)));

    // Next login: it is to be restored, and gets its id back.
    let Some(bus) = TestBus::start() else { return };
    let options = ServiceOptions { restore: Some(file.clone()), ..Default::default() };
    let service = SessionService::start_with_options(bus.builder(), options).await.unwrap();
    assert_eq!(service.apps_to_restore(), ["org.example.Editor"]);
    tokio::spawn(service.run(std::future::pending()));
    let app = bus.connect().await;
    let me = app.unique_name().unwrap().to_string();
    let (reason, again, _): (u32, String, Vec<String>) =
        call(&app, "RegisterRestore", &("org.example.Editor", me.as_str())).await.unwrap();
    assert_eq!((reason, again.as_str()), (3, id.as_str()), "restored after a clean end");
    call::<_, ()>(&app, "UnregisterRestore", &("org.example.Editor", id.as_str())).await.unwrap();
    let (_, _, cleanup): (u32, String, Vec<String>) =
        call(&app, "RegisterRestore", &("org.example.Editor", me.as_str())).await.unwrap();
    assert_eq!(cleanup, std::slice::from_ref(&id), "the unregistered instance may be deleted");
    call::<_, ()>(&app, "DeletedInstanceIds", &("org.example.Editor", vec![id.as_str()])).await.unwrap();
    let _ = std::fs::remove_file(&file);
}
