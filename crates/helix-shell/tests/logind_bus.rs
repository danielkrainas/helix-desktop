//! The shell and logind, over a private bus with a fake logind.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use helix_session::client::SessionManagerProxy;
use helix_session::logind::Logind;
use helix_session::{ServiceOptions, SessionService};
use helix_shell::logind::{availability, lock_before_sleep, Availability};
use helix_shell::{PowerAction, ShellService, UiRequest};
use helix_test_support::logind::FakeLogind;
use helix_test_support::TestBus;

async fn eventually(mut check: impl FnMut() -> bool) {
    for _ in 0..100 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("condition never became true");
}

#[tokio::test]
async fn menu_offers_only_what_logind_allows() {
    let Some(bus) = TestBus::start() else { return };
    let conn = bus.connect().await;
    assert_eq!(availability(&conn).await, Availability::default(), "no logind: nothing");

    let fake = FakeLogind::start(&bus).await;
    fake.set_can("CanSuspend", "challenge");
    fake.set_can("CanReboot", "yes");
    fake.set_can("CanPowerOff", "na");
    assert_eq!(
        availability(&conn).await,
        Availability { suspend: false, restart: true, power_off: false }
    );
}

#[tokio::test]
async fn locks_before_sleep() {
    let Some(bus) = TestBus::start() else { return };
    let fake = FakeLogind::start(&bus).await;
    let locks = Arc::new(AtomicUsize::new(0));
    let counted = locks.clone();
    let conn = bus.connect().await;
    tokio::spawn(lock_before_sleep(conn, move || {
        let counted = counted.clone();
        async move {
            counted.fetch_add(1, Ordering::SeqCst);
        }
    }));
    let delay = vec![("sleep".to_owned(), "delay".to_owned())];
    eventually(|| fake.held_inhibitors() == delay).await;

    fake.prepare_for_sleep(true).await;
    eventually(|| fake.held_inhibitors().is_empty()).await;
    assert_eq!(locks.load(Ordering::SeqCst), 1, "locked before letting the system sleep");

    fake.prepare_for_sleep(false).await;
    eventually(|| fake.held_inhibitors() == delay).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn power_menu_suspend_goes_through_the_session_manager() {
    let Some(bus) = TestBus::start() else { return };
    let fake = FakeLogind::start(&bus).await;
    let options = ServiceOptions { logind: Some(Logind::new(bus.connect().await)), ..Default::default() };
    let session = SessionService::start_with_options(bus.builder(), options).await.unwrap();
    tokio::spawn(session.run(std::future::pending()));
    let conn = bus.connect().await;
    SessionManagerProxy::new(&conn).await.unwrap().initialized().await.unwrap();
    let (shell, _ui) = ShellService::start(bus.builder()).await.unwrap();

    shell.ui_bridge().handle(UiRequest::Power(PowerAction::Suspend)).await.unwrap();
    assert_eq!(fake.calls(), ["Suspend false"]);
}
