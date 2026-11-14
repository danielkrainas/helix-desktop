//! The shell's windows on a private accessibility bus (helix-desktop-spec
//! `contracts/dbus/org.a11y.atspi.md`): a fake `org.a11y.Bus` on a test
//! session bus hands out a second test bus as the accessibility bus, with
//! a fake registry on it; a client there reads the tree over AT-SPI and
//! presses a button.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use helix_shell::a11y::publish::{Event, Publisher};
use helix_shell::a11y::{Node, Rect, Role, Window};
use helix_test_support::TestBus;
use zbus::zvariant::{OwnedObjectPath, Type};

const ROOT: &str = "/org/a11y/atspi/accessible/root";

struct A11yBus {
    address: String,
}

#[zbus::interface(name = "org.a11y.Bus")]
impl A11yBus {
    fn get_address(&self) -> String {
        self.address.clone()
    }
}

struct Status;

#[zbus::interface(name = "org.a11y.Status")]
impl Status {
    #[zbus(property)]
    fn is_enabled(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn screen_reader_enabled(&self) -> bool {
        true
    }
}

#[derive(serde::Serialize, serde::Deserialize, Type)]
struct ObjectRef(String, OwnedObjectPath);

/// The registry's socket: records who embeds itself.
struct Socket {
    embedded: Mutex<mpsc::Sender<String>>,
}

#[zbus::interface(name = "org.a11y.atspi.Socket")]
impl Socket {
    fn embed(&self, plug: ObjectRef, #[zbus(connection)] conn: &zbus::Connection) -> ObjectRef {
        let _ = self.embedded.lock().unwrap().send(plug.0);
        let me = conn.unique_name().map(|n| n.to_string()).unwrap_or_default();
        ObjectRef(me, OwnedObjectPath::try_from(ROOT).unwrap())
    }
}

async fn accessible<'a>(conn: &zbus::Connection, name: &str, path: OwnedObjectPath) -> zbus::Proxy<'a> {
    zbus::Proxy::new(conn, name.to_owned(), path, "org.a11y.atspi.Accessible").await.unwrap()
}

async fn children(proxy: &zbus::Proxy<'_>) -> Vec<ObjectRef> {
    proxy.call("GetChildren", &()).await.unwrap()
}

/// (role, name) of an accessible.
async fn describe(proxy: &zbus::Proxy<'_>) -> (u32, String) {
    (proxy.call("GetRole", &()).await.unwrap(), proxy.get_property("Name").await.unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn windows_are_read_and_pressed_over_atspi() {
    let Some(session) = TestBus::start() else { return };
    let Some(a11y) = TestBus::start() else { return };
    // The adapters' thread (started with the first adapter) finds the
    // accessibility bus through the session bus.
    std::env::set_var("DBUS_SESSION_BUS_ADDRESS", session.address());
    std::env::remove_var("AT_SPI_BUS_ADDRESS");
    let _bus = session.builder().serve_at("/org/a11y/bus", A11yBus { address: a11y.address().to_owned() }).unwrap().serve_at("/org/a11y/bus", Status).unwrap().name("org.a11y.Bus").unwrap().build().await.unwrap();
    let (embedded_tx, embedded) = mpsc::channel();
    let _registry = a11y.builder().serve_at(ROOT, Socket { embedded: Mutex::new(embedded_tx) }).unwrap().name("org.a11y.atspi.Registry").unwrap().build().await.unwrap();

    let (events_tx, events) = mpsc::channel();
    let events_tx = Mutex::new(events_tx);
    let mut publisher: Publisher<&'static str> = Publisher::new(Arc::new(move |event| {
        let _ = events_tx.lock().unwrap().send(event);
    }));
    let field = Rect::new(400, 300, 360, 44);
    let switch = Rect::new(1000, 700, 140, 36);
    let window = Window {
        title: "Lock Screen".into(),
        bounds: Rect::new(0, 0, 1280, 800),
        active: true,
        children: vec![
            Node::label("12:00").id("clock"),
            Node::text_field(Role::PasswordInput, "Password", Node::mask("abc"), field).id("password").focused(true),
            Node::button("Switch User…", switch).id("switch-user"),
        ],
    };
    let shown = || vec![("lock".to_owned(), "lock surface")];
    publisher.sync(shown(), |_| Some(window.clone()));
    // A client is there: the adapter asks for the tree, which comes with
    // the next sync.
    let event = events.recv_timeout(Duration::from_secs(10)).expect("adapter activated");
    assert!(matches!(event, Event::Activated(_)), "{event:?}");
    assert!(publisher.event(event).is_none());
    assert!(publisher.any_active());
    publisher.sync(shown(), |_| Some(window.clone()));

    // The application embedded itself in the registry.
    let app = embedded.recv_timeout(Duration::from_secs(10)).expect("application embedded");
    let client = a11y.connect().await;
    let root = accessible(&client, &app, OwnedObjectPath::try_from(ROOT).unwrap()).await;
    // AT-SPI roles: application 75, frame 23, label 29, password text
    // 40, button 43.
    assert_eq!(root.call::<_, _, u32>("GetRole", &()).await.unwrap(), 75);
    let mut windows = children(&root).await;
    for _ in 0..50 {
        if !windows.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        windows = children(&root).await;
    }
    assert_eq!(windows.len(), 1, "one window");
    let window_ref = windows.remove(0);
    let lock = accessible(&client, &window_ref.0, window_ref.1).await;
    assert_eq!(describe(&lock).await, (23, "Lock Screen".to_owned()));
    let mut seen = Vec::new();
    let mut button = None;
    let mut password_state = Vec::new();
    for child in children(&lock).await {
        let proxy = accessible(&client, &child.0, child.1.clone()).await;
        let (role, name) = describe(&proxy).await;
        if role == 40 {
            password_state = proxy.call::<_, _, Vec<u32>>("GetState", &()).await.unwrap();
        }
        if role == 43 {
            button = Some(child);
        }
        seen.push((role, name));
    }
    assert_eq!(seen, [(29, "12:00".to_owned()), (40, "Password".to_owned()), (43, "Switch User…".to_owned())]);
    // State bits: 11 focusable, 12 focused.
    assert_eq!(password_state.first().map(|w| w & (1 << 11 | 1 << 12)), Some(1 << 11 | 1 << 12), "the password field has the focus");

    // Pressing the button comes back as an action on its place.
    let button = button.expect("button");
    let action = zbus::Proxy::new(&client, button.0.clone(), button.1.clone(), "org.a11y.atspi.Action").await.unwrap();
    let pressed: bool = action.call("DoAction", &(0i32,)).await.unwrap();
    assert!(pressed);
    let event = events.recv_timeout(Duration::from_secs(10)).expect("action");
    let (surface, target, request) = publisher.event(event).expect("an action on a known node");
    assert_eq!((surface, target.bounds, request.action), ("lock surface", switch, accesskit::Action::Click));

    // The surface goes away: so does its window.
    publisher.sync(Vec::new(), |_| None);
    let mut left = children(&root).await;
    for _ in 0..50 {
        if left.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        left = children(&root).await;
    }
    assert!(left.is_empty(), "the window left the application");
}
