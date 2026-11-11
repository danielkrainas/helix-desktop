//! The RemoteDesktop portal backend against the compositor's real
//! `org.gnome.Mutter.RemoteDesktop` / `ScreenCast` (with a stand-in for the
//! compositor) and a stand-in shell chooser.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use helix_comp::display_config::{Mode, Monitor, Store, ALL_TRANSFORMS};
use helix_comp::remote_desktop::RemoteInput;
use helix_comp::screencast::CastRequest;
use helix_comp::{CompositorServices, MonitorControl};
use helix_portals::settings::Snapshot;
use helix_portals::{PortalService, BUS_NAME, OBJECT_PATH};
use helix_session::SessionService;
use helix_test_support::TestBus;
use tokio::sync::mpsc;
use zbus::zvariant::{ObjectPath, OwnedValue, Value};
use zbus::Connection;

/// What the stand-in shell was asked: ("control", devices) or
/// ("share", control devices).
type Asked = Arc<Mutex<Vec<(String, Vec<String>)>>>;

struct Chooser {
    asked: Asked,
}

#[zbus::interface(name = "org.helix.ScreenCastChooser")]
impl Chooser {
    async fn choose_source(&self, _app_id: String, _monitors: Vec<(String, String)>, options: HashMap<String, OwnedValue>) -> (bool, u32, String, u32) {
        let control = options.get("control").and_then(|v| Vec::<String>::try_from(v.clone()).ok()).unwrap_or_default();
        self.asked.lock().unwrap().push(("share".into(), control));
        (true, 1, "DP-1".into(), 0)
    }

    async fn allow_remote_control(&self, _app_id: String, devices: Vec<String>) -> bool {
        self.asked.lock().unwrap().push(("control".into(), devices));
        true
    }
}

fn monitor(connector: &str) -> Monitor {
    Monitor {
        connector: connector.into(),
        vendor: "Acme".into(),
        product: "Panel".into(),
        serial: "1".into(),
        display_name: "Acme 24\"".into(),
        builtin: false,
        modes: vec![Mode { width: 1920, height: 1080, refresh_mhz: 60_000, preferred: true, current: true }],
        x: 0,
        y: 0,
        scale: 1.0,
        transform: 0,
        primary: true,
        transforms: ALL_TRANSFORMS.to_vec(),
    }
}

/// A stand-in compositor: starts streams with node 42, logs the rest, and
/// hands out a socket for EIS.
fn fake_compositor(log: Arc<Mutex<Vec<String>>>) -> mpsc::UnboundedSender<CastRequest> {
    let (tx, mut rx) = mpsc::unbounded_channel::<CastRequest>();
    tokio::spawn(async move {
        while let Some(request) = rx.recv().await {
            match request {
                CastRequest::Start { stream, source, reply, .. } => {
                    log.lock().unwrap().push(format!("start {stream} {source:?}"));
                    let _ = reply.send(Ok(42));
                }
                CastRequest::Input(input) => log.lock().unwrap().push(format!("{input:?}")),
                CastRequest::ConnectEis { devices, reply, .. } => {
                    log.lock().unwrap().push(format!("eis {devices}"));
                    let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
                    let _ = reply.send(Ok(a.into()));
                }
                CastRequest::EndRemote { session } => log.lock().unwrap().push(format!("end remote {session}")),
                CastRequest::Stop { stream } => log.lock().unwrap().push(format!("stop {stream}")),
                CastRequest::LockKeys { reply } => {
                    let _ = reply.send((true, false));
                }
                CastRequest::WindowSize { reply, .. } => {
                    let _ = reply.send(None);
                }
                CastRequest::CreateVirtual { reply, .. } => {
                    let _ = reply.send(Err("none".into()));
                }
            }
        }
    });
    tx
}

async fn call(conn: &Connection, iface: &str, method: &str, body: &(impl serde::Serialize + zbus::zvariant::DynamicType)) -> (u32, HashMap<String, OwnedValue>) {
    let reply = conn.call_method(Some(BUS_NAME), OBJECT_PATH, Some(iface), method, body).await.unwrap();
    reply.body().deserialize().unwrap()
}

const RD: &str = "org.freedesktop.impl.portal.RemoteDesktop";
const SC: &str = "org.freedesktop.impl.portal.ScreenCast";

#[tokio::test(flavor = "multi_thread")]
async fn remote_control() {
    let Some(bus) = TestBus::start() else { return };
    let session = SessionService::start(bus.builder()).await.unwrap();
    let _session = tokio::spawn(session.run(std::future::pending()));
    let log = Arc::new(Mutex::new(Vec::new()));
    let store = std::env::temp_dir().join(format!("helix-rd-{}.toml", std::process::id()));
    let control = MonitorControl { store: Store::at(store), cast: Some(fake_compositor(log.clone())), ..MonitorControl::default() };
    let services = CompositorServices::start_with(bus.builder(), vec![monitor("DP-1")], control).await.unwrap();
    let (_events, rx) = mpsc::unbounded_channel();
    tokio::spawn(services.run(rx));
    let asked: Asked = Arc::default();
    let _shell = bus
        .builder()
        .serve_at("/org/helix/ScreenCastChooser", Chooser { asked: asked.clone() })
        .unwrap()
        .name("org.gnome.Shell")
        .unwrap()
        .build()
        .await
        .unwrap();
    let settings: Arc<Mutex<Snapshot>> = Arc::new(Mutex::new(BTreeMap::new()));
    let _portals = PortalService::start(bus.builder(), settings).await.unwrap();
    let conn = bus.connect().await;
    let request = ObjectPath::try_from("/org/freedesktop/portal/desktop/request/1_1/r").unwrap();
    let none: HashMap<&str, Value<'_>> = HashMap::new();

    // Keyboard and pointer, no screen: the shell asks about control only.
    let s1 = ObjectPath::try_from("/org/freedesktop/portal/desktop/session/1_1/rd1").unwrap();
    assert_eq!(call(&conn, RD, "CreateSession", &(&request, &s1, "org.example.Remote", none.clone())).await.0, 0);
    let options = HashMap::from([("types", Value::from(3u32))]);
    assert_eq!(call(&conn, RD, "SelectDevices", &(&request, &s1, "org.example.Remote", options)).await.0, 0);
    let (r, results) = call(&conn, RD, "Start", &(&request, &s1, "org.example.Remote", "", none.clone())).await;
    assert_eq!(r, 0);
    assert_eq!(u32::try_from(results["devices"].clone()).unwrap(), 3);
    assert!(!bool::try_from(results["clipboard_enabled"].clone()).unwrap());
    assert_eq!(asked.lock().unwrap()[0], ("control".into(), vec!["keyboard".to_owned(), "pointer".to_owned()]));

    let empty: HashMap<&str, Value<'_>> = HashMap::new();
    conn.call_method(Some(BUS_NAME), OBJECT_PATH, Some(RD), "NotifyPointerMotion", &(&s1, empty.clone(), 5.0f64, -2.0f64)).await.unwrap();
    conn.call_method(Some(BUS_NAME), OBJECT_PATH, Some(RD), "NotifyKeyboardKeycode", &(&s1, empty.clone(), 30i32, 1u32)).await.unwrap();
    // Touch was not asked for: ignored.
    conn.call_method(Some(BUS_NAME), OBJECT_PATH, Some(RD), "NotifyTouchUp", &(&s1, empty.clone(), 0u32)).await.unwrap();
    let fd = conn.call_method(Some(BUS_NAME), OBJECT_PATH, Some(RD), "ConnectToEIS", &(&s1, "org.example.Remote", empty.clone())).await.unwrap();
    let _fd: zbus::zvariant::OwnedFd = fd.body().deserialize().unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    {
        let log = log.lock().unwrap();
        assert!(log.contains(&format!("{:?}", RemoteInput::Motion { dx: 5.0, dy: -2.0 })), "{log:?}");
        assert!(log.contains(&format!("{:?}", RemoteInput::Key { keycode: 30, pressed: true })));
        assert!(!log.iter().any(|l| l.starts_with("TouchUp")), "{log:?}");
        assert!(log.contains(&"eis 3".to_owned()), "{log:?}");
    }

    // With a screen: one dialog for both; absolute positions on its stream.
    let s2 = ObjectPath::try_from("/org/freedesktop/portal/desktop/session/1_1/rd2").unwrap();
    assert_eq!(call(&conn, RD, "CreateSession", &(&request, &s2, "org.example.Remote", none.clone())).await.0, 0);
    let options = HashMap::from([("types", Value::from(1u32))]);
    assert_eq!(call(&conn, SC, "SelectSources", &(&request, &s2, "org.example.Remote", options)).await.0, 0);
    let (r, results) = call(&conn, RD, "Start", &(&request, &s2, "org.example.Remote", "", none.clone())).await;
    assert_eq!(r, 0);
    let streams = <Vec<(u32, HashMap<String, OwnedValue>)>>::try_from(results["streams"].clone()).unwrap();
    assert_eq!(streams[0].0, 42);
    assert_eq!(asked.lock().unwrap()[1], ("share".into(), vec!["keyboard".to_owned(), "pointer".to_owned(), "touchscreen".to_owned()]));
    conn.call_method(Some(BUS_NAME), OBJECT_PATH, Some(RD), "NotifyPointerMotionAbsolute", &(&s2, empty.clone(), 42u32, 10.0f64, 20.0f64))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(log.lock().unwrap().iter().any(|l| l.starts_with("MotionAbsolute") && l.ends_with("x: 10.0, y: 20.0 }")), "{:?}", log.lock().unwrap());

    // Closing the portal session ends the remote session and its cast.
    conn.call_method(Some(BUS_NAME), s2.as_str(), Some("org.freedesktop.impl.portal.Session"), "Close", &()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(log.lock().unwrap().iter().any(|l| l.starts_with("end remote")));
    assert!(log.lock().unwrap().iter().any(|l| l.starts_with("stop")));

    // Only the portal may use the compositor's interface.
    let stranger = bus.connect().await;
    let denied = stranger
        .call_method(Some("org.gnome.Mutter.RemoteDesktop"), "/org/gnome/Mutter/RemoteDesktop", Some("org.gnome.Mutter.RemoteDesktop"), "CreateSession", &())
        .await;
    assert!(matches!(denied, Err(zbus::Error::MethodError(name, _, _)) if name.as_str() == "org.freedesktop.DBus.Error.AccessDenied"));
}
