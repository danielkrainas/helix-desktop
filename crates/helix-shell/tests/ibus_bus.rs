//! The IBus side of the input method, against a fake IBus on a test bus.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use helix_shell::ibus::{make_text, ImEvent, ImRequest, Ibus, RELEASE_MASK};
use helix_shell::UiEvent;
use helix_test_support::TestBus;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

const IC: &str = "/org/freedesktop/IBus/InputContext_1";

struct Bus {
    calls: Arc<Mutex<Vec<String>>>,
}

#[zbus::interface(name = "org.freedesktop.IBus")]
impl Bus {
    async fn create_input_context(&self, client_name: String) -> OwnedObjectPath {
        self.calls.lock().unwrap().push(format!("CreateInputContext {client_name}"));
        OwnedObjectPath::try_from(IC).unwrap()
    }
    async fn set_global_engine(&self, engine_name: String) {
        self.calls.lock().unwrap().push(format!("SetGlobalEngine {engine_name}"));
    }
}

struct Context {
    calls: Arc<Mutex<Vec<String>>>,
}

#[zbus::interface(name = "org.freedesktop.IBus.InputContext")]
impl Context {
    async fn set_capabilities(&self, caps: u32) {
        self.calls.lock().unwrap().push(format!("SetCapabilities {caps}"));
    }
    async fn focus_in(&self) {
        self.calls.lock().unwrap().push("FocusIn".into());
    }
    async fn focus_out(&self) {
        self.calls.lock().unwrap().push("FocusOut".into());
    }
    async fn reset(&self) {
        self.calls.lock().unwrap().push("Reset".into());
    }
    async fn process_key_event(&self, keyval: u32, keycode: u32, state: u32) -> bool {
        self.calls.lock().unwrap().push(format!("ProcessKeyEvent {keyval:#x} {keycode} {state:#x}"));
        keyval == 0x61 // the engine takes 'a'
    }
    #[zbus(signal)]
    async fn commit_text(emitter: &SignalEmitter<'_>, text: zbus::zvariant::Value<'_>) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn update_preedit_text(emitter: &SignalEmitter<'_>, text: zbus::zvariant::Value<'_>, cursor_pos: u32, visible: bool) -> zbus::Result<()>;
}

async fn next_ime(ui: &mut tokio::sync::mpsc::UnboundedReceiver<UiEvent>) -> ImEvent {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), ui.recv()).await.unwrap().unwrap();
        if let UiEvent::Ime(event) = event {
            return event;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn keys_text_and_engines() {
    let Some(bus) = TestBus::start() else { return };
    let calls = Arc::new(Mutex::new(Vec::new()));
    let ibus_conn = bus
        .builder()
        .serve_at("/org/freedesktop/IBus", Bus { calls: calls.clone() })
        .unwrap()
        .serve_at(IC, Context { calls: calls.clone() })
        .unwrap()
        .name("org.freedesktop.IBus")
        .unwrap()
        .build()
        .await
        .unwrap();
    let (ui, mut ui_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut ibus = Ibus::with_connection(ui, bus.connect().await);

    // An xkb source: keys are not IBus's.
    ibus.handle(ImRequest::Key { seq: 1, keyval: 0x61, keycode: 30, state: 0 }).await;
    assert_eq!(next_ime(&mut ui_rx).await, ImEvent::KeyHandled { seq: 1, handled: false });
    assert!(calls.lock().unwrap().is_empty());

    ibus.handle(ImRequest::Engine(Some("table:cangjie5".into()))).await;
    ibus.handle(ImRequest::FocusIn).await;
    ibus.handle(ImRequest::Key { seq: 2, keyval: 0x61, keycode: 30, state: 0 }).await;
    assert_eq!(next_ime(&mut ui_rx).await, ImEvent::KeyHandled { seq: 2, handled: true });
    ibus.handle(ImRequest::Key { seq: 3, keyval: 0x62, keycode: 48, state: RELEASE_MASK }).await;
    assert_eq!(next_ime(&mut ui_rx).await, ImEvent::KeyHandled { seq: 3, handled: false });
    assert_eq!(
        *calls.lock().unwrap(),
        [
            "SetGlobalEngine table:cangjie5",
            "CreateInputContext helix-shell",
            "SetCapabilities 45",
            "FocusIn",
            "ProcessKeyEvent 0x61 30 0x0",
            "ProcessKeyEvent 0x62 48 0x40000000",
        ]
    );

    // Text from the engine reaches the input method.
    let emitter = SignalEmitter::new(&ibus_conn, IC).unwrap();
    Context::update_preedit_text(&emitter, make_text("日"), 1, true).await.unwrap();
    assert_eq!(next_ime(&mut ui_rx).await, ImEvent::Preedit { text: "日".into(), cursor: 1 });
    Context::commit_text(&emitter, make_text("日本")).await.unwrap();
    assert_eq!(next_ime(&mut ui_rx).await, ImEvent::Commit("日本".into()));

    ibus.handle(ImRequest::FocusOut).await;
    assert!(calls.lock().unwrap().ends_with(&["Reset".to_owned(), "FocusOut".to_owned()]));
    let _ = OwnedValue::from(0u32);
}
