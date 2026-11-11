//! The shell's end-session dialog driven by the real session manager.

use std::time::Duration;

use helix_session::client::SessionManagerProxy;
use helix_session::{SessionEnd, SessionService};
use helix_shell::end_dialog::{Choice, EndDialog, LOGOUT};
use helix_shell::{PowerAction, ShellService, UiEvent, UiRequest};
use helix_test_support::TestBus;
use tokio::sync::mpsc::UnboundedReceiver;
use zbus::proxy::CacheProperties;

async fn next_dialog(ui: &mut UnboundedReceiver<UiEvent>) -> Option<EndDialog> {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), ui.recv()).await.expect("ui event").unwrap();
        if let UiEvent::EndDialog(d) = event {
            return d;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn power_menu_logout_through_the_dialog() {
    let Some(bus) = TestBus::start() else { return };
    let session = SessionService::start(bus.builder()).await.unwrap();
    let session = tokio::spawn(session.run(std::future::pending()));
    let (shell, mut ui) = ShellService::start(bus.builder()).await.unwrap();
    let bridge = shell.ui_bridge();

    let conn = bus.connect().await;
    let sm = SessionManagerProxy::builder(&conn).cache_properties(CacheProperties::No).build().await.unwrap();
    sm.initialized().await.unwrap();
    // An app that blocks logout.
    let app = bus.connect().await;
    let app_sm = SessionManagerProxy::new(&app).await.unwrap();
    app_sm.inhibit("org.example.Editor", 0, "Unsaved document", 1).await.unwrap();

    // Power menu → Log Out… → session manager → the shell's dialog.
    bridge.handle(UiRequest::Power(PowerAction::LogOut)).await.unwrap();
    let dialog = next_dialog(&mut ui).await.expect("dialog opens");
    assert_eq!(dialog.kind, LOGOUT);
    assert_eq!(dialog.seconds, 60);
    assert_eq!(dialog.blockers, vec![("org.example.Editor".to_owned(), "Unsaved document".to_owned())]);

    // Cancel: the session keeps running.
    bridge.handle(UiRequest::EndDialogAnswer(Choice::Canceled)).await.unwrap();
    assert_eq!(next_dialog(&mut ui).await, None);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(sm.is_session_running().await.unwrap());

    // Again, and "Log Out Anyway".
    bridge.handle(UiRequest::Power(PowerAction::LogOut)).await.unwrap();
    assert!(next_dialog(&mut ui).await.is_some());
    bridge.handle(UiRequest::EndDialogAnswer(Choice::Confirmed)).await.unwrap();
    let end = tokio::time::timeout(Duration::from_secs(10), session).await.expect("session ends").unwrap().unwrap();
    assert!(matches!(end, SessionEnd::Requested(_)));
}
