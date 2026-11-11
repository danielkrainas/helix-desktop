//! The session manager and logind, over a private bus with a fake logind.

use std::time::Duration;

use helix_session::client::SessionManagerProxy;
use helix_session::logind::{after_session, Logind};
use helix_session::state::{inhibit_flags, EndSessionAction, LogoutMode};
use helix_session::{ServiceOptions, SessionService};
use helix_test_support::logind::FakeLogind;
use helix_test_support::TestBus;
use zbus::proxy::CacheProperties;
use zbus::Connection;

async fn start(bus: &TestBus, logind: Option<Logind>) -> SessionManagerProxy<'static> {
    let options = ServiceOptions { logind, ..Default::default() };
    let service = SessionService::start_with_options(bus.builder(), options).await.unwrap();
    tokio::spawn(service.run(std::future::pending()));
    let conn = bus.connect().await;
    let sm = SessionManagerProxy::builder(&conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .unwrap();
    sm.initialized().await.unwrap();
    sm
}

async fn logind(bus: &TestBus) -> (FakeLogind, Logind) {
    let fake = FakeLogind::start(bus).await;
    let conn: Connection = bus.connect().await;
    (fake, Logind::new(conn))
}

/// Poll until `check` holds (signals and fd closes are asynchronous).
async fn eventually(mut check: impl AsyncFnMut() -> bool) {
    for _ in 0..100 {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("condition never became true");
}

#[tokio::test]
async fn suspend_goes_to_logind() {
    let Some(bus) = TestBus::start() else { return };
    let (fake, logind) = logind(&bus).await;
    let sm = start(&bus, Some(logind)).await;

    sm.suspend().await.unwrap();
    assert_eq!(fake.calls(), ["Suspend false"]);

    fake.refuse_suspend(true);
    let err = sm.suspend().await.unwrap_err().to_string();
    assert!(err.contains("refused"), "{err}");
}

#[tokio::test]
async fn suspend_without_logind_fails() {
    let Some(bus) = TestBus::start() else { return };
    let sm = start(&bus, None).await;
    let err = sm.suspend().await.unwrap_err().to_string();
    assert!(err.contains("org.gnome.SessionManager.Error.General"), "{err}");
}

#[tokio::test]
async fn suspend_inhibitors_block_system_sleep() {
    let Some(bus) = TestBus::start() else { return };
    let (fake, logind) = logind(&bus).await;
    let sm = start(&bus, Some(logind)).await;

    // Idle-only inhibitors don't touch logind.
    let idle = sm.inhibit("player", 0, "Playing", inhibit_flags::IDLE).await.unwrap();
    assert!(fake.held_inhibitors().is_empty());

    let a = sm.inhibit("burner", 0, "Burning a disc", inhibit_flags::SUSPEND).await.unwrap();
    let b = sm.inhibit("sync", 0, "Syncing", inhibit_flags::SUSPEND | inhibit_flags::LOGOUT).await.unwrap();
    let sleep_block = vec![("sleep".to_owned(), "block".to_owned())];
    assert_eq!(fake.held_inhibitors(), sleep_block, "one logind inhibitor for both");

    sm.uninhibit(a).await.unwrap();
    assert_eq!(fake.held_inhibitors(), sleep_block, "still one suspend inhibitor");
    sm.uninhibit(b).await.unwrap();
    eventually(async || fake.held_inhibitors().is_empty()).await;
    sm.uninhibit(idle).await.unwrap();
}

#[tokio::test]
async fn session_is_active_follows_logind() {
    let Some(bus) = TestBus::start() else { return };
    let (fake, logind) = logind(&bus).await;
    let sm = start(&bus, Some(logind)).await;
    assert!(sm.session_is_active().await.unwrap());

    fake.set_active(false).await;
    eventually(async || !sm.session_is_active().await.unwrap()).await;
    fake.set_active(true).await;
    eventually(async || sm.session_is_active().await.unwrap()).await;
}

#[tokio::test]
async fn power_off_and_restart_after_the_session() {
    let Some(bus) = TestBus::start() else { return };
    let (fake, logind) = logind(&bus).await;

    after_session(EndSessionAction::Logout(LogoutMode::Normal), Some(&logind)).await;
    assert!(fake.calls().is_empty(), "logging out leaves the machine on");
    after_session(EndSessionAction::Shutdown, Some(&logind)).await;
    after_session(EndSessionAction::Reboot, Some(&logind)).await;
    assert_eq!(fake.calls(), ["PowerOff false", "Reboot false"]);

    // Without logind the session just ends.
    after_session(EndSessionAction::Shutdown, None).await;
}
