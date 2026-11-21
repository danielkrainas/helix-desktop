//! Dragging a maximized window restores it under the pointer
//! (helixdesktop/spec `contracts/wayland/README.md`, "Moving windows"):
//! the pointer keeps its place on the title bar (the same distance below
//! the top, the same fraction of the width), whichever way the drag
//! started (a client-side title bar's `xdg_toplevel.move`, Helix's own
//! title bar on an X11 window, or a Super+drag), on one monitor or across
//! two side by side, with client-side shadows (an `xdg_surface` window
//! geometry inset in the buffer) or none, and for every window in turn.
//!
//! Before the fix a `move` request restored the window to where it was
//! before it was maximized: the pointer ended up off its title bar, by
//! more for each new window (new windows cascade down and right).
//!
//! Runs the real `helix-comp` headless; needs `dbus-daemon` and Xwayland
//! (as every headless session), skipped otherwise.

use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use helix_test_support::headless::{HeadlessSession, Options};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_buffer, wl_compositor, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

/// The size of each virtual monitor.
const SIZE: (i32, i32) = (1280, 800);
/// A restored window's size.
const RESTORED: (i32, i32) = (400, 300);

#[derive(Default)]
struct State {
    /// The surface under the pointer and where on it (surface-local).
    pointer: Option<(wl_surface::WlSurface, f64, f64)>,
    /// The serial of the last button press.
    press: Option<u32>,
    /// The last toplevel configure: size and whether maximized.
    configure: Option<((i32, i32), bool)>,
    /// A configure to ack.
    pending: Option<u32>,
}

macro_rules! quiet {
    ($($iface:ty),*) => {$(
        impl Dispatch<$iface, ()> for State {
            fn event(_: &mut Self, _: &$iface, _: <$iface as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
quiet!(wl_compositor::WlCompositor, wl_surface::WlSurface, wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_buffer::WlBuffer, wl_seat::WlSeat);

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(_: &mut Self, base: &xdg_wm_base::XdgWmBase, event: xdg_wm_base::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            base.pong(serial);
        }
    }
}
impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
    fn event(state: &mut Self, _: &xdg_toplevel::XdgToplevel, event: xdg_toplevel::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_toplevel::Event::Configure { width, height, states } = event {
            let maximized = xdg_toplevel::State::Maximized as u32;
            let max = states.as_chunks::<4>().0.iter().any(|c| u32::from_ne_bytes(*c) == maximized);
            state.configure = Some(((width, height), max));
        }
    }
}
impl Dispatch<xdg_surface::XdgSurface, ()> for State {
    fn event(state: &mut Self, _: &xdg_surface::XdgSurface, event: xdg_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_surface::Event::Configure { serial } = event {
            state.pending = Some(serial);
        }
    }
}
impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(state: &mut Self, _: &wl_pointer::WlPointer, event: wl_pointer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_pointer::Event::Enter { surface, surface_x, surface_y, .. } => state.pointer = Some((surface, surface_x, surface_y)),
            wl_pointer::Event::Leave { .. } => state.pointer = None,
            wl_pointer::Event::Motion { surface_x, surface_y, .. } => {
                if let Some(p) = state.pointer.as_mut() {
                    (p.1, p.2) = (surface_x, surface_y);
                }
            }
            wl_pointer::Event::Button { serial, state: wayland_client::WEnum::Value(wl_pointer::ButtonState::Pressed), .. } => state.press = Some(serial),
            _ => {}
        }
    }
}

struct Client {
    queue: EventQueue<State>,
    qh: QueueHandle<State>,
    state: State,
    compositor: wl_compositor::WlCompositor,
    shm: wl_shm::WlShm,
    base: xdg_wm_base::XdgWmBase,
    seat: wl_seat::WlSeat,
}

/// One toplevel that draws itself at whatever size it is configured to,
/// with `shadow` pixels of (client-side) shadow around its window
/// geometry, as GTK does.
struct Toplevel {
    surface: wl_surface::WlSurface,
    xdg: xdg_surface::XdgSurface,
    toplevel: xdg_toplevel::XdgToplevel,
    shadow: i32,
    size: (i32, i32),
}

fn buffer(shm: &wl_shm::WlShm, qh: &QueueHandle<State>, (w, h): (i32, i32)) -> wl_buffer::WlBuffer {
    let bytes: Vec<u8> = std::iter::repeat_n(0xFF80_8080u32.to_le_bytes(), (w * h) as usize).flatten().collect();
    // SAFETY: memfd_create returns a new fd we own (or -1).
    let raw = unsafe { libc::memfd_create(c"move-test".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(raw >= 0);
    // SAFETY: just created.
    let fd = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(raw) };
    std::io::Write::write_all(&mut std::fs::File::from(fd.try_clone().unwrap()), &bytes).unwrap();
    let pool = shm.create_pool(fd.as_fd(), bytes.len() as i32, qh, ());
    let buffer = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, qh, ());
    pool.destroy();
    buffer
}

impl Client {
    fn connect(session: &HeadlessSession) -> Self {
        let conn = Connection::from_socket(UnixStream::connect(&session.wayland_display).unwrap()).unwrap();
        let (globals, queue) = registry_queue_init::<State>(&conn).unwrap();
        let qh = queue.handle();
        let seat: wl_seat::WlSeat = globals.bind(&qh, 5..=9, ()).unwrap();
        seat.get_pointer(&qh, ());
        Client {
            compositor: globals.bind(&qh, 4..=6, ()).unwrap(),
            shm: globals.bind(&qh, 1..=1, ()).unwrap(),
            base: globals.bind(&qh, 1..=6, ()).unwrap(),
            seat,
            queue,
            qh,
            state: State::default(),
        }
    }

    /// Handle events, answering a configure by drawing at its size.
    fn settle(&mut self, t: &mut Toplevel) {
        for _ in 0..3 {
            self.queue.roundtrip(&mut self.state).unwrap();
            let Some(serial) = self.state.pending.take() else { continue };
            t.xdg.ack_configure(serial);
            if let Some(((w, h), _)) = self.state.configure {
                if w > 0 && h > 0 {
                    t.size = (w, h);
                }
            }
            let s = t.shadow;
            let full = (t.size.0 + 2 * s, t.size.1 + 2 * s);
            t.xdg.set_window_geometry(s, s, t.size.0, t.size.1);
            t.surface.attach(Some(&buffer(&self.shm, &self.qh, full)), 0, 0);
            t.surface.damage_buffer(0, 0, full.0, full.1);
            t.surface.commit();
        }
    }

    fn until(&mut self, t: &mut Toplevel, what: &str, done: impl Fn(&State) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            self.settle(t);
            if done(&self.state) {
                return;
            }
            assert!(Instant::now() < deadline, "{what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn toplevel(&mut self, shadow: i32) -> Toplevel {
        let surface = self.compositor.create_surface(&self.qh, ());
        let xdg = self.base.get_xdg_surface(&surface, &self.qh, ());
        let toplevel = xdg.get_toplevel(&self.qh, ());
        toplevel.set_title("move test".into());
        surface.commit();
        let mut t = Toplevel { surface, xdg, toplevel, shadow, size: RESTORED };
        self.state.configure = None;
        self.until(&mut t, "no first configure", |s| s.configure.is_some());
        t
    }

    /// Where the pointer is on `t` (window geometry coordinates), if on it.
    fn on(&mut self, session: &mut HeadlessSession, t: &mut Toplevel, x: f64, y: f64) -> Option<(f64, f64)> {
        // A motion off and back makes sure the client hears where it is.
        session.input(&format!("move {} {}", x + 1.0, y));
        session.input(&format!("move {x} {y}"));
        self.settle(t);
        let (surface, sx, sy) = self.state.pointer.clone()?;
        (surface == t.surface).then(|| (sx - f64::from(t.shadow), sy - f64::from(t.shadow)))
    }
}

/// How a drag starts.
#[derive(Debug, Clone, Copy)]
enum Start {
    /// The client's own title bar: it asks with `xdg_toplevel.move`.
    MoveRequest,
    /// Super held, anywhere on the window.
    SuperDrag,
}

fn session(outputs: u32) -> Option<HeadlessSession> {
    let comp = std::path::PathBuf::from(env!("CARGO_BIN_EXE_helix-comp"));
    HeadlessSession::start(Options {
        comp: &comp,
        shell: None,
        size: (SIZE.0 as u32, SIZE.1 as u32),
        // No edge tiling: a drop near an edge must not tile.
        settings: "[org/gnome/mutter]\nedge-tiling=false\n",
        env: vec![("HELIX_HEADLESS_OUTPUTS".into(), outputs.to_string())],
    })
}

/// Maximize a new window, grab it at `grab` (window coordinates of the
/// maximized window), drag the pointer to `to`, and return where the
/// pointer is on the restored window once dropped.
fn drag_maximized(session: &mut HeadlessSession, c: &mut Client, shadow: i32, start: Start, grab: (f64, f64), to: (f64, f64)) -> (f64, f64) {
    // New windows open on the monitor with the pointer: the first one.
    session.input("move 5 5");
    let mut t = c.toplevel(shadow);
    t.toplevel.set_maximized();
    c.until(&mut t, "not maximized", |s| s.configure.is_some_and(|(_, m)| m));
    c.settle(&mut t);
    assert_eq!(t.size, SIZE, "maximized to the first monitor (no panel)");
    let at = c.on(session, &mut t, grab.0, grab.1);
    let at = at.unwrap_or_else(|| panic!("the pointer is not on the maximized window: {:?}\n{}", c.state.pointer, tail(session)));
    assert_eq!(at, grab, "the maximized window fills the first monitor");

    if let Start::SuperDrag = start {
        session.input("key 125 1");
    }
    session.input("button 272 1");
    c.settle(&mut t);
    if let Start::MoveRequest = start {
        let serial = c.state.press.expect("the press reached the window");
        t.toplevel._move(&c.seat, serial);
        c.settle(&mut t);
    }
    let steps = 12;
    for i in 1..=steps {
        let f = f64::from(i) / f64::from(steps);
        session.input(&format!("move {} {}", grab.0 + (to.0 - grab.0) * f, grab.1 + (to.1 - grab.1) * f));
        // The client answers the restore configure while it is dragged.
        c.settle(&mut t);
    }
    session.input("button 272 0");
    if let Start::SuperDrag = start {
        session.input("key 125 0");
    }
    c.settle(&mut t);
    assert!(c.state.configure.is_some_and(|(_, m)| !m), "the drag restored the window");
    assert_eq!(t.size, RESTORED, "restored to its own size");
    let on = c.on(session, &mut t, to.0, to.1);
    let on = on.unwrap_or_else(|| panic!("{start:?} (shadow {shadow}): the pointer at {to:?} is off the dropped window\n{}", tail(session)));
    t.toplevel.destroy();
    t.xdg.destroy();
    t.surface.destroy();
    c.settle_none();
    on
}

impl Client {
    fn settle_none(&mut self) {
        self.queue.roundtrip(&mut self.state).unwrap();
    }
}

fn tail(session: &HeadlessSession) -> String {
    let log = session.log();
    log.lines().rev().take(40).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n")
}

/// The pointer kept its place on the title bar: as far below the top,
/// and the same fraction of the width (within a pixel).
fn assert_anchored(what: &str, grab: (f64, f64), on: (f64, f64)) {
    let x = grab.0 / f64::from(SIZE.0) * f64::from(RESTORED.0);
    assert!((on.1 - grab.1).abs() <= 1.0, "{what}: the pointer is {} below the top, grabbed {} below it", on.1, grab.1);
    assert!((on.0 - x).abs() <= 1.0, "{what}: the pointer is {} from the left, expected {x}", on.0);
}

#[test]
fn title_bar_drag_across_monitors_keeps_the_pointer_on_the_title_bar() {
    let Some(mut session) = session(2) else { return };
    let mut c = Client::connect(&session);
    // Several windows in turn (new windows cascade), with and without
    // client-side shadows.
    for (i, shadow) in [0, 24, 0, 24, 0].into_iter().enumerate() {
        let grab = (900.0, 12.0);
        let to = (f64::from(SIZE.0) + 500.0, 200.0 + 10.0 * i as f64);
        let on = drag_maximized(&mut session, &mut c, shadow, Start::MoveRequest, grab, to);
        assert_anchored(&format!("window {i} (shadow {shadow})"), grab, on);
    }
}

#[test]
fn title_bar_drag_on_one_monitor_keeps_the_pointer_on_the_title_bar() {
    let Some(mut session) = session(1) else { return };
    let mut c = Client::connect(&session);
    for (i, shadow) in [24, 0, 24].into_iter().enumerate() {
        let grab = (300.0, 20.0);
        let to = (700.0, 400.0);
        let on = drag_maximized(&mut session, &mut c, shadow, Start::MoveRequest, grab, to);
        assert_anchored(&format!("window {i} (shadow {shadow})"), grab, on);
    }
}

/// Super+drag grabs anywhere: lower than the restored window is tall, the
/// pointer keeps its fraction of the height too.
#[test]
fn super_drag_of_a_maximized_window_keeps_it_under_the_pointer() {
    let Some(mut session) = session(2) else { return };
    let mut c = Client::connect(&session);
    for (i, shadow) in [0, 24].into_iter().enumerate() {
        let grab = (1000.0, 700.0);
        let to = (f64::from(SIZE.0) + 600.0, 500.0);
        let on = drag_maximized(&mut session, &mut c, shadow, Start::SuperDrag, grab, to);
        let expect = (grab.0 / f64::from(SIZE.0) * f64::from(RESTORED.0), grab.1 / f64::from(SIZE.1) * f64::from(RESTORED.1));
        assert!((on.0 - expect.0).abs() <= 1.0 && (on.1 - expect.1).abs() <= 1.0, "window {i} (shadow {shadow}): the pointer is at {on:?} on it, expected {expect:?}");
    }
}

/// An X11 window with the title bar Helix draws above it, maximized
/// (`_NET_WM_STATE`), dragged by that title bar to the other monitor:
/// restored under the pointer, every window in turn.
#[test]
fn framed_x11_title_bar_drag_keeps_the_pointer_on_the_title_bar() {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::{AtomEnum, ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, WindowClass};
    use x11rb::wrapper::ConnectionExt as _;

    /// Helix's title bar height (`helix_comp::frames::TITLE_H`).
    const TITLE_H: i32 = 34;
    let Some(mut session) = session(2) else { return };
    if session.x_display.is_empty() {
        eprintln!("no Xwayland; skipped");
        return;
    }
    std::env::set_var("XAUTHORITY", &session.x_authority);
    let (conn, screen_num) = x11rb::connect(Some(&session.x_display)).unwrap();
    let root = conn.setup().roots[screen_num].root;
    let atom = |name: &str| conn.intern_atom(false, name.as_bytes()).unwrap().reply().unwrap().atom;
    let (state, horz, vert) = (atom("_NET_WM_STATE"), atom("_NET_WM_STATE_MAXIMIZED_HORZ"), atom("_NET_WM_STATE_MAXIMIZED_VERT"));
    let rect = |window: u32| -> Option<(i32, i32, i32, i32)> {
        let g = conn.get_geometry(window).ok()?.reply().ok()?;
        let t = conn.translate_coordinates(window, root, 0, 0).ok()?.reply().ok()?;
        Some((i32::from(t.dst_x), i32::from(t.dst_y), i32::from(g.width), i32::from(g.height)))
    };
    let wait = |what: &dyn Fn() -> String, f: &dyn Fn() -> bool| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !f() {
            assert!(Instant::now() < deadline, "{}", what());
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    for i in 0..4 {
        session.input("move 5 5");
        let window = conn.generate_id().unwrap();
        let (w, h) = (RESTORED.0 as u16, RESTORED.1 as u16);
        let aux = CreateWindowAux::new().background_pixel(0x80_8080).event_mask(EventMask::STRUCTURE_NOTIFY);
        conn.create_window(x11rb::COPY_DEPTH_FROM_PARENT, window, root, 0, 0, w, h, 0, WindowClass::INPUT_OUTPUT, 0, &aux).unwrap();
        conn.change_property8(PropMode::REPLACE, window, AtomEnum::WM_NAME, AtomEnum::STRING, b"framed").unwrap();
        conn.map_window(window).unwrap();
        conn.flush().unwrap();
        wait(&|| format!("window {i} is not placed: {:?}", rect(window)), &|| rect(window).is_some_and(|r| r.1 >= TITLE_H));
        // _NET_WM_STATE add (1) both maximized states.
        let message = ClientMessageEvent::new(32, window, state, [1, horz, vert, 1, 0]);
        conn.send_event(false, root, EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY, message).unwrap();
        conn.flush().unwrap();
        let maximized = (0, TITLE_H, SIZE.0, SIZE.1 - TITLE_H);
        wait(&|| format!("window {i} is not maximized: {:?}", rect(window)), &|| rect(window) == Some(maximized));

        // Grab Helix's title bar (above the window), drag to the other monitor.
        let grab = (900.0, 12.0);
        let to = (f64::from(SIZE.0) + 500.0, 200.0 + 10.0 * f64::from(i));
        session.input(&format!("move {} {}", grab.0, grab.1));
        session.input("button 272 1");
        for s in 1..=12 {
            let f = f64::from(s) / 12.0;
            session.input(&format!("move {} {}", grab.0 + (to.0 - grab.0) * f, grab.1 + (to.1 - grab.1) * f));
        }
        session.input("button 272 0");
        let expect = ((to.0 - grab.0 / f64::from(SIZE.0) * f64::from(RESTORED.0)).round() as i32, (to.1 - grab.1).round() as i32 + TITLE_H);
        wait(&|| format!("window {i}: not restored at {expect:?} under the pointer: {:?}", rect(window)), &|| {
            rect(window).is_some_and(|(x, y, w, h)| (x - expect.0).abs() <= 1 && y == expect.1 && (w, h) == RESTORED)
        });
        conn.destroy_window(window).unwrap();
        conn.flush().unwrap();
    }
}
