//! The login screen read over AT-SPI on a private accessibility bus
//! (helix-desktop-spec `components/greeter.md`, "Accessibility"): the
//! greeter's window is built from its view and painted places, published
//! with AccessKit as the greeter does, and read back by a client: roles,
//! names, states, the password field's text (masked); an action clicks.
//!
//! AccessKit's adapter finds the accessibility bus once per process,
//! through the session bus, so the reading runs in a process of its own:
//! `login_screen_over_atspi` runs this test binary again for `child`
//! only, which sets up its buses (a private session bus with a fake
//! `org.a11y.Bus`, and a second private bus as the accessibility bus with
//! a fake registry). No real accessibility bus, screen reader or audio is
//! touched.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use helix_a11y::publish::{Event, Publisher};
use helix_test_support::TestBus;
use zbus::zvariant::{OwnedObjectPath, Type};

use crate::app::{Effect, Greeter, Hit, Key};
use crate::config::Config;
use crate::greetd::{AuthMessageType, Response};
use crate::paint::Hits;
use crate::power::Offered;
use crate::sessions::Session;
use crate::state::State;
use crate::users::User;

const ROOT: &str = "/org/a11y/atspi/accessible/root";
const CHILD: &str = "HELIX_GREETER_ATSPI_CHILD";
/// What the reading process prints at its end, or when it cannot start.
const READ: &str = "login screen read over AT-SPI";
const SKIPPED: &str = "login screen not read: no private D-Bus";

#[test]
fn login_screen_over_atspi() {
    let exe = std::env::current_exe().expect("test binary");
    let out = std::process::Command::new(exe)
        .args(["atspi_bus::child", "--exact", "--ignored", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .env_remove("AT_SPI_BUS_ADDRESS")
        .output()
        .expect("run the reading process");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "reading over AT-SPI failed:\n{stdout}\n{stderr}");
    assert!(stdout.contains("1 passed"), "the reading test did not run:\n{stdout}\n{stderr}");
    // Without dbus-daemon there are no private buses to read on.
    assert!(stdout.contains(READ) || stdout.contains(SKIPPED), "the reading test stopped early:\n{stdout}\n{stderr}");
    if stdout.contains(SKIPPED) {
        eprintln!("login screen over AT-SPI: skipped (no dbus-daemon)");
    }
}

// ----- The fake accessibility bus ---------------------------------------------

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

#[derive(serde::Serialize, serde::Deserialize, Type, Debug, Clone)]
struct ObjectRef(String, OwnedObjectPath);

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

// ----- Reading -----------------------------------------------------------------

/// AT-SPI state bits.
const CHECKED: u64 = 1 << 4;
const EXPANDABLE: u64 = 1 << 9;
const EXPANDED: u64 = 1 << 10;
const FOCUSED: u64 = 1 << 12;
const SELECTED: u64 = 1 << 23;

#[derive(Debug, Clone)]
struct Obj {
    at: ObjectRef,
    role: String,
    name: String,
    states: u64,
    children: Vec<Obj>,
}

impl Obj {
    fn has(&self, bits: u64) -> bool {
        self.states & bits == bits
    }

    fn all(&self, f: &dyn Fn(&Obj) -> bool) -> Vec<&Obj> {
        let mut out = Vec::new();
        if f(self) {
            out.push(self);
        }
        for c in &self.children {
            out.extend(c.all(f));
        }
        out
    }

    fn find(&self, role: &str, name: &str) -> &Obj {
        self.all(&|o| o.role == role && o.name == name).into_iter().next().unwrap_or_else(|| panic!("no {role} {name:?} in {:#?}", self.outline()))
    }

    fn focused(&self) -> Vec<String> {
        self.all(&|o| o.has(FOCUSED)).into_iter().map(|o| o.name.clone()).collect()
    }

    fn outline(&self) -> Vec<String> {
        fn walk(o: &Obj, depth: usize, out: &mut Vec<String>) {
            out.push(format!("{}{} {:?} {:#x}", "  ".repeat(depth), o.role, o.name, o.states));
            for c in &o.children {
                walk(c, depth + 1, out);
            }
        }
        let mut out = Vec::new();
        walk(self, 0, &mut out);
        out
    }
}

async fn proxy<'a>(conn: &zbus::Connection, at: &ObjectRef, interface: &'static str) -> zbus::Proxy<'a> {
    zbus::Proxy::new(conn, at.0.clone(), at.1.clone(), interface).await.unwrap()
}

/// AT-SPI role names (at-spi2-core's numbering) of the roles used here.
fn role_name(role: u32) -> String {
    let name = match role {
        8 => "check menu item",
        23 => "frame",
        29 => "label",
        31 => "list",
        32 => "list item",
        33 => "menu",
        39 => "panel",
        40 => "password text",
        43 => "push button",
        45 => "radio menu item",
        62 => "toggle button",
        75 => "application",
        79 => "entry",
        other => return format!("role {other}"),
    };
    name.to_owned()
}

fn read<'a>(conn: &'a zbus::Connection, at: ObjectRef) -> std::pin::Pin<Box<dyn std::future::Future<Output = Obj> + Send + 'a>> {
    Box::pin(async move {
        let p = proxy(conn, &at, "org.a11y.atspi.Accessible").await;
        let role = role_name(p.call("GetRole", &()).await.unwrap());
        let name: String = p.get_property("Name").await.unwrap();
        let words: Vec<u32> = p.call("GetState", &()).await.unwrap();
        let states = u64::from(words.first().copied().unwrap_or(0)) | u64::from(words.get(1).copied().unwrap_or(0)) << 32;
        let refs: Vec<ObjectRef> = p.call("GetChildren", &()).await.unwrap();
        let mut children = Vec::new();
        for r in refs {
            children.push(read(conn, r).await);
        }
        Obj { at, role, name, states, children }
    })
}

/// The greeter's side: its state, painted (for the places), published.
struct Screen {
    greeter: Greeter,
    hits: Hits,
    publisher: Publisher<()>,
    events: mpsc::Receiver<Event>,
}

const SIZE: (i32, i32) = (1280, 800);

impl Screen {
    /// Paint (no font needed: only the places) and publish.
    fn sync(&mut self) {
        let view = self.greeter.view(0);
        let mut data = vec![0u8; (SIZE.0 * SIZE.1 * 4) as usize];
        let mut canvas = helix_draw::Canvas::scaled(&mut data, SIZE.0 as usize, SIZE.1 as usize, 1.0);
        self.hits = crate::paint::paint(&mut canvas, None, &view, SIZE.0, SIZE.1);
        let window = crate::atspi::window(&view, &self.hits, SIZE);
        self.publisher.sync(vec![(crate::atspi::KEY.to_owned(), ())], |_| Some(window.clone()));
    }

    /// Handle the adapters' events for a while (activation), as the
    /// greeter's loop does.
    fn settle(&mut self) {
        while let Ok(event) = self.events.recv_timeout(Duration::from_millis(400)) {
            assert!(self.publisher.event(event).is_none(), "no actions yet");
            self.sync();
        }
    }

    /// The next action, done as the greeter does it: a click where its
    /// node was painted.
    fn act(&mut self) -> (Hit, Vec<Effect>) {
        let event = self.events.recv_timeout(Duration::from_secs(10)).expect("an action");
        let ((), target, request) = self.publisher.event(event).expect("an action on a known node");
        let hit = crate::atspi::hit_for(&self.hits, &target, &request).expect("a painted target");
        let effects = self.greeter.click(Some(hit));
        self.sync();
        (hit, effects)
    }
}

/// The client's side.
struct Client {
    conn: zbus::Connection,
    app: String,
}

impl Client {
    /// The login screen's window once `ready` accepts it.
    async fn window(&self, ready: impl Fn(&Obj) -> bool) -> Obj {
        let mut last = None;
        for _ in 0..100 {
            let root = ObjectRef(self.app.clone(), OwnedObjectPath::try_from(ROOT).unwrap());
            if let Some(w) = read(&self.conn, root).await.children.into_iter().find(|w| w.name == "Login screen") {
                if ready(&w) {
                    return w;
                }
                last = Some(w);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("login screen not as expected: {:#?}", last.map(|w| w.outline()));
    }

    async fn click(&self, o: &Obj) {
        let action = proxy(&self.conn, &o.at, "org.a11y.atspi.Action").await;
        assert!(action.call::<_, _, bool>("DoAction", &(0i32,)).await.unwrap(), "{} {:?}", o.role, o.name);
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "run by login_screen_over_atspi, in a process of its own"]
async fn child() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    let (Some(session), Some(a11y)) = (TestBus::start(), TestBus::start()) else {
        println!("{SKIPPED}");
        return;
    };
    std::env::set_var("DBUS_SESSION_BUS_ADDRESS", session.address());
    std::env::remove_var("AT_SPI_BUS_ADDRESS");
    let _bus = session.builder().serve_at("/org/a11y/bus", A11yBus { address: a11y.address().to_owned() }).unwrap().serve_at("/org/a11y/bus", Status).unwrap().name("org.a11y.Bus").unwrap().build().await.unwrap();
    let (embedded_tx, embedded) = mpsc::channel();
    let _registry = a11y.builder().serve_at(ROOT, Socket { embedded: Mutex::new(embedded_tx) }).unwrap().name("org.a11y.atspi.Registry").unwrap().build().await.unwrap();

    // A greeter asking Bob's password, "pw" typed.
    let sessions = vec![Session { id: "helix".into(), name: "Helix".into(), exec: "helix-session".into(), desktop_names: None }];
    let state = State { last_user: Some("bob".into()), ..Default::default() };
    let mut greeter = Greeter::new(Config::default(), sessions, &state);
    greeter.set_users(vec![User::new("ann", "Ann", 0), User::new("bob", "Bob", 0)]);
    greeter.set_power(Offered { restart: true, power_off: true });
    greeter.key(Key::Return);
    greeter.greetd_reply(Response::AuthMessage { auth_message_type: AuthMessageType::Secret, auth_message: "Password:".into() });
    greeter.key(Key::Text("p".into()));
    greeter.key(Key::Text("w".into()));

    let (events_tx, events) = mpsc::channel();
    let events_tx = Mutex::new(events_tx);
    let publisher: Publisher<()> = Publisher::new(Arc::new(move |event| {
        let _ = events_tx.lock().unwrap().send(event);
    }));
    let mut screen = Screen { greeter, hits: Hits::new(), publisher, events };
    screen.sync();
    let app = embedded.recv_timeout(Duration::from_secs(10)).expect("application embedded");
    screen.settle();
    let conn = a11y.connect().await;
    let client = Client { conn: conn.clone(), app };

    // ----- The password question.
    let w = client.window(|w| !w.focused().is_empty()).await;
    assert_eq!(w.role, "frame", "{:#?}", w.outline());
    let password = w.find("password text", "Password:");
    assert!(password.has(FOCUSED), "the field has the focus: {:#?}", w.outline());
    let text = proxy(&conn, &password.at, "org.a11y.atspi.Text").await;
    assert_eq!(text.call::<_, _, String>("GetText", &(0i32, -1i32)).await.unwrap(), "••", "the password itself is never published");
    w.find("label", "Bob");
    w.find("label", &crate::clock::date(0));
    w.find("push button", "Back");
    w.find("push button", "Restart…");
    w.find("push button", "Power Off…");
    let session_button = w.find("push button", "Helix");
    assert!(session_button.has(EXPANDABLE) && !session_button.has(EXPANDED));
    let a11y_button = w.find("push button", "Accessibility");
    assert!(a11y_button.has(EXPANDABLE) && !a11y_button.has(EXPANDED));

    // ----- Clicking "Accessibility" opens its menu; the focus moves there.
    client.click(a11y_button).await;
    let (hit, effects) = screen.act();
    assert_eq!((hit, effects), (Hit::A11yButton, Vec::new()));
    let w = client.window(|w| w.focused() == ["High Contrast"]).await;
    assert!(w.find("push button", "Accessibility").has(EXPANDED));
    let menu = w.find("menu", "Accessibility");
    let items: Vec<(&str, &str)> = menu.children.iter().map(|c| (c.role.as_str(), c.name.as_str())).collect();
    assert_eq!(items, [("check menu item", "High Contrast"), ("check menu item", "Large Text"), ("check menu item", "Screen Reader"), ("check menu item", "Screen Keyboard")]);
    assert!(menu.children.iter().all(|c| !c.has(CHECKED)), "all off");

    // ----- Turning on the screen keyboard: checked, and its keys appear.
    client.click(w.find("check menu item", "Screen Keyboard")).await;
    let (hit, effects) = screen.act();
    assert_eq!(hit, Hit::A11yItem(crate::app::Setting::ScreenKeyboard));
    assert!(matches!(&effects[..], [Effect::Accessibility(s)] if s.accessibility.screen_keyboard));
    let w = client.window(|w| w.all(&|o| o.name == "Screen Keyboard" && o.has(CHECKED)).len() == 1).await;
    let keyboard = w.find("panel", "Screen Keyboard");
    assert!(keyboard.children.iter().any(|k| k.role == "push button" && k.name == "q"), "{:#?}", keyboard.outline());
    // Close the menu (a click elsewhere), then type with a key.
    screen.greeter.click(None);
    screen.sync();
    let w = client.window(|w| w.all(&|o| o.role == "menu").is_empty()).await;
    client.click(w.find("panel", "Screen Keyboard").find("push button", "q")).await;
    let (hit, _) = screen.act();
    assert!(matches!(hit, Hit::OskKey(_)));
    let w = client.window(|w| w.focused() == ["Password:"]).await;
    let text = proxy(&conn, &w.find("password text", "Password:").at, "org.a11y.atspi.Text").await;
    assert_eq!(text.call::<_, _, String>("GetText", &(0i32, -1i32)).await.unwrap(), "•••", "one more character");

    // ----- Back to the list: the users, the selected one focused.
    client.click(w.find("push button", "Back")).await;
    let (hit, effects) = screen.act();
    assert_eq!(hit, Hit::Back);
    assert_eq!(effects.len(), 1, "the login is cancelled");
    screen.greeter.greetd_reply(Response::Success);
    screen.sync();
    let w = client.window(|w| w.focused() == ["Bob"]).await;
    let list = w.find("list", "Users");
    let users: Vec<(&str, bool)> = list.children.iter().map(|c| (c.name.as_str(), c.has(SELECTED))).collect();
    assert_eq!(users, [("Ann", false), ("Bob", true)]);
    assert_eq!(list.children[0].role, "list item");
    println!("{READ}");
}
