//! New windows asking to be maximized, in-process: the real compositor
//! state (no backend, one monitor) and a Wayland client on a thread whose
//! window geometry is inset in its buffer (a shadow margin, as Chromium
//! and Electron draw while not maximized).

use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::sync::{mpsc, Arc, Once};
use std::time::{Duration, Instant};

use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::Display;
use smithay::utils::{Logical, Point, Rectangle, Size, Transform};

use super::{ClientState, Helix};

/// Where the monitor is (not at 0,0, so offsets show).
const ORIGIN: (i32, i32) = (100, 50);
const SIZE: (i32, i32) = (1280, 720);
/// The client's shadow margin around its window geometry.
const INSET: (i32, i32) = (24, 16);

fn runtime_dir() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let dir = std::env::temp_dir().join(format!("helix-comp-placement-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // SAFETY: once, before any thread of these tests reads it.
        std::env::set_var("XDG_RUNTIME_DIR", dir);
    });
}

fn start() -> (EventLoop<'static, Helix>, Helix) {
    runtime_dir();
    let mut event_loop: EventLoop<'static, Helix> = EventLoop::try_new().unwrap();
    let display: Display<Helix> = Display::new().unwrap();
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let mut state = Helix::new(&mut event_loop, display, events).unwrap();
    let output = Output::new(
        "TEST-1".into(),
        PhysicalProperties { size: (0, 0).into(), subpixel: Subpixel::Unknown, make: "Helix".into(), model: "Test".into() },
    );
    let _ = output.create_global::<Helix>(&state.display_handle);
    let mode = Mode { size: SIZE.into(), refresh: 60_000 };
    output.change_current_state(Some(mode), Some(Transform::Normal), None, Some(ORIGIN.into()));
    output.set_preferred(mode);
    state.space.map_output(&output, ORIGIN);
    // A later window: new windows cascade away from the area's corner.
    state.windows_mapped = 3;
    (event_loop, state)
}

fn work_area() -> Rectangle<i32, Logical> {
    Rectangle::new(ORIGIN.into(), SIZE.into())
}

/// Run the compositor until the client reports `what`.
fn serve<T>(event_loop: &mut EventLoop<'static, Helix>, state: &mut Helix, from_client: &mpsc::Receiver<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        event_loop.dispatch(Some(Duration::from_millis(2)), state).unwrap();
        let _ = state.display_handle.flush_clients();
        if let Ok(value) = from_client.try_recv() {
            return value;
        }
        assert!(Instant::now() < deadline, "the client did not get on");
    }
}

/// The one window, its client area, and whether it is maximized.
fn window(state: &Helix) -> (Rectangle<i32, Logical>, Point<i32, Logical>, bool) {
    let window = state.space.elements().next().expect("a window").clone();
    let area = state.space.element_geometry(&window).unwrap();
    let maximized = window.toplevel().unwrap().current_state().states.contains(xdg_toplevel::State::Maximized);
    assert!(!state.wm.unplaced.contains(&window), "placed");
    (area, window.geometry().loc, maximized)
}

// ---- the client ----

mod client {
    use wayland_client::globals::GlobalListContents;
    use wayland_client::protocol::{wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface};
    use wayland_client::{Connection, Dispatch, QueueHandle};
    use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

    #[derive(Default, Debug, Clone)]
    pub struct State {
        /// The last configure: size, maximized, and its serial.
        pub size: (i32, i32),
        pub maximized: bool,
        pub serial: Option<u32>,
    }

    macro_rules! quiet {
        ($($iface:ty),*) => {$(
            impl Dispatch<$iface, ()> for State {
                fn event(_: &mut Self, _: &$iface, _: <$iface as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
            }
        )*};
    }
    quiet!(wl_compositor::WlCompositor, wl_surface::WlSurface, wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_buffer::WlBuffer);

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
    impl Dispatch<xdg_surface::XdgSurface, ()> for State {
        fn event(state: &mut Self, _: &xdg_surface::XdgSurface, event: xdg_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
            if let xdg_surface::Event::Configure { serial } = event {
                state.serial = Some(serial);
            }
        }
    }
    impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
        fn event(state: &mut Self, _: &xdg_toplevel::XdgToplevel, event: xdg_toplevel::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
            if let xdg_toplevel::Event::Configure { width, height, states } = event {
                state.size = (width, height);
                state.maximized = states
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| u32::from_ne_bytes(*c))
                    .any(|s| matches!(xdg_toplevel::State::try_from(s), Ok(xdg_toplevel::State::Maximized)));
            }
        }
    }
}

use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_buffer, wl_shm, wl_surface};
use wayland_client::{Connection, EventQueue, QueueHandle};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel as client_toplevel, xdg_wm_base};

/// A toplevel that draws a shadow margin of [`INSET`] around its window
/// geometry, as Chromium does while not maximized, tiled or fullscreen.
struct App {
    queue: EventQueue<client::State>,
    state: client::State,
    qh: QueueHandle<client::State>,
    shm: wl_shm::WlShm,
    surface: wl_surface::WlSurface,
    xdg: xdg_surface::XdgSurface,
    toplevel: client_toplevel::XdgToplevel,
}

impl App {
    /// Up to (not including) the initial commit.
    fn new(stream: UnixStream) -> Self {
        let conn = Connection::from_socket(stream).unwrap();
        let (globals, queue) = registry_queue_init::<client::State>(&conn).unwrap();
        let qh = queue.handle();
        let compositor: wayland_client::protocol::wl_compositor::WlCompositor = globals.bind(&qh, 1..=6, ()).unwrap();
        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
        let base: xdg_wm_base::XdgWmBase = globals.bind(&qh, 1..=6, ()).unwrap();
        let surface = compositor.create_surface(&qh, ());
        let xdg = base.get_xdg_surface(&surface, &qh, ());
        let toplevel = xdg.get_toplevel(&qh, ());
        toplevel.set_app_id("org.example.Shadowed".into());
        Self { queue, state: client::State::default(), qh, shm, surface, xdg, toplevel }
    }

    fn roundtrip(&mut self) {
        self.queue.roundtrip(&mut self.state).unwrap();
    }

    /// A `w`×`h` buffer (the margin is transparent).
    fn buffer(&self, w: i32, h: i32) -> wl_buffer::WlBuffer {
        let bytes = vec![0u8; (w * h * 4) as usize];
        // SAFETY: memfd_create returns a new fd we own (or -1).
        let raw = unsafe { libc::memfd_create(c"placement-test".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(raw >= 0);
        // SAFETY: just created.
        let fd = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(raw) };
        std::io::Write::write_all(&mut std::fs::File::from(fd.try_clone().unwrap()), &bytes).unwrap();
        let pool = self.shm.create_pool(fd.as_fd(), bytes.len() as i32, &self.qh, ());
        let buffer = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, &self.qh, ());
        pool.destroy();
        buffer
    }

    /// Answer the last configure and draw at its size (else `fallback`):
    /// with the shadow margin unless maximized.
    fn draw(&mut self, fallback: (i32, i32)) {
        let serial = self.state.serial.take().expect("a configure");
        self.xdg.ack_configure(serial);
        let (w, h) = if self.state.size.0 > 0 { self.state.size } else { fallback };
        let (ix, iy) = if self.state.maximized { (0, 0) } else { INSET };
        self.xdg.set_window_geometry(ix, iy, w, h);
        self.surface.attach(Some(&self.buffer(w + 2 * ix, h + 2 * iy)), 0, 0);
        self.surface.commit();
        self.roundtrip();
    }
}

/// Run `client` on a thread connected to the compositor; it reports
/// through the sender after each step.
fn connect<T: Send + 'static>(state: &mut Helix, client: impl FnOnce(UnixStream, mpsc::Sender<T>) + Send + 'static) -> mpsc::Receiver<T> {
    let (ours, theirs) = UnixStream::pair().unwrap();
    let data = Arc::new(ClientState::connected(&ours));
    state.display_handle.insert_client(ours, data).unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || client(theirs, tx));
    rx
}

#[derive(Debug)]
enum Step {
    Configured(client::State),
    Drawn(client::State),
}

/// Asked to be maximized before its first commit (Chromium starting
/// maximized): configured maximized at the work area's size, and its
/// geometry lands exactly on the work area, even with centering on.
#[test]
fn maximized_before_the_first_commit() {
    let (mut event_loop, mut state) = start();
    state.wm_settings.center_new_windows = true;
    state.wm_settings.auto_maximize = false;
    let rx = connect(&mut state, |stream, tx| {
        let mut app = App::new(stream);
        app.xdg.set_window_geometry(INSET.0, INSET.1, 800, 600);
        app.toplevel.set_maximized();
        app.surface.commit();
        app.roundtrip();
        tx.send(Step::Configured(app.state.clone())).unwrap();
        app.draw((800, 600));
        tx.send(Step::Drawn(app.state.clone())).unwrap();
        std::thread::sleep(Duration::from_secs(1));
    });
    let Step::Configured(configured) = serve(&mut event_loop, &mut state, &rx) else { panic!() };
    assert!(configured.maximized, "{configured:?}");
    assert_eq!(configured.size, SIZE);
    let Step::Drawn(_) = serve(&mut event_loop, &mut state, &rx) else { panic!() };
    let (area, inset, maximized) = window(&state);
    assert!(maximized);
    assert_eq!(area, work_area());
    assert_eq!(inset, Point::from((0, 0)));
}

/// A window whose geometry has a shadow margin and asks to be maximized
/// before its first commit, but draws its first frame before handling the
/// configure (with the margin): still maximized, and once it answers, its
/// geometry is exactly on the work area.
#[test]
fn maximized_before_mapping_with_a_late_answer() {
    let (mut event_loop, mut state) = start();
    state.wm_settings.center_new_windows = true;
    state.wm_settings.auto_maximize = false;
    let rx = connect(&mut state, |stream, tx| {
        let mut app = App::new(stream);
        app.toplevel.set_maximized();
        app.surface.commit();
        app.roundtrip();
        let configured = app.state.clone();
        // A first frame at the size it had in mind, with the margin.
        let serial = app.state.serial.take().unwrap();
        app.xdg.ack_configure(serial);
        app.xdg.set_window_geometry(INSET.0, INSET.1, 800, 600);
        app.surface.attach(Some(&app.buffer(800 + 2 * INSET.0, 600 + 2 * INSET.1)), 0, 0);
        app.surface.commit();
        app.roundtrip();
        tx.send(Step::Configured(configured)).unwrap();
        // Then the maximized frame.
        app.state.serial = Some(serial);
        app.state.maximized = true;
        app.state.size = SIZE;
        app.xdg.set_window_geometry(0, 0, SIZE.0, SIZE.1);
        app.surface.attach(Some(&app.buffer(SIZE.0, SIZE.1)), 0, 0);
        app.surface.commit();
        app.roundtrip();
        tx.send(Step::Drawn(app.state.clone())).unwrap();
        std::thread::sleep(Duration::from_secs(1));
    });
    let Step::Configured(configured) = serve(&mut event_loop, &mut state, &rx) else { panic!() };
    assert!(configured.maximized);
    let Step::Drawn(_) = serve(&mut event_loop, &mut state, &rx) else { panic!() };
    let (area, _, maximized) = window(&state);
    assert!(maximized, "still maximized after a frame with the margin");
    assert_eq!(area, work_area(), "not moved by new-window placement");
}

/// A window restoring a size that fills the work area (with its margin
/// outside it), without asking to be maximized: maximized for it (mutter's
/// `auto-maximize`), so it drops the margin and sits on the work area.
#[test]
fn filling_the_work_area_maximizes() {
    let (mut event_loop, mut state) = start();
    state.wm_settings.center_new_windows = false;
    state.wm_settings.auto_maximize = true;
    let rx = connect(&mut state, |stream, tx| {
        let mut app = App::new(stream);
        app.surface.commit();
        app.roundtrip();
        app.draw(SIZE);
        tx.send(Step::Configured(app.state.clone())).unwrap();
        app.draw(SIZE);
        tx.send(Step::Drawn(app.state.clone())).unwrap();
        std::thread::sleep(Duration::from_secs(1));
    });
    let Step::Configured(after_first) = serve(&mut event_loop, &mut state, &rx) else { panic!() };
    assert!(after_first.maximized, "{after_first:?}");
    let Step::Drawn(_) = serve(&mut event_loop, &mut state, &rx) else { panic!() };
    let (area, inset, maximized) = window(&state);
    assert!(maximized);
    assert_eq!(area, work_area());
    assert_eq!(inset, Point::from((0, 0)));
}

/// Maximizing a mapped window puts its geometry on the work area.
#[test]
fn maximized_after_mapping() {
    let (mut event_loop, mut state) = start();
    state.wm_settings.center_new_windows = false;
    state.wm_settings.auto_maximize = false;
    let rx = connect(&mut state, |stream, tx| {
        let mut app = App::new(stream);
        app.surface.commit();
        app.roundtrip();
        app.draw((640, 480));
        tx.send(Step::Configured(app.state.clone())).unwrap();
        app.toplevel.set_maximized();
        app.roundtrip();
        app.draw((640, 480));
        tx.send(Step::Drawn(app.state.clone())).unwrap();
        std::thread::sleep(Duration::from_secs(1));
    });
    let Step::Configured(_) = serve(&mut event_loop, &mut state, &rx) else { panic!() };
    let (area, inset, maximized) = window(&state);
    assert!(!maximized);
    assert_eq!(inset, Point::from(INSET));
    assert_eq!(area.size, Size::from((640, 480)));
    let Step::Drawn(drawn) = serve(&mut event_loop, &mut state, &rx) else { panic!() };
    assert!(drawn.maximized);
    let (area, inset, maximized) = window(&state);
    assert!(maximized);
    assert_eq!(area, work_area());
    assert_eq!(inset, Point::from((0, 0)));
}
