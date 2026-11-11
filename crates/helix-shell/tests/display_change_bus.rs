//! The shell's `org.helix.DisplayChangeDialog`.

use std::time::Duration;

use futures_util::StreamExt;
use helix_shell::{ShellService, UiEvent, UiRequest};
use helix_test_support::TestBus;
use zbus::{MatchRule, MessageStream};

#[tokio::test(flavor = "multi_thread")]
async fn asks_the_ui_and_reports_the_answer() {
    let Some(bus) = TestBus::start() else { return };
    let (shell, mut ui) = ShellService::start(bus.builder()).await.unwrap();
    let bridge = shell.ui_bridge();
    let conn = bus.connect().await;
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.helix.DisplayChangeDialog")
        .unwrap()
        .build();
    let mut signals = MessageStream::for_match_rule(rule, &conn, None).await.unwrap();
    let call = |method: &'static str, seconds: Option<u32>| {
        let conn = conn.clone();
        async move {
            let (path, iface) = ("/org/helix/DisplayChangeDialog", "org.helix.DisplayChangeDialog");
            match seconds {
                Some(s) => conn.call_method(Some("org.gnome.Shell"), path, Some(iface), method, &(s,)).await,
                None => conn.call_method(Some("org.gnome.Shell"), path, Some(iface), method, &()).await,
            }
            .unwrap();
        }
    };
    let next_event = async |ui: &mut tokio::sync::mpsc::UnboundedReceiver<UiEvent>| {
        tokio::time::timeout(Duration::from_secs(5), ui.recv()).await.unwrap().unwrap()
    };
    let next_signal = async |signals: &mut MessageStream| {
        let message = tokio::time::timeout(Duration::from_secs(5), signals.next()).await.unwrap().unwrap().unwrap();
        message.header().member().unwrap().to_string()
    };

    // An answer nobody asked for is ignored.
    bridge.handle(UiRequest::DisplayChangeAnswer { keep: true }).await.unwrap();

    call("Open", Some(20)).await;
    assert_eq!(next_event(&mut ui).await, UiEvent::DisplayChange(Some(20)));
    bridge.handle(UiRequest::DisplayChangeAnswer { keep: true }).await.unwrap();
    assert_eq!(next_signal(&mut signals).await, "Kept");

    call("Open", Some(20)).await;
    assert_eq!(next_event(&mut ui).await, UiEvent::DisplayChange(Some(20)));
    bridge.handle(UiRequest::DisplayChangeAnswer { keep: false }).await.unwrap();
    assert_eq!(next_signal(&mut signals).await, "Reverted");

    call("Open", Some(20)).await;
    assert_eq!(next_event(&mut ui).await, UiEvent::DisplayChange(Some(20)));
    call("Close", None).await;
    assert_eq!(next_event(&mut ui).await, UiEvent::DisplayChange(None));
    // Closed: a late answer sends nothing.
    bridge.handle(UiRequest::DisplayChangeAnswer { keep: false }).await.unwrap();
    assert!(tokio::time::timeout(Duration::from_millis(300), signals.next()).await.is_err());
}
