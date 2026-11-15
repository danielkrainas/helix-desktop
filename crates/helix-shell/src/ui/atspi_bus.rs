//! The surfaces published by helix-desktop#73 read over AT-SPI on a
//! private accessibility bus (helix-desktop-spec
//! `contracts/dbus/org.a11y.atspi.md`): their windows are built by the
//! same code the shell uses (from painted places where the surface paints
//! them), published with AccessKit, and read back by a client: roles,
//! names, descriptions, states, focus events, text extents and edits.
//!
//! AccessKit's adapter finds the accessibility bus once per process,
//! through the session bus, so the reading runs in a process of its own:
//! `surfaces_over_atspi` runs this test binary again for `child` only,
//! which sets up its buses and does the work.

use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use helix_test_support::TestBus;
use zbus::zvariant::{OwnedObjectPath, Type};

use super::window_menu::{self, Choice, Item};
use super::Rect;
use crate::a11y::publish::{Event, Publisher};
use crate::a11y::{Node, Role, Window};

const ROOT: &str = "/org/a11y/atspi/accessible/root";
const CHILD: &str = "HELIX_ATSPI_BUS_CHILD";

#[test]
fn surfaces_over_atspi() {
    let exe = std::env::current_exe().expect("test binary");
    let out = std::process::Command::new(exe)
        .args(["ui::atspi_bus::child", "--exact", "--ignored", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .output()
        .expect("run the reading process");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "reading over AT-SPI failed:\n{stdout}\n{stderr}");
    assert!(stdout.contains("1 passed"), "the reading test did not run:\n{stdout}\n{stderr}");
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
const FOCUSED: u64 = 1 << 12;
const MODAL: u64 = 1 << 16;
const PRESSED: u64 = 1 << 20;
const SELECTED: u64 = 1 << 23;
const SENSITIVE: u64 = 1 << 24;

/// An accessible object as read.
#[derive(Debug, Clone)]
struct Obj {
    at: ObjectRef,
    role: String,
    name: String,
    description: String,
    states: u64,
    children: Vec<Obj>,
}

impl Obj {
    fn has(&self, bits: u64) -> bool {
        self.states & bits == bits
    }

    /// Depth first, the objects `f` accepts.
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

    fn named(&self, name: &str) -> &Obj {
        self.all(&|o| o.name == name).into_iter().next().unwrap_or_else(|| panic!("no {name:?} in {:#?}", self.outline()))
    }

    fn focused(&self) -> Vec<String> {
        self.all(&|o| o.has(FOCUSED)).into_iter().map(|o| o.name.clone()).collect()
    }

    /// (role, name) of each object, indented: for failure messages.
    fn outline(&self) -> Vec<String> {
        fn walk(o: &Obj, depth: usize, out: &mut Vec<String>) {
            out.push(format!("{}{} {:?} {:?} {:#x}", "  ".repeat(depth), o.role, o.name, o.description, o.states));
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

/// AT-SPI role names (at-spi2-core's numbering) of the roles the shell uses.
fn role_name(role: u32) -> String {
    let name = match role {
        7 => "check box",
        8 => "check menu item",
        16 => "dialog",
        23 => "frame",
        29 => "label",
        31 => "list",
        32 => "list item",
        33 => "menu",
        35 => "menu item",
        39 => "panel",
        40 => "password text",
        43 => "push button",
        44 => "radio button",
        45 => "radio menu item",
        50 => "separator",
        54 => "status bar",
        62 => "toggle button",
        63 => "tool bar",
        64 => "tool tip",
        75 => "application",
        79 => "entry",
        98 => "list box",
        101 => "notification",
        115 => "timer",
        other => return format!("role {other}"),
    };
    name.to_owned()
}

fn read<'a>(conn: &'a zbus::Connection, at: ObjectRef) -> std::pin::Pin<Box<dyn std::future::Future<Output = Obj> + Send + 'a>> {
    Box::pin(async move {
        let p = proxy(conn, &at, "org.a11y.atspi.Accessible").await;
        let role = role_name(p.call("GetRole", &()).await.unwrap());
        let name: String = p.get_property("Name").await.unwrap();
        let description: String = p.get_property("Description").await.unwrap();
        let words: Vec<u32> = p.call("GetState", &()).await.unwrap();
        let states = u64::from(words.first().copied().unwrap_or(0)) | u64::from(words.get(1).copied().unwrap_or(0)) << 32;
        let refs: Vec<ObjectRef> = p.call("GetChildren", &()).await.unwrap();
        let mut children = Vec::new();
        for r in refs {
            children.push(read(conn, r).await);
        }
        Obj { at, role, name, description, states, children }
    })
}

/// The client side: the application and its windows.
struct Client {
    conn: zbus::Connection,
    app: String,
}

impl Client {
    async fn windows(&self) -> Vec<Obj> {
        let root = ObjectRef(self.app.clone(), OwnedObjectPath::try_from(ROOT).unwrap());
        read(&self.conn, root).await.children
    }

    /// Window `title` once `ready` accepts it.
    async fn window(&self, title: &str, ready: impl Fn(&Obj) -> bool) -> Obj {
        let mut last = None;
        for _ in 0..100 {
            if let Some(w) = self.windows().await.into_iter().find(|w| w.name == title) {
                if ready(&w) {
                    return w;
                }
                last = Some(w);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("window {title:?} not as expected: {:#?}", last.map(|w| w.outline()));
    }
}

/// The shell side: windows published under keys.
struct Shell {
    publisher: Publisher<&'static str>,
    events: mpsc::Receiver<Event>,
    shown: Vec<(String, Window)>,
}

impl Shell {
    fn sync(&mut self) {
        let shown: Vec<(String, &'static str)> = self.shown.iter().map(|(k, _)| (k.clone(), "surface")).collect();
        let windows = self.shown.clone();
        self.publisher.sync(shown, |key| windows.iter().find(|(k, _)| k == key).map(|(_, w)| w.clone()));
    }

    /// Show only `window` (under `key`), and let its adapter come up.
    fn show(&mut self, key: &str, window: Window) {
        self.shown = vec![(key.to_owned(), window)];
        self.sync();
        while let Ok(event) = self.events.recv_timeout(Duration::from_millis(400)) {
            assert!(self.publisher.event(event).is_none(), "no actions yet");
            self.sync();
        }
    }

    /// The next action's target and request.
    fn action(&mut self) -> (crate::a11y::tree::Target, accesskit::ActionRequest) {
        let event = self.events.recv_timeout(Duration::from_secs(10)).expect("an action");
        let (_, target, request) = self.publisher.event(event).expect("an action on a known node");
        (target, request)
    }
}

fn window(title: &str, active: bool, children: Vec<Node>) -> Window {
    Window { title: title.into(), bounds: crate::a11y::Rect::new(0, 0, 1280, 800), active, children }
}

/// Waits for an `object:state-changed:focused` (gained) event from `path`.
async fn focus_event(stream: &mut zbus::MessageStream, path: &str) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        let Ok(Some(Ok(msg))) = tokio::time::timeout(Duration::from_millis(300), futures_util::StreamExt::next(stream)).await else { continue };
        let header = msg.header();
        if header.member().is_none_or(|m| m.as_str() != "StateChanged") || header.path().is_none_or(|p| p.as_str() != path) {
            continue;
        }
        let Ok((what, gained, ..)) = msg.body().deserialize::<(String, i32, i32, zbus::zvariant::OwnedValue, HashMap<String, zbus::zvariant::OwnedValue>)>() else { continue };
        if what == "focused" && gained == 1 {
            return true;
        }
    }
    false
}

/// A canvas to paint on, for the places surfaces paint their parts at.
fn canvas_data(w: i32, h: i32) -> Vec<u8> {
    vec![0u8; (w * h * 4) as usize]
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "run by surfaces_over_atspi, in a process of its own"]
async fn child() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    let Some(session) = TestBus::start() else { return };
    let Some(a11y) = TestBus::start() else { return };
    std::env::set_var("DBUS_SESSION_BUS_ADDRESS", session.address());
    std::env::remove_var("AT_SPI_BUS_ADDRESS");
    let _bus = session.builder().serve_at("/org/a11y/bus", A11yBus { address: a11y.address().to_owned() }).unwrap().serve_at("/org/a11y/bus", Status).unwrap().name("org.a11y.Bus").unwrap().build().await.unwrap();
    let (embedded_tx, embedded) = mpsc::channel();
    let _registry = a11y.builder().serve_at(ROOT, Socket { embedded: Mutex::new(embedded_tx) }).unwrap().name("org.a11y.atspi.Registry").unwrap().build().await.unwrap();
    let (events_tx, events) = mpsc::channel();
    let events_tx = Mutex::new(events_tx);
    let publisher: Publisher<&'static str> = Publisher::new(Arc::new(move |event| {
        let _ = events_tx.lock().unwrap().send(event);
    }));
    let mut shell = Shell { publisher, events, shown: Vec::new() };
    let conn = a11y.connect().await;
    let focus_rule = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).interface("org.a11y.atspi.Event.Object").unwrap().build();
    let mut stream = zbus::MessageStream::for_match_rule(focus_rule, &conn, None).await.unwrap();

    // ----- Menus: the window menu, painted; keyboard selection moves.
    let items = window_menu::items(helix_protocols::client::helix_window_list_v1::MenuState::Above, Some(0), 2);
    let (w, h) = (800, 600);
    let mut data = canvas_data(w, h);
    let (_, hits) = window_menu::paint_menu(&mut crate::text::Canvas::scaled(&mut data, w as usize, h as usize, 1.0), None, (w, h), (100, 100), &items, Some(0), true, false);
    let menu = |selected: Option<usize>| window("Window Menu", true, vec![window_menu::menu_node("Window Menu", &items, &hits, selected, false, 0)]);
    shell.show("menu", menu(Some(0)));
    let app = embedded.recv_timeout(Duration::from_secs(10)).expect("application embedded");
    let client = Client { conn: conn.clone(), app };
    let w = client.window("Window Menu", |w| !w.focused().is_empty()).await;
    let m = &w.children[0];
    assert_eq!((m.role.as_str(), m.name.as_str()), ("menu", "Window Menu"), "{:#?}", w.outline());
    let roles: Vec<(&str, &str)> = m.children.iter().map(|c| (c.role.as_str(), c.name.as_str())).collect();
    assert_eq!(roles[..3], [("menu item", "Minimize"), ("menu item", "Maximize"), ("menu item", "Move")], "{roles:?}");
    assert!(roles.contains(&("separator", "")), "separators: {roles:?}");
    assert!(roles.contains(&("check menu item", "Always on Top")), "{roles:?}");
    assert!(m.named("Always on Top").has(CHECKED), "Always on Top is checked");
    assert!(!m.named("Always on Visible Workspace").has(CHECKED));
    assert!(m.named("Minimize").has(SELECTED | FOCUSED), "the selected entry has the focus");
    assert_eq!(w.focused(), ["Minimize"]);
    // Down twice: Move is selected, with a focus event.
    let move_path = m.named("Move").at.1.to_string();
    shell.show("menu", menu(Some(2)));
    assert!(focus_event(&mut stream, &move_path).await, "focus event for Move");
    let w = client.window("Window Menu", |w| w.focused() == ["Move"]).await;
    assert!(!w.children[0].named("Minimize").has(SELECTED));
    // Doing its action clicks where it was painted.
    let action = proxy(&conn, &w.children[0].named("Close").at, "org.a11y.atspi.Action").await;
    assert!(action.call::<_, _, bool>("DoAction", &(0i32,)).await.unwrap());
    let (target, request) = shell.action();
    let close = items.iter().position(|i| i.as_ref().is_some_and(|i| i.label == "Close")).unwrap();
    assert_eq!((target.bounds, request.action), (hits.iter().find(|(_, i)| *i == close).unwrap().0.into(), accesskit::Action::Click));
    // An input source menu: radio items, the current one checked; a
    // submenu entry is expandable; nothing selected: the menu has focus.
    let radio = |label: &str, active| Some(Item { label: label.into(), choice: Choice::InputSource(0), checked: None, active, sub: Vec::new() });
    let items = vec![radio("English (US)", true), radio("German", false), None, Some(Item { label: "Input Mode".into(), choice: Choice::Submenu, checked: None, active: false, sub: vec![radio("Hiragana", true)] })];
    let mut data = canvas_data(800, 600);
    let (_, hits) = window_menu::paint_menu(&mut crate::text::Canvas::scaled(&mut data, 800, 600, 1.0), None, (800, 600), (0, 32), &items, None, false, true);
    shell.show("menu", window("Input Sources", true, vec![window_menu::menu_node("Input Sources", &items, &hits, None, false, 0)]));
    let w = client.window("Input Sources", |w| !w.focused().is_empty()).await;
    let m = &w.children[0];
    assert_eq!(w.focused(), ["Input Sources"], "the menu itself: {:#?}", w.outline());
    assert_eq!((m.named("English (US)").role.as_str(), m.named("English (US)").has(CHECKED)), ("radio menu item", true));
    assert!(!m.named("German").has(CHECKED));
    assert!(m.named("Input Mode").has(EXPANDABLE), "a submenu: {:#?}", w.outline());

    // ----- The overview: windows, workspaces, the dash; Tab moves focus.
    let r = |x, y| Rect { x, y, w: 200, h: 120 };
    let workspaces = [(r(400, 80), "Workspace 1".to_owned(), true), (r(620, 80), "Mail".to_owned(), false)];
    let windows = [(r(100, 300), 7, "Inbox".to_owned(), "Mail".to_owned()), (r(400, 300), 9, "~/src".to_owned(), "Terminal".to_owned())];
    let dash = [(r(500, 700), "org.gnome.Nautilus.desktop".to_owned(), "Files".to_owned(), true)];
    let overview = |region, dash_key| {
        let shown = super::overview::OverviewShown { search: Some(Rect { x: 440, y: 24, w: 400, h: 40 }), workspaces: &workspaces, windows: &windows, dash: &dash, selected: Some(9), region, workspace_key: Some(1), dash_key };
        window("Overview", true, super::overview::overview_nodes(&shown))
    };
    shell.show("overview", overview(super::overview::Region::Windows, None));
    let w = client.window("Overview", |w| !w.focused().is_empty()).await;
    let roles: Vec<(&str, &str)> = w.children.iter().map(|c| (c.role.as_str(), c.name.as_str())).collect();
    assert_eq!(roles, [("entry", "Search"), ("list box", "Workspaces"), ("list box", "Windows"), ("tool bar", "Dash")], "{:#?}", w.outline());
    let terminal = w.named("~/src");
    assert_eq!((terminal.role.as_str(), terminal.description.as_str()), ("list item", "Terminal"));
    assert!(terminal.has(SELECTED | FOCUSED), "the chosen window has the focus");
    assert!(w.named("Workspace 1").has(SELECTED), "the active workspace is selected");
    assert!(!w.named("Mail").has(SELECTED));
    assert_eq!((w.named("Files").role.as_str(), w.named("Files").description.as_str()), ("push button", "Running"));
    let mail_path = w.named("Mail").at.1.to_string();
    shell.show("overview", overview(super::overview::Region::Workspaces, None));
    assert!(focus_event(&mut stream, &mail_path).await, "Tab to the workspaces: focus event");
    let w = client.window("Overview", |w| w.focused() == ["Mail"]).await;
    assert!(w.named("~/src").has(SELECTED) && !w.named("~/src").has(FOCUSED), "the window stays chosen");
    shell.show("overview", overview(super::overview::Region::Dash, Some(0)));
    client.window("Overview", |w| w.focused() == ["Files"]).await;

    // ----- The workspace popup: the workspace announced, the active one selected.
    shell.show("workspace-popup", window("Workspace Switcher", false, super::workspace_popup::popup_nodes(&["Mail".into()], 3, 1, 260)));
    let w = client.window("Workspace Switcher", |w| !w.children.is_empty()).await;
    assert_eq!((w.children[0].role.as_str(), w.children[0].name.as_str()), ("status bar", "Workspace 2"));
    let list = &w.children[1];
    assert_eq!(list.children.iter().map(|c| (c.name.as_str(), c.has(SELECTED))).collect::<Vec<_>>(), [("Mail", false), ("Workspace 2", true), ("Workspace 3", false)]);

    // ----- The on-screen keyboard, painted: keys are buttons, Shift and
    // the page keys toggle buttons.
    let rows = super::osk::layout_rows("us", "");
    let bar = vec!["hello".to_owned(), "help".to_owned()];
    let paint = super::osk::OskPaint { rows: &rows, page: super::osk::Page::Letters, shift: true, bar: &bar, accents: false, down: None };
    let mut data = canvas_data(1280, 400);
    let (hits, bar_hits) = super::osk::paint_keyboard(&mut crate::text::Canvas::scaled(&mut data, 1280, 400, 1.0), (None, None), &[], &paint, (1280, 400));
    shell.show("osk", window("On-screen Keyboard", false, super::osk::keyboard_nodes(&paint, &hits, &bar_hits)));
    let w = client.window("On-screen Keyboard", |w| w.children.len() == 2).await;
    assert_eq!(w.children[0].children.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["hello", "help"]);
    let keys = &w.children[1];
    assert_eq!((keys.role.as_str(), keys.name.as_str()), ("panel", "Keyboard"));
    assert_eq!(w.named("Q").role, "push button", "shifted: the capital letter");
    let shift = w.named("Shift");
    assert_eq!(shift.role, "toggle button");
    assert!(shift.has(PRESSED), "Shift is latched");
    let symbols = w.named("Symbols");
    assert_eq!((symbols.role.as_str(), symbols.has(PRESSED)), ("toggle button", false));
    for name in ["Backspace", "Enter", "Tab", "Space", "Hide Keyboard"] {
        assert_eq!(w.named(name).role, "push button", "{name}");
    }

    // ----- The click type window: radio buttons, the current one checked.
    let hits: Vec<(Rect, usize)> = (0..4).map(|i| (Rect { x: 6, y: 6 + i as i32 * 50, w: 96, h: 44 }, i)).collect();
    shell.show("click-type", window("Click Type", false, super::click_type::click_type_nodes("double", &hits)));
    let w = client.window("Click Type", |w| !w.children.is_empty()).await;
    let types: Vec<(&str, &str, bool)> = w.children[0].children.iter().map(|c| (c.role.as_str(), c.name.as_str(), c.has(CHECKED))).collect();
    assert_eq!(types, [("radio button", "Single", false), ("radio button", "Double", true), ("radio button", "Drag", false), ("radio button", "Secondary", false)]);

    // ----- Desktop icons.
    let item = |name: &str, folder, launcher| crate::desktop_icons::DesktopItem { path: format!("/home/u/Desktop/{name}").into(), name: name.into(), icon: crate::icons::IconSource::Names(Vec::new()), folder, launcher };
    let icons = [item("Projects", true, false), item("notes.txt", false, false), item("Editor", false, true)];
    let cells = crate::desktop_icons::cells(icons.len(), 1280, 800);
    shell.show("desktop", window("Desktop", false, super::desktop_icons::icon_nodes(&icons, &cells, Some(std::path::Path::new("/home/u/Desktop/notes.txt")))));
    let w = client.window("Desktop", |w| !w.children.is_empty()).await;
    let shown: Vec<(&str, &str, &str, bool)> = w.children[0].children.iter().map(|c| (c.role.as_str(), c.name.as_str(), c.description.as_str(), c.has(SELECTED))).collect();
    assert_eq!(shown, [("list item", "Projects", "Folder", false), ("list item", "notes.txt", "", true), ("list item", "Editor", "Application", false)]);

    // ----- The screenshot UI's tool bar.
    let tools = super::capture::a11y_sample_tools();
    shell.show("screenshot", window("Screenshot", true, tools));
    let w = client.window("Screenshot", |w| !w.children.is_empty()).await;
    assert_eq!((w.named("Area").role.as_str(), w.named("Area").has(CHECKED)), ("radio button", true));
    assert!(!w.named("Window").has(SENSITIVE), "Window cannot be picked while recording");
    assert_eq!((w.named("Show Pointer").role.as_str(), w.named("Show Pointer").has(PRESSED)), ("toggle button", true));
    assert_eq!(w.named("Start Recording").role, "push button");

    // ----- The break screen.
    let view = crate::wellbeing::BreakView { title: "Time for a break".into(), message: "Look away from the screen.".into(), left: Some(299) };
    let hits = vec![(Rect { x: 500, y: 500, w: 120, h: 40 }, super::OverlayHit::Cancel), (Rect { x: 636, y: 500, w: 120, h: 40 }, super::OverlayHit::Confirm)];
    shell.show("break", window("Time for a break", true, super::break_screen::break_nodes(&view, &hits)));
    let w = client.window("Time for a break", |w| !w.children.is_empty()).await;
    let d = &w.children[0];
    assert_eq!(d.role, "dialog");
    assert!(d.has(MODAL));
    assert_eq!(d.children.iter().map(|c| (c.role.as_str(), c.name.as_str())).collect::<Vec<_>>(), [("label", "Look away from the screen."), ("timer", "4:59"), ("push button", "Delay"), ("push button", "Skip")]);

    // ----- "Customize Panel…": columns of items with their buttons; Tab focus.
    use super::panel_editor::EditorHit;
    let row = |id: &str, name: &str| crate::panel_layout::Row { index: 0, id: id.into(), name: name.into() };
    let columns = vec![vec![row("helix:apps", "Apps")], vec![row("helix:clock", "Clock")], vec![], vec![]];
    let hidden = vec![(2, "helix:keyboard".to_owned(), "Input Source".to_owned())];
    let b = |x| Rect { x, y: 100, w: 24, h: 24 };
    let hits = vec![(b(10), EditorHit::Up(0, 0)), (b(34), EditorHit::Down(0, 0)), (b(58), EditorHit::Right(0, 0)), (b(82), EditorHit::Remove(0, 0)), (b(300), EditorHit::Up(1, 0)), (b(400), EditorHit::Add(0)), (b(10), EditorHit::Reset), (b(500), EditorHit::Done)];
    let editor = |focus| window("Customize Panel", true, super::panel_editor::editor_nodes(&columns, &hidden, &hits, focus, None));
    shell.show("panel-editor", editor(Some(1)));
    let w = client.window("Customize Panel", |w| !w.focused().is_empty()).await;
    let d = &w.children[0];
    assert!(d.has(MODAL));
    let lists: Vec<(&str, &str)> = d.children.iter().map(|c| (c.role.as_str(), c.name.as_str())).collect();
    assert_eq!(lists, [("list", "Left"), ("list", "Center"), ("list", "Right"), ("list", "Quick Settings"), ("list", "Hidden"), ("push button", "Reset"), ("push button", "Done")]);
    let apps = d.named("Apps");
    assert_eq!(apps.children.iter().map(|c| (c.name.as_str(), c.description.as_str())).collect::<Vec<_>>(), [("Move Up", "Apps"), ("Move Down", "Apps"), ("Move Right", "Apps"), ("Remove", "Apps")]);
    assert!(apps.children[1].has(FOCUSED), "Tab's button has the focus");
    shell.show("panel-editor", editor(Some(5)));
    let w = client.window("Customize Panel", |w| w.focused() == ["Add"]).await;
    assert_eq!(w.named("Add").description, "Input Source");

    // ----- The Extensions dialog.
    use crate::ext_host::manifest::{SettingDecl, SettingType, SettingValue};
    let setting = |key: &str, label: &str, ty, value| (SettingDecl { key: key.into(), ty, default: SettingValue::Bool(false), min: None, max: None, label: label.into(), help: String::new() }, value);
    let info = crate::ext_host::Info {
        id: "clock@example".into(),
        name: "World Clock".into(),
        description: "Clocks of other places.".into(),
        version: "1.2".into(),
        path: "/x".into(),
        kind: crate::ext_host::store::Kind::User,
        enabled: true,
        approved: true,
        state: crate::ext_host::State::Running,
        permissions: Vec::new(),
        error: String::new(),
        settings: vec![setting("seconds", "Show seconds", SettingType::Bool, SettingValue::Bool(true)), setting("size", "Size", SettingType::Int, SettingValue::Int(3))],
        commands: vec![("show".into(), "Show the clocks".into(), "<Super>w".into())],
        sandboxed: true,
    };
    use super::extensions_dialog::DialogHit;
    let b = |y| Rect { x: 600, y, w: 44, h: 22 };
    let hits = vec![(b(100), DialogHit::Switch(0)), (b(140), DialogHit::Settings(0)), (b(180), DialogHit::Flip(0, 0)), (b(220), DialogHit::Down(0, 1)), (b(220), DialogHit::Up(0, 1)), (b(700), DialogHit::Done)];
    shell.show("extensions", window("Extensions", true, super::extensions_dialog::extension_nodes(&[info], &[0], Some("clock@example"), &hits, Some(1), false)));
    let w = client.window("Extensions", |w| !w.focused().is_empty()).await;
    let entry = w.named("World Clock 1.2");
    assert_eq!(entry.role, "list item");
    assert_eq!(entry.description, "Clocks of other places. Running. Sandboxed", "{:#?}", w.outline());
    let parts: Vec<(&str, &str)> = entry.children.iter().map(|c| (c.role.as_str(), c.name.as_str())).collect();
    assert_eq!(parts, [("toggle button", "World Clock"), ("push button", "Settings"), ("label", "Show the clocks: <Super>w"), ("toggle button", "Show seconds"), ("push button", "Decrease"), ("push button", "Increase")]);
    assert!(entry.children[0].has(PRESSED), "on");
    assert!(entry.children[1].has(FOCUSED | EXPANDABLE), "Settings: focused, open");
    assert_eq!(entry.children[5].description, "Size: 3");
    assert_eq!(w.named("Done").role, "push button");

    // ----- The OSD: a live status whose changes reach clients.
    let osd = |words: &str| window("On-screen display", false, vec![Node::new(Role::Status, words).id("osd").live()]);
    shell.show("osd", osd("Volume 40%"));
    let w = client.window("On-screen display", |w| !w.children.is_empty()).await;
    assert_eq!((w.children[0].role.as_str(), w.children[0].name.as_str()), ("status bar", "Volume 40%"));
    let status_path = w.children[0].at.1.to_string();
    shell.show("osd", osd("Volume 45%"));
    let mut changed = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !changed && std::time::Instant::now() < deadline {
        let Ok(Some(Ok(msg))) = tokio::time::timeout(Duration::from_millis(300), futures_util::StreamExt::next(&mut stream)).await else { continue };
        let header = msg.header();
        changed = header.path().is_some_and(|p| p.as_str() == status_path) && header.member().is_some_and(|m| m.as_str() == "PropertyChange");
    }
    assert!(changed, "the new level reaches clients as a change of the live status");

    // ----- Panel tooltips and monitor labels.
    shell.show("tooltip", window("Tooltip", false, super::panel_tooltip::tooltip_nodes(&["Mail".into(), "3 unread".into()], 120, 60)));
    let w = client.window("Tooltip", |w| !w.children.is_empty()).await;
    assert_eq!((w.children[0].role.as_str(), w.children[0].name.as_str(), w.children[0].description.as_str()), ("tool tip", "Mail", "3 unread"));

    // ----- Calendar events: times and lengths as descriptions.
    let time = |t: i64| format!("{:02}:{:02}", t / 3600 % 24, t / 60 % 60);
    let e = crate::ical::Occurrence { start: 36_000, end: 41_400, all_day: false, summary: "Standup".into() };
    let when = super::calendar::event_when(&e, &time);
    shell.show("notifications", window("Notifications", true, vec![Node::new(Role::List, "Events").id("events").child(Node::new(Role::ListItem, &e.summary).describe(when))]));
    let w = client.window("Notifications", |w| !w.children.is_empty()).await;
    assert_eq!(w.named("Standup").description, "10:00 to 11:30, 1 hour 30 minutes");

    // ----- Text fields: character extents, and edits that reach the field.
    let secret = "abc";
    let field = crate::a11y::Rect::new(100, 200, 300, 34);
    let dots = crate::a11y::Chars::even(112.0, 200, 34, 12.0, 3);
    let typed = crate::a11y::Chars { x: 110.0, y: 200, h: 34, advances: vec![8.0, 9.0] };
    shell.show(
        "dialog",
        window(
            "Authentication Required",
            true,
            vec![Node::new(Role::Dialog, "Authentication Required").modal().children([
                Node::text_field(Role::PasswordInput, "Password", Node::mask(secret), field).id("password:0").chars(dots).focused(true),
                Node::text_field(Role::TextInput, "Command", "ls", crate::a11y::Rect::new(100, 260, 300, 34)).id("entry").chars(typed),
            ])],
        ),
    );
    let w = client.window("Authentication Required", |w| !w.focused().is_empty()).await;
    let password = w.named("Password");
    assert_eq!(password.role, "password text");
    let text = proxy(&conn, &password.at, "org.a11y.atspi.Text").await;
    assert_eq!(text.call::<_, _, String>("GetText", &(0i32, -1i32)).await.unwrap(), "•••", "the password itself is never published");
    // Screen coordinates (0); the window is at the origin.
    let extents: (i32, i32, i32, i32) = text.call("GetCharacterExtents", &(1i32, 0u32)).await.unwrap();
    assert_eq!(extents, (124, 200, 12, 34), "the second dot");
    let range: (i32, i32, i32, i32) = text.call("GetRangeExtents", &(0i32, 3i32, 0u32)).await.unwrap();
    assert_eq!(range, (112, 200, 36, 34));
    let command = proxy(&conn, &w.named("Command").at, "org.a11y.atspi.Text").await;
    let extents: (i32, i32, i32, i32) = command.call("GetCharacterExtents", &(1i32, 0u32)).await.unwrap();
    assert_eq!(extents, (118, 200, 9, 34), "the second letter");
    // Inserting into the password field: the range, then the new text.
    let editable = proxy(&conn, &password.at, "org.a11y.atspi.EditableText").await;
    assert!(editable.call::<_, _, bool>("InsertText", &(1i32, "x", 1i32)).await.unwrap());
    let (target, request) = shell.action();
    let accesskit::ActionData::SetTextSelection(sel) = request.data.clone().expect("a range") else { panic!("{request:?}") };
    assert_eq!((target.id.as_str(), request.action, sel.anchor.character_index, sel.focus.character_index), ("password:0", accesskit::Action::SetTextSelection, 1, 1));
    let (_, request) = shell.action();
    let Some(accesskit::ActionData::Value(value)) = request.data.clone() else { panic!("{request:?}") };
    assert_eq!((request.action, super::atspi::edited(secret, true, &value, Some((1, 1))).as_str()), (accesskit::Action::SetValue, "axbc"));
    // Deleting the first character.
    assert!(editable.call::<_, _, bool>("DeleteText", &(0i32, 1i32)).await.unwrap());
    let (_, request) = shell.action();
    assert_eq!(request.action, accesskit::Action::SetTextSelection);
    let (_, request) = shell.action();
    let Some(accesskit::ActionData::Value(value)) = request.data.clone() else { panic!("{request:?}") };
    assert_eq!(super::atspi::edited(secret, true, &value, Some((0, 1))), "bc");
    // Setting it all.
    assert!(editable.call::<_, _, bool>("SetTextContents", &("hunter2",)).await.unwrap());
    let (target, request) = shell.action();
    assert_eq!((target.id.as_str(), request.data), ("password:0", Some(accesskit::ActionData::Value("hunter2".into()))));

    // The windows leave with their surfaces.
    shell.shown.clear();
    shell.sync();
    for _ in 0..50 {
        if client.windows().await.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("windows left behind");
}
