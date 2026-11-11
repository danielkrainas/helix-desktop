//! The ScreenCast portal backend against the compositor's real
//! `org.gnome.Mutter.ScreenCast` (with a stand-in for the recording) and a
//! stand-in shell chooser.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use helix_comp::display_config::{Mode, Monitor, Store, ALL_TRANSFORMS};
use helix_comp::screencast::CastRequest;
use helix_comp::{CompositorServices, Event, MonitorControl};
use helix_portals::settings::Snapshot;
use helix_portals::{PortalService, BUS_NAME, OBJECT_PATH};
use helix_session::SessionService;
use helix_test_support::TestBus;
use tokio::sync::mpsc;
use zbus::zvariant::{ObjectPath, OwnedValue, Value};
use zbus::Connection;

/// (app id, monitors offered, windows offered, remember)
type Asked = (String, Vec<(String, String)>, bool, bool);

/// (chosen, source type, connector, window)
type Answer = (bool, u32, String, u32);

struct Chooser {
    answers: Arc<Mutex<Vec<Answer>>>,
    asked: Arc<Mutex<Vec<Asked>>>,
}

#[zbus::interface(name = "org.helix.ScreenCastChooser")]
impl Chooser {
    async fn choose_source(
        &self,
        app_id: String,
        monitors: Vec<(String, String)>,
        options: HashMap<String, OwnedValue>,
    ) -> (bool, u32, String, u32) {
        let flag = |k: &str| options.get(k).and_then(|v| bool::try_from(v.clone()).ok()).unwrap_or(false);
        self.asked.lock().unwrap().push((app_id, monitors, flag("windows"), flag("remember")));
        self.answers.lock().unwrap().remove(0)
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

/// Answers starts with node 42; records starts and stops.
fn fake_recorder(log: Arc<Mutex<Vec<String>>>) -> mpsc::UnboundedSender<CastRequest> {
    let (tx, mut rx) = mpsc::unbounded_channel::<CastRequest>();
    tokio::spawn(async move {
        while let Some(request) = rx.recv().await {
            match request {
                CastRequest::Start { stream, source, cursor, reply } => {
                    log.lock().unwrap().push(format!("start {stream} {source:?} cursor={cursor:?}"));
                    let _ = reply.send(Ok(42));
                }
                CastRequest::WindowSize { window, reply } => {
                    let _ = reply.send((window == 7).then_some((800, 600)));
                }
                CastRequest::Stop { stream } => log.lock().unwrap().push(format!("stop {stream}")),
                CastRequest::Input(input) => log.lock().unwrap().push(format!("input {input:?}")),
                CastRequest::ConnectEis { reply, .. } => {
                    let _ = reply.send(Err("no EIS here".into()));
                }
                CastRequest::EndRemote { session } => log.lock().unwrap().push(format!("end remote {session}")),
                CastRequest::LockKeys { reply } => {
                    let _ = reply.send((false, false));
                }
                CastRequest::CreateVirtual { stream, reply } => {
                    log.lock().unwrap().push(format!("virtual {stream}"));
                    let _ = reply.send(Ok(("Virtual-1".into(), (1920, 0), (1920, 1080))));
                }
            }
        }
    });
    tx
}

async fn call(conn: &Connection, method: &str, body: &(impl serde::Serialize + zbus::zvariant::DynamicType)) -> (u32, HashMap<String, OwnedValue>) {
    let reply = conn
        .call_method(Some(BUS_NAME), OBJECT_PATH, Some("org.freedesktop.impl.portal.ScreenCast"), method, body)
        .await
        .unwrap();
    reply.body().deserialize().unwrap()
}

async fn sharing(conn: &Connection) -> bool {
    zbus::Proxy::new(conn, "org.gnome.Mutter.ScreenCast", "/org/gnome/Mutter/ScreenCast", "org.helix.ScreenCastState")
        .await
        .unwrap()
        .get_property::<bool>("Active")
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn share_a_monitor() {
    let Some(bus) = TestBus::start() else { return };
    let session = SessionService::start(bus.builder()).await.unwrap();
    let _session = tokio::spawn(session.run(std::future::pending()));
    let log = Arc::new(Mutex::new(Vec::new()));
    let store = std::env::temp_dir().join(format!("helix-cast-{}.toml", std::process::id()));
    let control = MonitorControl { store: Store::at(store), cast: Some(fake_recorder(log.clone())), ..MonitorControl::default() };
    let services = CompositorServices::start_with(bus.builder(), vec![monitor("DP-1")], control).await.unwrap();
    let (events, rx) = mpsc::unbounded_channel();
    tokio::spawn(services.run(rx));
    let dp1 = (true, 1, "DP-1".to_owned(), 0);
    let answers = Arc::new(Mutex::new(vec![
        (false, 0, String::new(), 0),
        dp1.clone(),
        dp1.clone(),
        (true, 2, String::new(), 7),
        dp1,
    ]));
    let asked = Arc::new(Mutex::new(Vec::new()));
    let _shell = bus
        .builder()
        .serve_at("/org/helix/ScreenCastChooser", Chooser { answers, asked: asked.clone() })
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
    let start_session = |n: u32| {
        let conn = conn.clone();
        let request = request.clone();
        async move {
            let session = ObjectPath::try_from(format!("/org/freedesktop/portal/desktop/session/1_1/s{n}")).unwrap();
            let none: HashMap<&str, Value<'_>> = HashMap::new();
            let (r, results) = call(&conn, "CreateSession", &(&request, &session, "org.example.Meet", none)).await;
            assert_eq!(r, 0);
            assert_eq!(String::try_from(results["session_id"].clone()).unwrap(), session.as_str());
            let options = HashMap::from([("types", Value::from(1u32)), ("cursor_mode", Value::from(2u32))]);
            assert_eq!(call(&conn, "SelectSources", &(&request, &session, "org.example.Meet", options)).await.0, 0);
            session
        }
    };

    // The user says no.
    let s1 = start_session(1).await;
    let none: HashMap<&str, Value<'_>> = HashMap::new();
    assert_eq!(call(&conn, "Start", &(&request, &s1, "org.example.Meet", "", none.clone())).await.0, 1);
    assert_eq!(
        asked.lock().unwrap()[0],
        ("org.example.Meet".to_owned(), vec![("DP-1".to_owned(), "Acme 24\"".to_owned())], false, false)
    );

    // The user picks DP-1: one stream, node 42.
    let s2 = start_session(2).await;
    let (r, results) = call(&conn, "Start", &(&request, &s2, "org.example.Meet", "", none.clone())).await;
    assert_eq!(r, 0);
    let streams = <Vec<(u32, HashMap<String, OwnedValue>)>>::try_from(results["streams"].clone()).unwrap();
    assert_eq!(streams.len(), 1);
    let (node, props) = &streams[0];
    assert_eq!(*node, 42);
    assert_eq!(<(i32, i32)>::try_from(props["size"].clone()).unwrap(), (1920, 1080));
    assert_eq!(<(i32, i32)>::try_from(props["position"].clone()).unwrap(), (0, 0));
    assert_eq!(u32::try_from(props["source_type"].clone()).unwrap(), 1);
    assert!(sharing(&conn).await);
    assert!(log.lock().unwrap()[0].ends_with("Monitor(\"DP-1\") cursor=Embedded"), "{:?}", log.lock().unwrap());

    // The frontend closes the session: the recording stops.
    conn.call_method(Some(BUS_NAME), s2.as_str(), Some("org.freedesktop.impl.portal.Session"), "Close", &()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(log.lock().unwrap().iter().any(|l| l.starts_with("stop")));
    assert!(!sharing(&conn).await);

    // The compositor ends a recording: the portal session says Closed.
    let s3 = start_session(3).await;
    let closed_rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.impl.portal.Session")
        .unwrap()
        .member("Closed")
        .unwrap()
        .build();
    let mut closed = zbus::MessageStream::for_match_rule(closed_rule, &conn, None).await.unwrap();
    assert_eq!(call(&conn, "Start", &(&request, &s3, "org.example.Meet", "", none)).await.0, 0);
    let stream_id: u32 = log
        .lock()
        .unwrap()
        .last()
        .and_then(|l| l.split(' ').nth(1).and_then(|n| n.parse().ok()))
        .unwrap();
    events.send(Event::ScreenCastEnded(stream_id)).unwrap();
    let message = tokio::time::timeout(Duration::from_secs(5), closed.next()).await.unwrap().unwrap().unwrap();
    assert_eq!(message.header().path().unwrap().as_str(), s3.as_str());
    assert!(!sharing(&conn).await);

    // A window, chosen in the dialog.
    let s4 = ObjectPath::try_from("/org/freedesktop/portal/desktop/session/1_1/s4").unwrap();
    let none: HashMap<&str, Value<'_>> = HashMap::new();
    assert_eq!(call(&conn, "CreateSession", &(&request, &s4, "org.example.Meet", none.clone())).await.0, 0);
    let options = HashMap::from([("types", Value::from(2u32)), ("persist_mode", Value::from(2u32))]);
    assert_eq!(call(&conn, "SelectSources", &(&request, &s4, "org.example.Meet", options)).await.0, 0);
    let (r, results) = call(&conn, "Start", &(&request, &s4, "org.example.Meet", "", none.clone())).await;
    assert_eq!(r, 0);
    assert_eq!(asked.lock().unwrap().last().unwrap().1, vec![], "windows only: no monitors offered");
    assert!(asked.lock().unwrap().last().unwrap().2, "windows offered");
    let streams = <Vec<(u32, HashMap<String, OwnedValue>)>>::try_from(results["streams"].clone()).unwrap();
    assert_eq!(u32::try_from(streams[0].1["source_type"].clone()).unwrap(), 2);
    assert_eq!(<(i32, i32)>::try_from(streams[0].1["size"].clone()).unwrap(), (800, 600));
    assert_eq!(u32::try_from(results["persist_mode"].clone()).unwrap(), 0, "windows are not remembered");
    assert!(!results.contains_key("restore_data"));
    assert!(log.lock().unwrap().last().unwrap().contains("Window(7)"));

    // A monitor, remembered; then shared again without asking.
    let s5 = ObjectPath::try_from("/org/freedesktop/portal/desktop/session/1_1/s5").unwrap();
    assert_eq!(call(&conn, "CreateSession", &(&request, &s5, "org.example.Meet", none.clone())).await.0, 0);
    let options = HashMap::from([("types", Value::from(1u32)), ("persist_mode", Value::from(2u32))]);
    assert_eq!(call(&conn, "SelectSources", &(&request, &s5, "org.example.Meet", options)).await.0, 0);
    let (r, results) = call(&conn, "Start", &(&request, &s5, "org.example.Meet", "", none.clone())).await;
    assert_eq!(r, 0);
    assert!(asked.lock().unwrap().last().unwrap().3, "the dialog says it will be remembered");
    assert_eq!(u32::try_from(results["persist_mode"].clone()).unwrap(), 2);
    let restore = results["restore_data"].clone();
    let asked_before = asked.lock().unwrap().len();
    let s6 = ObjectPath::try_from("/org/freedesktop/portal/desktop/session/1_1/s6").unwrap();
    assert_eq!(call(&conn, "CreateSession", &(&request, &s6, "org.example.Meet", none.clone())).await.0, 0);
    let options = HashMap::from([
        ("types", Value::from(1u32)),
        ("persist_mode", Value::from(2u32)),
        ("restore_data", Value::from(restore)),
    ]);
    assert_eq!(call(&conn, "SelectSources", &(&request, &s6, "org.example.Meet", options)).await.0, 0);
    let (r, results) = call(&conn, "Start", &(&request, &s6, "org.example.Meet", "", none.clone())).await;
    assert_eq!(r, 0);
    assert_eq!(asked.lock().unwrap().len(), asked_before, "not asked again");
    assert!(results.contains_key("restore_data"));
    assert!(log.lock().unwrap().last().unwrap().contains("Monitor(\"DP-1\")"));

    // Only a virtual monitor: made without asking, with metadata cursors.
    let asked_before = asked.lock().unwrap().len();
    let s7 = ObjectPath::try_from("/org/freedesktop/portal/desktop/session/1_1/s7").unwrap();
    assert_eq!(call(&conn, "CreateSession", &(&request, &s7, "org.example.Meet", none.clone())).await.0, 0);
    let options = HashMap::from([("types", Value::from(4u32)), ("cursor_mode", Value::from(4u32))]);
    assert_eq!(call(&conn, "SelectSources", &(&request, &s7, "org.example.Meet", options)).await.0, 0);
    let (r, results) = call(&conn, "Start", &(&request, &s7, "org.example.Meet", "", none.clone())).await;
    assert_eq!(r, 0);
    assert_eq!(asked.lock().unwrap().len(), asked_before, "not asked");
    let streams = <Vec<(u32, HashMap<String, OwnedValue>)>>::try_from(results["streams"].clone()).unwrap();
    assert_eq!(u32::try_from(streams[0].1["source_type"].clone()).unwrap(), 4);
    assert_eq!(<(i32, i32)>::try_from(streams[0].1["position"].clone()).unwrap(), (1920, 0));
    assert!(log.lock().unwrap().last().unwrap().ends_with("Virtual(\"Virtual-1\") cursor=Metadata"), "{:?}", log.lock().unwrap());

    // Only the portal may record directly.
    let stranger = bus.connect().await;
    let denied = stranger
        .call_method(Some("org.gnome.Mutter.ScreenCast"), "/org/gnome/Mutter/ScreenCast", Some("org.gnome.Mutter.ScreenCast"), "CreateSession", &(none_props(),))
        .await;
    assert!(matches!(denied, Err(zbus::Error::MethodError(name, _, _)) if name.as_str() == "org.freedesktop.DBus.Error.AccessDenied"));
}

fn none_props() -> HashMap<&'static str, Value<'static>> {
    HashMap::new()
}
