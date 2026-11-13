//! The end-session dialog and offline updates (helix-desktop-spec
//! `org.gnome.SessionManager.EndSessionDialog.md`, "Updates"): a fake
//! PackageKit on a second test bus standing in for the system bus.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use helix_shell::end_dialog::{Choice, EndDialog, OBJECT_PATH, REBOOT, SHUTDOWN, UPDATE_REBOOT, UPGRADE_REBOOT};
use helix_shell::{ShellService, UiEvent, UiRequest};
use helix_test_support::TestBus;
use tokio::sync::mpsc::UnboundedReceiver;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

#[derive(Default)]
struct State {
    update_prepared: bool,
    update_triggered: bool,
    upgrade: Option<(String, String)>,
    fail: bool,
    calls: Vec<String>,
}

struct Offline(Arc<Mutex<State>>);

#[zbus::interface(name = "org.freedesktop.PackageKit.Offline")]
impl Offline {
    #[zbus(property)]
    fn update_prepared(&self) -> bool {
        self.0.lock().unwrap().update_prepared
    }
    #[zbus(property)]
    fn update_triggered(&self) -> bool {
        self.0.lock().unwrap().update_triggered
    }
    #[zbus(property)]
    fn upgrade_prepared(&self) -> bool {
        self.0.lock().unwrap().upgrade.is_some()
    }
    #[zbus(property)]
    fn upgrade_triggered(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn prepared_upgrade(&self) -> HashMap<String, OwnedValue> {
        let s = |v: &str| OwnedValue::try_from(Value::from(v.to_owned())).unwrap();
        self.0.lock().unwrap().upgrade.as_ref().map(|(n, v)| HashMap::from([("name".to_owned(), s(n)), ("version".to_owned(), s(v))])).unwrap_or_default()
    }
    #[zbus(property)]
    fn trigger_action(&self) -> String {
        "unset".into()
    }
    fn trigger(&self, action: String) -> zbus::fdo::Result<()> {
        self.record(format!("Trigger {action}"))
    }
    fn trigger_upgrade(&self, action: String) -> zbus::fdo::Result<()> {
        self.record(format!("TriggerUpgrade {action}"))
    }
    fn cancel(&self) -> zbus::fdo::Result<()> {
        self.record("Cancel".into())
    }
}

impl Offline {
    fn record(&self, call: String) -> zbus::fdo::Result<()> {
        let mut state = self.0.lock().unwrap();
        if state.fail {
            return Err(zbus::fdo::Error::AccessDenied("not allowed".into()));
        }
        state.calls.push(call);
        Ok(())
    }
}

async fn next_dialog(ui: &mut UnboundedReceiver<UiEvent>) -> Option<EndDialog> {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), ui.recv()).await.expect("ui event").unwrap();
        // Closings of the dialog before are skipped.
        if let UiEvent::EndDialog(Some(d)) = event {
            return Some(d);
        }
    }
}

/// The dialog's next answer (not `Closed`).
async fn answered(signals: &mut zbus::MessageStream) -> String {
    loop {
        let m = tokio::time::timeout(Duration::from_secs(5), signals.next()).await.expect("signal").unwrap().unwrap();
        let name = m.header().member().map(|m| m.to_string()).unwrap_or_default();
        if name != "Closed" {
            return name;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn updates_installed_on_restart_and_power_off() {
    let (Some(bus), Some(system)) = (TestBus::start(), TestBus::start()) else { return };
    // Only this test runs in this binary: the shell's system bus is the fake's.
    unsafe {
        std::env::set_var("DBUS_SYSTEM_BUS_ADDRESS", system.address());
        std::env::set_var("HELIX_TEST_PACKAGEKIT", "1");
    }
    let state = Arc::new(Mutex::new(State::default()));
    let _packagekit = system
        .builder()
        .name("org.freedesktop.PackageKit")
        .unwrap()
        .serve_at("/org/freedesktop/PackageKit", Offline(state.clone()))
        .unwrap()
        .build()
        .await
        .unwrap();
    let (shell, mut ui) = ShellService::start(bus.builder()).await.unwrap();
    let bridge = shell.ui_bridge();
    let conn = bus.connect().await;
    let rule = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).interface("org.gnome.SessionManager.EndSessionDialog").unwrap().build();
    let mut signals = zbus::MessageStream::for_match_rule(rule, &conn, None).await.unwrap();
    let open = |kind: u32| {
        let conn = conn.clone();
        async move {
            conn.call_method(Some("org.gnome.Shell"), OBJECT_PATH, Some("org.gnome.SessionManager.EndSessionDialog"), "Open", &(kind, 0u32, 60u32, Vec::<OwnedObjectPath>::new()))
                .await
                .unwrap();
        }
    };
    let calls = |state: &Arc<Mutex<State>>| std::mem::take(&mut state.lock().unwrap().calls);

    // No updates: a plain restart, no check box; asking for an update
    // restart gives a plain one too.
    open(REBOOT).await;
    let d = next_dialog(&mut ui).await.unwrap();
    assert_eq!((d.kind, d.install), (REBOOT, None));
    bridge.handle(UiRequest::EndDialogAnswer(Choice::Confirmed, d.install)).await.unwrap();
    assert_eq!(answered(&mut signals).await, "ConfirmedReboot");
    open(UPDATE_REBOOT).await;
    assert_eq!(next_dialog(&mut ui).await.unwrap().kind, REBOOT);
    bridge.handle(UiRequest::EndDialogAnswer(Choice::Canceled, None)).await.unwrap();
    assert_eq!(answered(&mut signals).await, "Canceled");
    assert!(calls(&state).is_empty());

    // Updates ready: restart with the box ticked installs them.
    state.lock().unwrap().update_prepared = true;
    open(REBOOT).await;
    let d = next_dialog(&mut ui).await.unwrap();
    assert_eq!(d.install, Some(true));
    bridge.handle(UiRequest::EndDialogAnswer(Choice::Confirmed, Some(true))).await.unwrap();
    assert_eq!(answered(&mut signals).await, "ConfirmedReboot");
    assert_eq!(calls(&state), ["Trigger reboot"]);

    // Already set to install; powering off with the box unticked cancels.
    state.lock().unwrap().update_triggered = true;
    open(SHUTDOWN).await;
    assert_eq!(next_dialog(&mut ui).await.unwrap().install, Some(true));
    bridge.handle(UiRequest::EndDialogAnswer(Choice::Confirmed, Some(false))).await.unwrap();
    assert_eq!(answered(&mut signals).await, "ConfirmedShutdown");
    assert_eq!(calls(&state), ["Cancel"]);

    // Cancelling the dialog changes nothing.
    open(SHUTDOWN).await;
    next_dialog(&mut ui).await.unwrap();
    bridge.handle(UiRequest::EndDialogAnswer(Choice::Canceled, Some(true))).await.unwrap();
    assert_eq!(answered(&mut signals).await, "Canceled");
    assert!(calls(&state).is_empty());

    // "Restart & Install Updates".
    open(UPDATE_REBOOT).await;
    let d = next_dialog(&mut ui).await.unwrap();
    assert_eq!((d.kind, d.install), (UPDATE_REBOOT, None));
    bridge.handle(UiRequest::EndDialogAnswer(Choice::Confirmed, None)).await.unwrap();
    assert_eq!(answered(&mut signals).await, "ConfirmedReboot");
    assert_eq!(calls(&state), ["Trigger reboot"]);

    // "Restart & Install Upgrade".
    state.lock().unwrap().upgrade = Some(("Fedora".into(), "42".into()));
    open(UPGRADE_REBOOT).await;
    let d = next_dialog(&mut ui).await.unwrap();
    assert_eq!((d.kind, d.upgrade.as_deref()), (UPGRADE_REBOOT, Some("Fedora 42")));
    bridge.handle(UiRequest::EndDialogAnswer(Choice::Confirmed, None)).await.unwrap();
    assert_eq!(answered(&mut signals).await, "ConfirmedReboot");
    assert_eq!(calls(&state), ["TriggerUpgrade reboot"]);

    // PackageKit refuses: nothing ends.
    state.lock().unwrap().fail = true;
    open(UPDATE_REBOOT).await;
    next_dialog(&mut ui).await.unwrap();
    bridge.handle(UiRequest::EndDialogAnswer(Choice::Confirmed, None)).await.unwrap();
    assert_eq!(answered(&mut signals).await, "Canceled");
}
