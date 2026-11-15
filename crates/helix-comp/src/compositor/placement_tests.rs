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

/// The tests' runtime dir (for the compositor's listening sockets),
/// shared by the tests running at once.
fn runtime_dir_path() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("helix-comp-placement-{}", std::process::id()))
}

/// Remove the runtime dir when the test process exits, whether the tests
/// passed or not (the harness exits through `exit`, which runs this).
extern "C" fn remove_runtime_dir() {
    let _ = std::fs::remove_dir_all(runtime_dir_path());
}

fn runtime_dir() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let dir = runtime_dir_path();
        let _ = std::fs::create_dir_all(&dir);
        // SAFETY: registering a plain function with no captured state.
        unsafe { libc::atexit(remove_runtime_dir) };
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

    use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

    #[derive(Default, Debug, Clone)]
    pub struct State {
        /// The last configure: size, maximized, and its serial.
        pub size: (i32, i32),
        pub maximized: bool,
        pub fullscreen: bool,
        pub serial: Option<u32>,
        /// How many `xdg_surface.configure` events came.
        pub configures: u32,
        /// The layer surface's last configure serial.
        pub layer_serial: Option<u32>,
    }

    impl Dispatch<zwlr_layer_shell_v1::ZwlrLayerShellV1, ()> for State {
        fn event(_: &mut Self, _: &zwlr_layer_shell_v1::ZwlrLayerShellV1, _: zwlr_layer_shell_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
    }
    impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for State {
        fn event(state: &mut Self, _: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, event: zwlr_layer_surface_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
            if let zwlr_layer_surface_v1::Event::Configure { serial, .. } = event {
                state.layer_serial = Some(serial);
            }
        }
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
                state.configures += 1;
            }
        }
    }
    impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
        fn event(state: &mut Self, _: &xdg_toplevel::XdgToplevel, event: xdg_toplevel::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
            if let xdg_toplevel::Event::Configure { width, height, states } = event {
                state.size = (width, height);
                let has = |which: xdg_toplevel::State| states.as_chunks::<4>().0.iter().map(|c| u32::from_ne_bytes(*c)).any(|s| xdg_toplevel::State::try_from(s) == Ok(which));
                state.maximized = has(xdg_toplevel::State::Maximized);
                state.fullscreen = has(xdg_toplevel::State::Fullscreen);
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
    globals: wayland_client::globals::GlobalList,
    compositor: wayland_client::protocol::wl_compositor::WlCompositor,
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
        Self { queue, state: client::State::default(), qh, shm, surface, xdg, toplevel, globals, compositor }
    }

    fn roundtrip(&mut self) {
        self.queue.roundtrip(&mut self.state).unwrap();
    }

    /// A `w`×`h` buffer (the margin is transparent).
    fn buffer(&self, w: i32, h: i32) -> wl_buffer::WlBuffer {
        self.filled(w, h, 0)
    }

    /// A `w`×`h` buffer of one ARGB color.
    fn filled(&self, w: i32, h: i32, argb: u32) -> wl_buffer::WlBuffer {
        let bytes: Vec<u8> = std::iter::repeat_n(argb.to_le_bytes(), (w * h) as usize).flatten().collect();
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

/// xdg-shell ordering: nothing is configured before the client's initial
/// commit (not even the focus a new window gets), and the first configure
/// answers that commit with what the client asked for before it.
#[test]
fn first_configure_follows_the_initial_commit() {
    for fullscreen in [false, true] {
        let (mut event_loop, mut state) = start();
        state.wm_settings.center_new_windows = false;
        state.wm_settings.auto_maximize = false;
        let rx = connect(&mut state, move |stream, tx| {
            let mut app = App::new(stream);
            app.roundtrip();
            tx.send(Step::Configured(app.state.clone())).unwrap();
            if fullscreen {
                app.toplevel.set_fullscreen(None);
            } else {
                app.toplevel.set_maximized();
            }
            app.roundtrip();
            tx.send(Step::Configured(app.state.clone())).unwrap();
            app.surface.commit();
            app.roundtrip();
            tx.send(Step::Drawn(app.state.clone())).unwrap();
            std::thread::sleep(Duration::from_secs(1));
        });
        let Step::Configured(created) = serve(&mut event_loop, &mut state, &rx) else { panic!() };
        assert_eq!(created.configures, 0, "no configure for a new toplevel before its initial commit");
        assert_eq!(state.focused_surface(), None, "not focused before its initial commit");
        let Step::Configured(asked) = serve(&mut event_loop, &mut state, &rx) else { panic!() };
        assert_eq!(asked.configures, 0, "asking for a state before the initial commit is not answered yet");
        let Step::Drawn(first) = serve(&mut event_loop, &mut state, &rx) else { panic!() };
        assert_eq!(first.configures, 1, "{first:?}");
        assert_eq!((first.maximized, first.fullscreen), (!fullscreen, fullscreen), "{first:?}");
        assert_eq!(first.size, SIZE, "the monitor's (work area's) size");
        let window = state.space.elements().next().unwrap().clone();
        assert_eq!(state.focused_surface().as_ref(), window.toplevel().map(|t| t.wl_surface()), "focused at its initial commit");
    }
}

/// The panel (a top layer surface) has the keyboard exclusively, as after
/// `switch-panels`; a click on a window gives that window the keyboard,
/// and the panel's next redraws don't take it back until it asks anew.
#[test]
fn clicking_a_window_takes_the_keyboard_from_the_panel() {
    use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};
    let (mut event_loop, mut state) = start();
    state.wm_settings.center_new_windows = true;
    state.wm_settings.auto_maximize = false;
    let (go, wait) = mpsc::channel::<()>();
    let rx = connect(&mut state, move |stream, tx| {
        let mut app = App::new(stream);
        app.surface.commit();
        app.roundtrip();
        app.draw((640, 480));
        tx.send(Step::Drawn(app.state.clone())).unwrap();
        // The panel, asking for the keyboard exclusively.
        let shell: zwlr_layer_shell_v1::ZwlrLayerShellV1 = app.globals.bind(&app.qh, 1..=4, ()).unwrap();
        let surface = app.compositor.create_surface(&app.qh, ());
        let panel = shell.get_layer_surface(&surface, None, zwlr_layer_shell_v1::Layer::Top, "helix-panel".into(), &app.qh, ());
        panel.set_size(0, 32);
        panel.set_anchor(zwlr_layer_surface_v1::Anchor::Top | zwlr_layer_surface_v1::Anchor::Left | zwlr_layer_surface_v1::Anchor::Right);
        panel.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::Exclusive);
        surface.commit();
        app.roundtrip();
        let redraw = |app: &mut App| {
            if let Some(serial) = app.state.layer_serial.take() {
                panel.ack_configure(serial);
            }
            surface.attach(Some(&app.buffer(SIZE.0, 32)), 0, 0);
            surface.commit();
            app.roundtrip();
        };
        redraw(&mut app);
        tx.send(Step::Drawn(app.state.clone())).unwrap();
        // After the click: the panel redraws (a clock tick).
        wait.recv().unwrap();
        redraw(&mut app);
        redraw(&mut app);
        tx.send(Step::Drawn(app.state.clone())).unwrap();
        // It gives up the keyboard, then asks again (switch-panels).
        wait.recv().unwrap();
        panel.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
        redraw(&mut app);
        panel.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::Exclusive);
        redraw(&mut app);
        tx.send(Step::Drawn(app.state.clone())).unwrap();
        std::thread::sleep(Duration::from_secs(1));
    });
    let Step::Drawn(_) = serve(&mut event_loop, &mut state, &rx) else { panic!() };
    let window = state.space.elements().next().unwrap().clone();
    let window_surface = window.toplevel().unwrap().wl_surface().clone();
    assert_eq!(state.focused_surface().as_ref(), Some(&window_surface));
    let Step::Drawn(_) = serve(&mut event_loop, &mut state, &rx) else { panic!() };
    let output = state.space.outputs().next().unwrap().clone();
    let panel_surface = smithay::desktop::layer_map_for_output(&output).layers().next().unwrap().wl_surface().clone();
    assert_eq!(state.focused_surface().as_ref(), Some(&panel_surface), "the panel took the keyboard");

    // A click in the middle of the window.
    let area = state.space.element_geometry(&window).unwrap();
    let at = Point::<f64, Logical>::from((f64::from(area.loc.x + area.size.w / 2), f64::from(area.loc.y + area.size.h / 2)));
    assert!(!state.over_panel(at));
    state.pointer_to(at, 1);
    state.button_input(super::input::BTN_LEFT, smithay::backend::input::ButtonState::Pressed, 2);
    state.button_input(super::input::BTN_LEFT, smithay::backend::input::ButtonState::Released, 3);
    assert_eq!(state.focused_surface().as_ref(), Some(&window_surface), "the click focused the window");

    go.send(()).unwrap();
    let Step::Drawn(_) = serve(&mut event_loop, &mut state, &rx) else { panic!() };
    assert_eq!(state.focused_surface().as_ref(), Some(&window_surface), "the panel's redraws don't take it back");

    go.send(()).unwrap();
    let Step::Drawn(_) = serve(&mut event_loop, &mut state, &rx) else { panic!() };
    assert_eq!(state.focused_surface().as_ref(), Some(&panel_surface), "asking anew takes it");
}

/// A full-screen window hides the panel and other top-layer shell
/// surfaces on its monitor (drawn and hit alike), except the on-screen
/// keyboard; overlay surfaces (an OSD) stay above it. Leaving full screen
/// brings the panel back.
#[test]
fn fullscreen_hides_the_top_layer() {
    use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};
    const WINDOW: u32 = 0xFFFF_FFFF;
    const PANEL: u32 = 0xFFFF_0000;
    const OSK: u32 = 0xFF00_FF00;
    const OSD: u32 = 0xFF00_00FF;
    let (mut event_loop, mut state) = start();
    state.wm_settings.center_new_windows = true;
    state.wm_settings.auto_maximize = false;
    let (go, wait) = mpsc::channel::<()>();
    let rx = connect(&mut state, move |stream, tx| {
        use zwlr_layer_surface_v1::Anchor;
        let mut app = App::new(stream);
        app.surface.commit();
        app.roundtrip();
        let frame = |app: &mut App, size: (i32, i32)| {
            if let Some(serial) = app.state.serial.take() {
                app.xdg.ack_configure(serial);
            }
            let (w, h) = if app.state.size.0 > 0 { app.state.size } else { size };
            app.xdg.set_window_geometry(0, 0, w, h);
            app.surface.attach(Some(&app.filled(w, h, WINDOW)), 0, 0);
            app.surface.commit();
            app.roundtrip();
        };
        frame(&mut app, (640, 480));
        let shell: zwlr_layer_shell_v1::ZwlrLayerShellV1 = app.globals.bind(&app.qh, 1..=4, ()).unwrap();
        let layers = [
            (zwlr_layer_shell_v1::Layer::Top, "helix-panel", Anchor::Top | Anchor::Left | Anchor::Right, (0, 32), (SIZE.0, 32), PANEL),
            (zwlr_layer_shell_v1::Layer::Top, "helix-osk", Anchor::Bottom | Anchor::Left | Anchor::Right, (0, 100), (SIZE.0, 100), OSK),
            (zwlr_layer_shell_v1::Layer::Overlay, "helix-osd", Anchor::empty(), (100, 100), (100, 100), OSD),
        ];
        let mut surfaces = Vec::new();
        for (layer, namespace, anchor, size, buffer, color) in layers {
            let surface = app.compositor.create_surface(&app.qh, ());
            let layer = shell.get_layer_surface(&surface, None, layer, namespace.into(), &app.qh, ());
            layer.set_size(size.0 as u32, size.1 as u32);
            layer.set_anchor(anchor);
            surface.commit();
            app.roundtrip();
            layer.ack_configure(app.state.layer_serial.take().expect("a layer configure"));
            surface.attach(Some(&app.filled(buffer.0, buffer.1, color)), 0, 0);
            surface.commit();
            app.roundtrip();
            surfaces.push((surface, layer));
        }
        tx.send(Step::Drawn(app.state.clone())).unwrap();
        wait.recv().unwrap();
        app.toplevel.set_fullscreen(None);
        app.roundtrip();
        frame(&mut app, SIZE);
        tx.send(Step::Drawn(app.state.clone())).unwrap();
        wait.recv().unwrap();
        app.toplevel.unset_fullscreen();
        app.roundtrip();
        frame(&mut app, (640, 480));
        tx.send(Step::Drawn(app.state.clone())).unwrap();
        std::thread::sleep(Duration::from_secs(1));
    });
    let Step::Drawn(_) = serve(&mut event_loop, &mut state, &rx) else { panic!() };
    let output = state.space.outputs().next().unwrap().clone();
    let window = state.space.elements().next().unwrap().clone();
    let window_surface = window.toplevel().unwrap().wl_surface().clone();
    // Output-local points: on the panel, on the keyboard, on the OSD.
    let (panel_at, osk_at, osd_at) = ((SIZE.0 / 2, 10), (SIZE.0 / 2, SIZE.1 - 10), (SIZE.0 / 2, SIZE.1 / 2));
    let global = |(x, y): (i32, i32)| Point::<f64, Logical>::from((f64::from(ORIGIN.0 + x), f64::from(ORIGIN.1 + y)));
    let shown = |state: &Helix| {
        let image = state.headless_image(&output).expect("a snapshot");
        (image.at(panel_at.0, panel_at.1), image.at(osk_at.0, osk_at.1), image.at(osd_at.0, osd_at.1))
    };
    assert!(!state.in_fullscreen(&output));
    assert_eq!(shown(&state), (PANEL, OSK, OSD));
    assert!(state.over_panel(global(panel_at)));

    go.send(()).unwrap();
    let Step::Drawn(drawn) = serve(&mut event_loop, &mut state, &rx) else { panic!() };
    assert!(drawn.fullscreen, "{drawn:?}");
    assert!(state.in_fullscreen(&output));
    assert_eq!(shown(&state), (WINDOW, OSK, OSD), "the panel is hidden; the keyboard and the OSD stay");
    assert!(!state.over_panel(global(panel_at)), "the hidden panel takes no input");
    assert_eq!(state.surface_under(global(panel_at)).map(|(s, _)| s), Some(window_surface.clone()));
    assert!(state.over_panel(global(osk_at)));

    go.send(()).unwrap();
    let Step::Drawn(drawn) = serve(&mut event_loop, &mut state, &rx) else { panic!() };
    assert!(!drawn.fullscreen, "{drawn:?}");
    assert!(!state.in_fullscreen(&output));
    assert_eq!(shown(&state).0, PANEL, "the panel is back");
}
