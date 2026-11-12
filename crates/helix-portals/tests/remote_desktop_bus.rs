//! The RemoteDesktop portal backend against the compositor's real
//! `org.gnome.Mutter.RemoteDesktop` / `ScreenCast` (with a stand-in for the
//! compositor) and a stand-in shell chooser.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use helix_comp::display_config::{Mode, Monitor, Store, ALL_TRANSFORMS};
use helix_comp::remote_desktop::{ClipboardRequest, KeymapRequest, RemoteError, RemoteInput, RemoteReply, RemoteRequest};
use helix_comp::screencast::CastRequest;
use helix_comp::{CompositorServices, MonitorControl};
use helix_portals::settings::Snapshot;
use helix_portals::{PortalService, BUS_NAME, OBJECT_PATH};
use helix_session::SessionService;
use helix_test_support::TestBus;
use tokio::sync::mpsc;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
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
        let monitors = options.get("monitors").and_then(|v| bool::try_from(v.clone()).ok()).unwrap_or(true);
        if monitors {
            (true, 1, "DP-1".into(), 0)
        } else {
            (true, 2, String::new(), 7)
        }
    }

    async fn describe_window(&self, window: u32) -> zbus::fdo::Result<(String, String)> {
        if window == 7 {
            Ok(("org.example.Doc".into(), "Notes".into()))
        } else {
            Err(zbus::fdo::Error::InvalidArgs("no window".into()))
        }
    }

    async fn find_window(&self, app_id: String, title: String) -> (bool, u32) {
        if app_id == "org.example.Doc" && title == "Notes" {
            (true, 7)
        } else {
            (false, 0)
        }
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
        hdr: false,
        color_mode: 0,
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
                CastRequest::Capture(request) => log.lock().unwrap().push(format!("capture {request:?}")),
                CastRequest::Stop { stream } => log.lock().unwrap().push(format!("stop {stream}")),
                CastRequest::LockKeys { reply } => {
                    let _ = reply.send((true, false));
                }
                CastRequest::WindowSize { window, reply } => {
                    let _ = reply.send((window == 7).then_some((800, 600)));
                }
                CastRequest::CreateVirtual { reply, .. } => {
                    let _ = reply.send(Err("none".into()));
                }
                CastRequest::Remote { session, request, reply } => {
                    let pipe_end = || -> std::os::fd::OwnedFd { std::os::unix::net::UnixStream::pair().unwrap().0.into() };
                    let answer = match request {
                        RemoteRequest::Clipboard(ClipboardRequest::Write(serial)) => {
                            log.lock().unwrap().push(format!("clip write {serial}"));
                            Ok(RemoteReply::Fd(pipe_end()))
                        }
                        RemoteRequest::Clipboard(ClipboardRequest::Read(mime)) => {
                            log.lock().unwrap().push(format!("clip read {mime}"));
                            Ok(RemoteReply::Fd(pipe_end()))
                        }
                        RemoteRequest::Keymap(KeymapRequest::Current) => Ok(RemoteReply::Keymap { name: "English (US)".into(), source: 1 }),
                        RemoteRequest::Keymap(KeymapRequest::LayoutIndex(i)) if i > 0 => Err(RemoteError::InvalidArgs("no such layout".into())),
                        other => {
                            log.lock().unwrap().push(format!("remote {session} {other:?}"));
                            Ok(RemoteReply::Done)
                        }
                    };
                    let _ = reply.send(answer);
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
const CB: &str = "org.freedesktop.impl.portal.Clipboard";
const MUTTER_RD: &str = "org.gnome.Mutter.RemoteDesktop";
const MUTTER_SESSION: &str = "org.gnome.Mutter.RemoteDesktop.Session";

/// The compositor's interface directly (as the portal backend): keymaps and
/// clipboard errors.
#[tokio::test(flavor = "multi_thread")]
async fn mutter_keymaps_and_clipboard() {
    let Some(bus) = TestBus::start() else { return };
    let log = Arc::new(Mutex::new(Vec::new()));
    let store = std::env::temp_dir().join(format!("helix-rdk-{}.toml", std::process::id()));
    let control = MonitorControl { store: Store::at(store), cast: Some(fake_compositor(log.clone())), ..MonitorControl::default() };
    let services = CompositorServices::start_with(bus.builder(), vec![monitor("DP-1")], control).await.unwrap();
    let (_events, rx) = mpsc::unbounded_channel();
    tokio::spawn(services.run(rx));
    let conn = bus.connect().await;
    conn.request_name("org.freedesktop.impl.portal.desktop.helix").await.unwrap();
    let create = || async {
        let reply = conn.call_method(Some(MUTTER_RD), "/org/gnome/Mutter/RemoteDesktop", Some(MUTTER_RD), "CreateSession", &()).await.unwrap();
        reply.body().deserialize::<OwnedObjectPath>().unwrap()
    };
    let error = |r: zbus::Result<zbus::Message>| match r {
        Err(zbus::Error::MethodError(name, _, _)) => name.as_str().to_owned(),
        other => format!("{other:?}"),
    };
    let none: HashMap<&str, Value<'_>> = HashMap::new();

    // Not started: refused.
    let idle = create().await;
    let r = conn.call_method(Some(MUTTER_RD), idle.as_str(), Some(MUTTER_SESSION), "EnableClipboard", &(none.clone(),)).await;
    assert_eq!(error(r), "org.freedesktop.DBus.Error.Failed");

    let session = create().await;
    conn.call_method(Some(MUTTER_RD), session.as_str(), Some(MUTTER_SESSION), "Start", &()).await.unwrap();
    let proxy = zbus::Proxy::new(&conn, MUTTER_RD, session.as_str(), MUTTER_SESSION).await.unwrap();
    let caps: HashMap<String, OwnedValue> = proxy.get_property("KeymapCapabilities").await.unwrap();
    assert_eq!(Vec::<u32>::try_from(caps["supported-keymap-types"].clone()).unwrap(), [0]);
    assert_eq!(Vec::<u32>::try_from(caps["supported-xkb-keymap-formats"].clone()).unwrap(), [1]);

    // A keymap by fd, read to its end.
    let (a, mut b) = std::os::unix::net::UnixStream::pair().unwrap();
    std::io::Write::write_all(&mut b, b"xkb_keymap { };\0").unwrap();
    drop(b);
    let fd = zbus::zvariant::Fd::from(std::os::fd::OwnedFd::from(a));
    let options = HashMap::from([
        ("keymap-type", Value::from(0u32)),
        ("xkb-keymap", Value::from(fd)),
        ("xkb-keymap-layout-index", Value::from(0u32)),
        ("lock-keymap", Value::from(true)),
    ]);
    conn.call_method(Some(MUTTER_RD), session.as_str(), Some(MUTTER_SESSION), "SetKeymap", &(options,)).await.unwrap();
    let missing = HashMap::from([("keymap-type", Value::from(0u32))]);
    let r = conn.call_method(Some(MUTTER_RD), session.as_str(), Some(MUTTER_SESSION), "SetKeymap", &(missing,)).await;
    assert_eq!(error(r), "org.freedesktop.DBus.Error.InvalidArgs");
    let other = HashMap::from([("keymap-type", Value::from(5u32))]);
    let r = conn.call_method(Some(MUTTER_RD), session.as_str(), Some(MUTTER_SESSION), "SetKeymap", &(other,)).await;
    assert_eq!(error(r), "org.freedesktop.DBus.Error.InvalidArgs");
    conn.call_method(Some(MUTTER_RD), session.as_str(), Some(MUTTER_SESSION), "SetKeymap", &(none.clone(),)).await.unwrap();
    let r = conn.call_method(Some(MUTTER_RD), session.as_str(), Some(MUTTER_SESSION), "SetKeymapLayoutIndex", &(2u32,)).await;
    assert_eq!(error(r), "org.freedesktop.DBus.Error.InvalidArgs");
    let current: HashMap<String, OwnedValue> = proxy.get_property("CurrentKeymap").await.unwrap();
    assert_eq!(String::try_from(current["name"].clone()).unwrap(), "English (US)");
    assert_eq!(u32::try_from(current["source"].clone()).unwrap(), 1);

    // SetSelection needs types.
    let r = conn.call_method(Some(MUTTER_RD), session.as_str(), Some(MUTTER_SESSION), "SetSelection", &(none.clone(),)).await;
    assert_eq!(error(r), "org.freedesktop.DBus.Error.InvalidArgs");

    // Someone else's session: refused.
    let stranger = bus.connect().await;
    let r = stranger.call_method(Some(MUTTER_RD), session.as_str(), Some(MUTTER_SESSION), "SetKeymap", &(none.clone(),)).await;
    assert_eq!(error(r), "org.freedesktop.DBus.Error.AccessDenied");

    tokio::time::sleep(Duration::from_millis(100)).await;
    let log = log.lock().unwrap();
    let set = log.iter().find(|l| l.contains("Keymap(Set(Some(")).expect("SetKeymap reached the compositor");
    assert!(set.contains("text: \"xkb_keymap { };\", layout: 0, lock: true"), "{set}");
    assert!(log.iter().any(|l| l.contains("Keymap(Set(None))")), "{log:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_control() {
    let Some(bus) = TestBus::start() else { return };
    let session = SessionService::start(bus.builder()).await.unwrap();
    let _session = tokio::spawn(session.run(std::future::pending()));
    let log = Arc::new(Mutex::new(Vec::new()));
    let store = std::env::temp_dir().join(format!("helix-rd-{}.toml", std::process::id()));
    let control = MonitorControl { store: Store::at(store), cast: Some(fake_compositor(log.clone())), ..MonitorControl::default() };
    let services = CompositorServices::start_with(bus.builder(), vec![monitor("DP-1")], control).await.unwrap();
    let (events, rx) = mpsc::unbounded_channel();
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

    // The clipboard: asked for before Start, shared after.
    let s3 = ObjectPath::try_from("/org/freedesktop/portal/desktop/session/1_1/rd3").unwrap();
    assert_eq!(call(&conn, RD, "CreateSession", &(&request, &s3, "org.example.Remote", none.clone())).await.0, 0);
    conn.call_method(Some(BUS_NAME), OBJECT_PATH, Some(CB), "RequestClipboard", &(&s3, empty.clone())).await.unwrap();
    let (r, results) = call(&conn, RD, "Start", &(&request, &s3, "org.example.Remote", "", none.clone())).await;
    assert_eq!(r, 0);
    assert!(bool::try_from(results["clipboard_enabled"].clone()).unwrap());
    let too_late = conn.call_method(Some(BUS_NAME), OBJECT_PATH, Some(CB), "RequestClipboard", &(&s3, empty.clone())).await;
    assert!(too_late.is_err());
    let enabled = log.lock().unwrap().iter().find(|l| l.contains("Clipboard(Enable([]))")).cloned().expect("EnableClipboard");
    let mutter_id: u32 = enabled.split(' ').nth(1).unwrap().parse().unwrap();
    let options = HashMap::from([("mime_types", Value::from(vec!["text/plain"]))]);
    conn.call_method(Some(BUS_NAME), OBJECT_PATH, Some(CB), "SetSelection", &(&s3, options)).await.unwrap();
    let fd = conn.call_method(Some(BUS_NAME), OBJECT_PATH, Some(CB), "SelectionRead", &(&s3, "text/html")).await.unwrap();
    let _fd: zbus::zvariant::OwnedFd = fd.body().deserialize().unwrap();
    // Not shared on s1: refused.
    assert!(conn.call_method(Some(BUS_NAME), OBJECT_PATH, Some(CB), "SelectionRead", &(&s1, "text/html")).await.is_err());
    // Compositor signals come out as the portal's, for the right session.
    let mut transfers = zbus::MessageStream::for_match_rule(
        zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).interface(CB).unwrap().build(),
        &conn,
        None,
    )
    .await
    .unwrap();
    events
        .send(helix_comp::Event::RemoteSelectionOwner { session: mutter_id, mime_types: vec!["image/png".into()], is_owner: false })
        .unwrap();
    events.send(helix_comp::Event::RemoteSelectionTransfer { session: mutter_id, mime_type: "text/plain".into(), serial: 7 }).unwrap();
    let mut got = Vec::new();
    while got.len() < 2 {
        let msg = tokio::time::timeout(Duration::from_secs(5), futures_util::StreamExt::next(&mut transfers)).await.unwrap().unwrap().unwrap();
        got.push(msg);
    }
    // The two signals come from separate streams: in either order.
    got.sort_by_key(|m| m.header().member().map(|m| m.to_string()));
    let (path, owner): (OwnedObjectPath, HashMap<String, OwnedValue>) = got[0].body().deserialize().unwrap();
    assert_eq!(path.as_str(), s3.as_str());
    assert_eq!(Vec::<String>::try_from(owner["mime_types"].clone()).unwrap(), ["image/png"]);
    assert!(!bool::try_from(owner["session_is_owner"].clone()).unwrap());
    let (path, mime, serial): (OwnedObjectPath, String, u32) = got[1].body().deserialize().unwrap();
    assert_eq!((path.as_str(), mime.as_str(), serial), (s3.as_str(), "text/plain", 7));
    let fd = conn.call_method(Some(BUS_NAME), OBJECT_PATH, Some(CB), "SelectionWrite", &(&s3, 7u32)).await.unwrap();
    let _fd: zbus::zvariant::OwnedFd = fd.body().deserialize().unwrap();
    conn.call_method(Some(BUS_NAME), OBJECT_PATH, Some(CB), "SelectionWriteDone", &(&s3, 7u32, true)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    {
        let log = log.lock().unwrap();
        assert!(log.iter().any(|l| l.contains("SetSelection([\"text/plain\"])")), "{log:?}");
        assert!(log.contains(&"clip read text/html".to_owned()), "{log:?}");
        assert!(log.contains(&"clip write 7".to_owned()), "{log:?}");
        assert!(log.iter().any(|l| l.contains("WriteDone(7, true)")), "{log:?}");
    }

    // Remembering: remote control without a screen.
    let start = |path: &'static str, devices: u32, restore: Option<Value<'static>>, sources: Option<u32>| {
        let conn = conn.clone();
        let request = request.clone();
        async move {
            let session = ObjectPath::try_from(path).unwrap();
            assert_eq!(call(&conn, RD, "CreateSession", &(&request, &session, "org.example.Remote", HashMap::<&str, Value<'_>>::new())).await.0, 0);
            let mut options = HashMap::from([("types", Value::from(devices)), ("persist_mode", Value::from(2u32))]);
            if let Some(restore) = restore {
                options.insert("restore_data", restore);
            }
            assert_eq!(call(&conn, RD, "SelectDevices", &(&request, &session, "org.example.Remote", options)).await.0, 0);
            if let Some(types) = sources {
                let options = HashMap::from([("types", Value::from(types))]);
                assert_eq!(call(&conn, SC, "SelectSources", &(&request, &session, "org.example.Remote", options)).await.0, 0);
            }
            call(&conn, RD, "Start", &(&request, &session, "org.example.Remote", "", HashMap::<&str, Value<'_>>::new())).await
        }
    };
    let restore_of = |results: &HashMap<String, OwnedValue>| Value::from(results["restore_data"].try_clone().unwrap());
    let asked_before = asked.lock().unwrap().len();
    let (r, first) = start("/org/freedesktop/portal/desktop/session/1_1/rd4", 3, None, None).await;
    assert_eq!(r, 0);
    assert_eq!(u32::try_from(first["persist_mode"].clone()).unwrap(), 2);
    assert_eq!(asked.lock().unwrap().len(), asked_before + 1, "asked the first time");
    let (r, again) = start("/org/freedesktop/portal/desktop/session/1_1/rd5", 1, Some(restore_of(&first)), None).await;
    assert_eq!(r, 0);
    assert_eq!(u32::try_from(again["devices"].clone()).unwrap(), 1);
    assert_eq!(asked.lock().unwrap().len(), asked_before + 1, "remembered: not asked");
    let (r, _) = start("/org/freedesktop/portal/desktop/session/1_1/rd6", 7, Some(restore_of(&first)), None).await;
    assert_eq!(r, 0);
    assert_eq!(asked.lock().unwrap().len(), asked_before + 2, "touchscreen was not allowed before: asked");

    // A shared window is remembered by app id and title.
    let (r, window) = start("/org/freedesktop/portal/desktop/session/1_1/rd7", 1, None, Some(2)).await;
    assert_eq!(r, 0);
    assert_eq!(asked.lock().unwrap().len(), asked_before + 3);
    let data = restore_of(&window);
    assert!(format!("{data:?}").contains("Notes"), "{data:?}");
    let (r, again) = start("/org/freedesktop/portal/desktop/session/1_1/rd8", 1, Some(data), Some(2)).await;
    assert_eq!(r, 0);
    assert!(again.contains_key("streams"));
    assert_eq!(asked.lock().unwrap().len(), asked_before + 3, "the remembered window: not asked");

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
