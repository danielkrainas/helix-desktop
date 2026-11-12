//! The InputCapture portal backend against a fake shell (its dialog says
//! yes) and a fake compositor `org.helix.InputCapture`, on a test bus.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use helix_portals::settings::Snapshot;
use helix_portals::{PortalService, BUS_NAME, OBJECT_PATH};
use helix_session::SessionService;
use helix_test_support::TestBus;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedFd, OwnedObjectPath, OwnedValue, Value};

const SESSION: &str = "/org/helix/InputCapture/Session/u1";

type Calls = Arc<Mutex<Vec<String>>>;

struct Dialogs {
    calls: Calls,
}

#[zbus::interface(name = "org.helix.PortalDialog")]
impl Dialogs {
    async fn dialog(&self, options: HashMap<String, OwnedValue>) -> (u32, HashMap<String, OwnedValue>) {
        let title = options.get("title").and_then(|v| String::try_from(v.clone()).ok()).unwrap_or_default();
        self.calls.lock().unwrap().push(format!("Dialog {title}"));
        (0, HashMap::new())
    }
}

struct Capture {
    calls: Calls,
}

#[zbus::interface(name = "org.helix.InputCapture")]
impl Capture {
    async fn create_session(&self, capabilities: u32) -> OwnedObjectPath {
        self.calls.lock().unwrap().push(format!("CreateSession {capabilities}"));
        OwnedObjectPath::try_from(SESSION).unwrap()
    }
}

struct Session {
    calls: Calls,
}

#[zbus::interface(name = "org.helix.InputCapture.Session")]
impl Session {
    async fn get_zones(&self) -> (u32, Vec<(u32, u32, i32, i32)>) {
        (4, vec![(1000, 800, 0, 0)])
    }
    async fn set_pointer_barriers(&self, barriers: Vec<(u32, (i32, i32, i32, i32))>, zone_set: u32) -> Vec<u32> {
        self.calls.lock().unwrap().push(format!("SetPointerBarriers {barriers:?} {zone_set}"));
        barriers.iter().filter(|(_, p)| p.0 != p.2 && p.1 != p.3).map(|(id, _)| *id).collect()
    }
    async fn enable(&self) {
        self.calls.lock().unwrap().push("Enable".into());
    }
    async fn release(&self, activation_id: u32, has_position: bool, position: (f64, f64)) {
        self.calls.lock().unwrap().push(format!("Release {activation_id} {has_position} {position:?}"));
    }
    #[zbus(name = "ConnectToEIS")]
    async fn connect_to_eis(&self) -> OwnedFd {
        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        OwnedFd::from(std::os::fd::OwnedFd::from(a))
    }
    async fn close(&self) {
        self.calls.lock().unwrap().push("Close".into());
    }
    async fn enable_clipboard(&self, options: HashMap<String, OwnedValue>, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) {
        self.calls.lock().unwrap().push(format!("EnableClipboard {}", options.len()));
        // The clipboard already holds something.
        let offered = HashMap::from([("mime-types".to_owned(), o(Value::from(vec!["text/plain"]))), ("session-is-owner".to_owned(), o(Value::from(false)))]);
        Self::selection_owner_changed(&emitter, offered).await.unwrap();
    }
    async fn selection_read(&self, mime_type: String) -> OwnedFd {
        self.calls.lock().unwrap().push(format!("SelectionRead {mime_type}"));
        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        OwnedFd::from(std::os::fd::OwnedFd::from(a))
    }
    #[zbus(signal)]
    async fn selection_owner_changed(emitter: &SignalEmitter<'_>, options: HashMap<String, OwnedValue>) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn activated(emitter: &SignalEmitter<'_>, activation_id: u32, barrier_id: u32, cursor_position: (f64, f64)) -> zbus::Result<()>;
}

fn o(v: Value<'_>) -> OwnedValue {
    OwnedValue::try_from(v).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn capture_session_flow() {
    let Some(bus) = TestBus::start() else { return };
    let calls: Calls = Arc::default();
    let _shell = bus.builder().serve_at("/org/helix/PortalDialog", Dialogs { calls: calls.clone() }).unwrap().name("org.gnome.Shell").unwrap().build().await.unwrap();
    let compositor = bus
        .builder()
        .serve_at("/org/helix/InputCapture", Capture { calls: calls.clone() })
        .unwrap()
        .serve_at(SESSION, Session { calls: calls.clone() })
        .unwrap()
        .name("org.helix.InputCapture")
        .unwrap()
        .build()
        .await
        .unwrap();
    let session = SessionService::start(bus.builder()).await.unwrap();
    let _session = tokio::spawn(session.run(std::future::pending()));
    let settings: Arc<Mutex<Snapshot>> = Arc::new(Mutex::new(BTreeMap::new()));
    let _portals = PortalService::start(bus.builder(), settings).await.unwrap();
    let app = bus.connect().await;
    const IFACE: &str = "org.freedesktop.impl.portal.InputCapture";
    let portal_session = "/org/freedesktop/portal/desktop/session/1_1/s1";

    // Asking for keyboard, pointer and touchscreen: touch is not offered.
    let reply = app
        .call_method(Some(BUS_NAME), OBJECT_PATH, Some(IFACE), "CreateSession", &(OwnedObjectPath::try_from("/org/freedesktop/portal/desktop/request/1_1/r1").unwrap(), OwnedObjectPath::try_from(portal_session).unwrap(), "org.example.Leap", "", HashMap::from([("capabilities", o(Value::from(7u32)))])))
        .await
        .unwrap();
    let (code, results): (u32, HashMap<String, OwnedValue>) = reply.body().deserialize().unwrap();
    assert_eq!(code, 0);
    assert_eq!(u32::try_from(results["capabilities"].clone()).unwrap(), 3);

    let reply = app.call_method(Some(BUS_NAME), OBJECT_PATH, Some(IFACE), "GetZones", &(OwnedObjectPath::try_from("/org/freedesktop/portal/desktop/request/1_1/r2").unwrap(), OwnedObjectPath::try_from(portal_session).unwrap(), "org.example.Leap", HashMap::<String, OwnedValue>::new())).await.unwrap();
    let (code, results): (u32, HashMap<String, OwnedValue>) = reply.body().deserialize().unwrap();
    assert_eq!((code, u32::try_from(results["zone_set"].clone()).unwrap()), (0, 4));
    assert_eq!(<Vec<(u32, u32, i32, i32)>>::try_from(results["zones"].clone()).unwrap(), [(1000, 800, 0, 0)]);

    let barriers = vec![
        HashMap::from([("barrier_id", o(Value::from(1u32))), ("position", o(Value::from((1000, 0, 1000, 799))))]),
        HashMap::from([("barrier_id", o(Value::from(2u32))), ("position", o(Value::from((0, 0, 10, 10))))]),
    ];
    let reply = app
        .call_method(Some(BUS_NAME), OBJECT_PATH, Some(IFACE), "SetPointerBarriers", &(OwnedObjectPath::try_from("/org/freedesktop/portal/desktop/request/1_1/r3").unwrap(), OwnedObjectPath::try_from(portal_session).unwrap(), "org.example.Leap", HashMap::<String, OwnedValue>::new(), barriers, 4u32))
        .await
        .unwrap();
    let (_, results): (u32, HashMap<String, OwnedValue>) = reply.body().deserialize().unwrap();
    assert_eq!(<Vec<u32>>::try_from(results["failed_barriers"].clone()).unwrap(), [2]);

    let reply = app.call_method(Some(BUS_NAME), OBJECT_PATH, Some(IFACE), "ConnectToEIS", &(OwnedObjectPath::try_from(portal_session).unwrap(), "org.example.Leap", HashMap::<String, OwnedValue>::new())).await.unwrap();
    let _fd: OwnedFd = reply.body().deserialize().unwrap();
    app.call_method(Some(BUS_NAME), OBJECT_PATH, Some(IFACE), "Enable", &(OwnedObjectPath::try_from(portal_session).unwrap(), "org.example.Leap", HashMap::<String, OwnedValue>::new())).await.unwrap();

    // The compositor's signal comes out as the portal's.
    let rule = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).interface(IFACE).unwrap().member("Activated").unwrap().build();
    let mut activated = zbus::MessageStream::for_match_rule(rule, &app, None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let emitter = SignalEmitter::new(&compositor, SESSION).unwrap();
    Session::activated(&emitter, 9, 1, (999.0, 300.0)).await.unwrap();
    let message = tokio::time::timeout(Duration::from_secs(5), activated.next()).await.unwrap().unwrap().unwrap();
    let (path, options): (OwnedObjectPath, HashMap<String, OwnedValue>) = message.body().deserialize().unwrap();
    assert_eq!(path.as_str(), portal_session);
    assert_eq!(u32::try_from(options["activation_id"].clone()).unwrap(), 9);
    assert_eq!(<(f64, f64)>::try_from(options["cursor_position"].clone()).unwrap(), (999.0, 300.0));

    app.call_method(Some(BUS_NAME), OBJECT_PATH, Some(IFACE), "Release", &(OwnedObjectPath::try_from(portal_session).unwrap(), "org.example.Leap", HashMap::from([("activation_id", o(Value::from(9u32))), ("cursor_position", o(Value::from((10.0f64, 20.0f64))))]))).await.unwrap();
    app.call_method(Some(BUS_NAME), portal_session, Some("org.freedesktop.impl.portal.Session"), "Close", &()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        *calls.lock().unwrap(),
        [
            "Dialog Allow Input Capture?",
            "CreateSession 3",
            "SetPointerBarriers [(1, (1000, 0, 1000, 799)), (2, (0, 0, 10, 10))] 4",
            "Enable",
            "Release 9 true (10.0, 20.0)",
            "Close",
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn version_two_remembers() {
    let Some(bus) = TestBus::start() else { return };
    let calls: Calls = Arc::default();
    let _shell = bus.builder().serve_at("/org/helix/PortalDialog", Dialogs { calls: calls.clone() }).unwrap().name("org.gnome.Shell").unwrap().build().await.unwrap();
    let _compositor = bus
        .builder()
        .serve_at("/org/helix/InputCapture", Capture { calls: calls.clone() })
        .unwrap()
        .serve_at(SESSION, Session { calls: calls.clone() })
        .unwrap()
        .name("org.helix.InputCapture")
        .unwrap()
        .build()
        .await
        .unwrap();
    let session = SessionService::start(bus.builder()).await.unwrap();
    let _session = tokio::spawn(session.run(std::future::pending()));
    let settings: Arc<Mutex<Snapshot>> = Arc::new(Mutex::new(BTreeMap::new()));
    let _portals = PortalService::start(bus.builder(), settings).await.unwrap();
    let app = bus.connect().await;
    const IFACE: &str = "org.freedesktop.impl.portal.InputCapture";
    let version: u32 = app.call_method(Some(BUS_NAME), OBJECT_PATH, Some("org.freedesktop.DBus.Properties"), "Get", &(IFACE, "version")).await.unwrap().body().deserialize::<OwnedValue>().unwrap().try_into().unwrap();
    assert_eq!(version, 2);

    let start = |n: u32, options: HashMap<&'static str, OwnedValue>| {
        let app = app.clone();
        async move {
            let s = OwnedObjectPath::try_from(format!("/org/freedesktop/portal/desktop/session/1_1/v{n}")).unwrap();
            app.call_method(Some(BUS_NAME), OBJECT_PATH, Some(IFACE), "CreateSession2", &(s.clone(), "org.example.Leap", HashMap::<String, OwnedValue>::new())).await.unwrap();
            let r = OwnedObjectPath::try_from(format!("/org/freedesktop/portal/desktop/request/1_1/q{n}")).unwrap();
            let reply = app.call_method(Some(BUS_NAME), OBJECT_PATH, Some(IFACE), "Start", &(r, s, "org.example.Leap", "", options)).await.unwrap();
            reply.body().deserialize::<(u32, HashMap<String, OwnedValue>)>().unwrap()
        }
    };
    let (code, results) = start(1, HashMap::from([("capabilities", o(Value::from(3u32))), ("persist_mode", o(Value::from(2u32)))])).await;
    assert_eq!(code, 0);
    assert!(!bool::try_from(results["clipboard_enabled"].clone()).unwrap());
    let restore = results["restore_data"].clone();
    assert_eq!(calls.lock().unwrap().iter().filter(|c| c.starts_with("Dialog")).count(), 1);

    // With the restore data the user is not asked again.
    let (code, _) = start(2, HashMap::from([("capabilities", o(Value::from(3u32))), ("restore_data", restore)])).await;
    assert_eq!(code, 0);
    assert_eq!(calls.lock().unwrap().iter().filter(|c| c.starts_with("Dialog")).count(), 1);
    assert_eq!(calls.lock().unwrap().iter().filter(|c| c.starts_with("CreateSession")).count(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn clipboard_shared() {
    let Some(bus) = TestBus::start() else { return };
    let calls: Calls = Arc::default();
    let _shell = bus.builder().serve_at("/org/helix/PortalDialog", Dialogs { calls: calls.clone() }).unwrap().name("org.gnome.Shell").unwrap().build().await.unwrap();
    let _compositor = bus
        .builder()
        .serve_at("/org/helix/InputCapture", Capture { calls: calls.clone() })
        .unwrap()
        .serve_at(SESSION, Session { calls: calls.clone() })
        .unwrap()
        .name("org.helix.InputCapture")
        .unwrap()
        .build()
        .await
        .unwrap();
    let session = SessionService::start(bus.builder()).await.unwrap();
    let _session = tokio::spawn(session.run(std::future::pending()));
    let settings: Arc<Mutex<Snapshot>> = Arc::new(Mutex::new(BTreeMap::new()));
    let _portals = PortalService::start(bus.builder(), settings).await.unwrap();
    let app = bus.connect().await;
    const IFACE: &str = "org.freedesktop.impl.portal.InputCapture";
    const CLIP: &str = "org.freedesktop.impl.portal.Clipboard";
    let s = OwnedObjectPath::try_from("/org/freedesktop/portal/desktop/session/1_1/c1").unwrap();
    let none = HashMap::<String, OwnedValue>::new();
    // Not a session yet.
    assert!(app.call_method(Some(BUS_NAME), OBJECT_PATH, Some(CLIP), "RequestClipboard", &(s.clone(), none.clone())).await.is_err());
    app.call_method(Some(BUS_NAME), OBJECT_PATH, Some(IFACE), "CreateSession2", &(s.clone(), "org.example.Leap", none.clone())).await.unwrap();
    app.call_method(Some(BUS_NAME), OBJECT_PATH, Some(CLIP), "RequestClipboard", &(s.clone(), none.clone())).await.unwrap();
    let rule = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).interface(CLIP).unwrap().member("SelectionOwnerChanged").unwrap().build();
    let mut owner = zbus::MessageStream::for_match_rule(rule, &app, None).await.unwrap();
    let r = OwnedObjectPath::try_from("/org/freedesktop/portal/desktop/request/1_1/c1").unwrap();
    let options = HashMap::from([("capabilities", o(Value::from(3u32)))]);
    let reply = app.call_method(Some(BUS_NAME), OBJECT_PATH, Some(IFACE), "Start", &(r, s.clone(), "org.example.Leap", "", options)).await.unwrap();
    let (code, results) = reply.body().deserialize::<(u32, HashMap<String, OwnedValue>)>().unwrap();
    assert_eq!(code, 0);
    assert!(bool::try_from(results["clipboard_enabled"].clone()).unwrap());
    assert!(calls.lock().unwrap().contains(&"EnableClipboard 0".to_owned()));

    // The compositor's news reaches the app, for its session.
    let msg = tokio::time::timeout(Duration::from_secs(5), owner.next()).await.unwrap().unwrap().unwrap();
    let (session_handle, options) = msg.body().deserialize::<(OwnedObjectPath, HashMap<String, OwnedValue>)>().unwrap();
    assert_eq!(session_handle, s);
    assert_eq!(Vec::<String>::try_from(options["mime_types"].clone()).unwrap(), ["text/plain"]);

    // Reading goes to the compositor's session.
    let fd = app.call_method(Some(BUS_NAME), OBJECT_PATH, Some(CLIP), "SelectionRead", &(s.clone(), "text/plain")).await.unwrap();
    fd.body().deserialize::<OwnedFd>().unwrap();
    assert!(calls.lock().unwrap().contains(&"SelectionRead text/plain".to_owned()));
    // Too late to ask once started.
    assert!(app.call_method(Some(BUS_NAME), OBJECT_PATH, Some(CLIP), "RequestClipboard", &(s, none)).await.is_err());
}
